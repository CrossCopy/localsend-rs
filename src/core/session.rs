use crate::protocol::{FileId, FileMetadata, SessionId, Token};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReceivePhase {
    Ready,
    Receiving,
    Publishing,
    Published,
    Cancelled,
}

#[derive(Debug)]
struct ReceiveSlot {
    phase: ReceivePhase,
    cancellation: CancellationToken,
}

#[derive(Debug)]
struct ReceiveLifecycle {
    slots: HashMap<FileId, ReceiveSlot>,
}

/// Exclusive ownership of one standard LocalSend upload body.
///
/// Dropping a live lease makes an ordinary I/O/validation failure retryable.
/// Session cancellation changes the slot to `Cancelled` first, so a dropped
/// cancelled lease can never resurrect it.
#[derive(Debug)]
pub(crate) struct StandardReceiveLease {
    lifecycle: Arc<Mutex<ReceiveLifecycle>>,
    file_id: FileId,
    cancellation: CancellationToken,
    armed: bool,
}

impl StandardReceiveLease {
    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Atomically chooses publication over cancellation. Once this succeeds,
    /// session cancellation must wait for the handler to publish and record
    /// the file rather than clearing its owning session underneath it.
    pub(crate) fn begin_publication(mut self) -> Option<StandardPublicationPermit> {
        let transitioned = {
            let mut lifecycle = self.lifecycle.lock();
            let slot = lifecycle.slots.get_mut(&self.file_id)?;
            if slot.phase == ReceivePhase::Receiving {
                slot.phase = ReceivePhase::Publishing;
                true
            } else {
                false
            }
        };

        if !transitioned {
            return None;
        }

        self.armed = false;
        Some(StandardPublicationPermit {
            lifecycle: self.lifecycle.clone(),
            file_id: self.file_id.clone(),
            armed: true,
        })
    }
}

impl Drop for StandardReceiveLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut lifecycle = self.lifecycle.lock();
        if let Some(slot) = lifecycle.slots.get_mut(&self.file_id)
            && slot.phase == ReceivePhase::Receiving
        {
            slot.phase = ReceivePhase::Ready;
        }
    }
}

/// Holds the cancellation/publication linearization point until the file has
/// both landed on disk and been recorded in the owning session.
#[derive(Debug)]
pub(crate) struct StandardPublicationPermit {
    lifecycle: Arc<Mutex<ReceiveLifecycle>>,
    file_id: FileId,
    armed: bool,
}

impl StandardPublicationPermit {
    pub(crate) fn finish(mut self) {
        let mut lifecycle = self.lifecycle.lock();
        let slot = lifecycle
            .slots
            .get_mut(&self.file_id)
            .expect("publication file remains in its session lifecycle");
        debug_assert_eq!(slot.phase, ReceivePhase::Publishing);
        slot.phase = ReceivePhase::Published;
        self.armed = false;
    }
}

impl Drop for StandardPublicationPermit {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut lifecycle = self.lifecycle.lock();
        if let Some(slot) = lifecycle.slots.get_mut(&self.file_id)
            && slot.phase == ReceivePhase::Publishing
        {
            slot.phase = ReceivePhase::Ready;
        }
    }
}

/// Active file transfer session
#[derive(Clone, Debug)]
pub struct Session {
    pub id: SessionId,
    pub files: HashMap<FileId, FileMetadata>,
    pub tokens: HashMap<FileId, Token>,
    pub received: HashSet<FileId>,
    pub received_bytes: Arc<AtomicU64>,
    pub sender_alias: String,
    pub created_at: Instant,
    pub last_activity: Instant,
    receive_lifecycle: Arc<Mutex<ReceiveLifecycle>>,
}

impl Session {
    /// Create a new session
    pub fn new(sender_alias: String, files: HashMap<FileId, FileMetadata>) -> Self {
        let id = SessionId::new();
        let now = Instant::now();

        // Generate a random, per-file token -- must not be derivable from the
        // session/file ids (guessable tokens would let anyone upload).
        let tokens = files
            .keys()
            .map(|file_id| (file_id.clone(), Token::random()))
            .collect();
        let receive_lifecycle = ReceiveLifecycle {
            slots: files
                .keys()
                .map(|file_id| {
                    (
                        file_id.clone(),
                        ReceiveSlot {
                            phase: ReceivePhase::Ready,
                            cancellation: CancellationToken::new(),
                        },
                    )
                })
                .collect(),
        };

        Self {
            id,
            files,
            tokens,
            received: HashSet::new(),
            received_bytes: Arc::new(AtomicU64::new(0)),
            sender_alias,
            created_at: now,
            last_activity: now,
            receive_lifecycle: Arc::new(Mutex::new(receive_lifecycle)),
        }
    }

    /// Update the last activity timestamp
    pub fn touch(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Check if the session has timed out (default: 5 minutes)
    pub fn is_timed_out(&self, timeout: Duration) -> bool {
        self.last_activity.elapsed() > timeout
    }

    /// Verify that a token is valid for a given file
    pub fn verify_token(&self, file_id: &FileId, token: &Token) -> bool {
        self.tokens
            .get(file_id)
            .map(|expected| expected.as_str() == token.as_str())
            .unwrap_or(false)
    }

    /// Get the token for a file
    pub fn get_token(&self, file_id: &FileId) -> Option<&Token> {
        self.tokens.get(file_id)
    }

    /// Claim the one receive body allowed for a file id. Concurrent/replayed
    /// uploads for the same token cannot race two filesystem publications.
    pub(crate) fn begin_receive(&self, file_id: &FileId) -> Option<StandardReceiveLease> {
        let cancellation = {
            let mut lifecycle = self.receive_lifecycle.lock();
            let slot = lifecycle.slots.get_mut(file_id)?;
            if slot.phase != ReceivePhase::Ready {
                return None;
            }
            slot.phase = ReceivePhase::Receiving;
            slot.cancellation.clone()
        };
        Some(StandardReceiveLease {
            lifecycle: self.receive_lifecycle.clone(),
            file_id: file_id.clone(),
            cancellation,
            armed: true,
        })
    }

    /// Cancel every not-yet-published receive owned by this session.
    ///
    /// A publishing permit wins the race atomically. Callers must then leave
    /// the session installed until the handler records the committed file and
    /// releases the permit.
    pub(crate) fn try_cancel_receives(&self) -> bool {
        let mut lifecycle = self.receive_lifecycle.lock();
        if lifecycle
            .slots
            .values()
            .any(|slot| slot.phase == ReceivePhase::Publishing)
        {
            return false;
        }
        for slot in lifecycle.slots.values_mut() {
            if matches!(slot.phase, ReceivePhase::Ready | ReceivePhase::Receiving) {
                slot.phase = ReceivePhase::Cancelled;
                slot.cancellation.cancel();
            }
        }
        true
    }

    /// Record a completed file. Returns true when every file has arrived.
    ///
    /// A file id that does not belong to this session (e.g. a stale upload
    /// from a session that was cancelled/timed out and replaced mid-write)
    /// is ignored entirely -- it must never be recorded and must never
    /// count toward "all files received" for a session it doesn't belong to.
    pub fn mark_received(&mut self, file_id: &FileId) -> bool {
        if !self.files.contains_key(file_id) {
            return false;
        }
        self.received.insert(file_id.clone());
        self.last_activity = Instant::now();
        self.received.len() == self.files.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::FileMetadata;
    use std::thread;
    use std::time::Duration;

    fn create_test_files() -> HashMap<FileId, FileMetadata> {
        let mut files = HashMap::new();
        let file_id = FileId::new();
        files.insert(
            file_id.clone(),
            FileMetadata {
                id: file_id,
                file_name: "test.txt".to_string(),
                size: 1024,
                file_type: "text/plain".to_string(),
                sha256: None,
                preview: None,
                metadata: None,
            },
        );
        files
    }

    #[test]
    fn test_session_creation() {
        let files = create_test_files();
        let session = Session::new("Test Sender".to_string(), files.clone());

        assert_eq!(session.sender_alias, "Test Sender");
        assert_eq!(session.files.len(), 1);
        assert_eq!(session.tokens.len(), 1);
        assert!(!session.is_timed_out(Duration::from_secs(300)));
    }

    #[test]
    fn test_token_verification() {
        let files = create_test_files();
        let session = Session::new("Test".to_string(), files.clone());

        let file_id = files.keys().next().unwrap();
        let valid_token = session.get_token(file_id).unwrap();

        assert!(session.verify_token(file_id, valid_token));

        // Invalid token
        let invalid_token = Token::from_string("invalid".to_string());
        assert!(!session.verify_token(file_id, &invalid_token));
    }

    #[test]
    fn test_timeout() {
        let files = create_test_files();
        let mut session = Session::new("Test".to_string(), files);

        // Should not timeout immediately
        assert!(!session.is_timed_out(Duration::from_secs(1)));

        // Sleep for 2 seconds
        thread::sleep(Duration::from_secs(2));

        // Should timeout after 1 second threshold
        assert!(session.is_timed_out(Duration::from_secs(1)));

        // Touch should reset timeout
        session.touch();
        assert!(!session.is_timed_out(Duration::from_secs(1)));
    }

    #[test]
    fn tokens_are_random_not_derived() {
        let files = create_test_files();
        let s1 = Session::new("A".to_string(), files.clone());
        let s2 = Session::new("A".to_string(), files.clone());
        let id = files.keys().next().unwrap();
        // Different sessions must produce different tokens for the same file id,
        // and a token must not embed the session/file ids.
        let t1 = s1.get_token(id).unwrap().as_str().to_string();
        let t2 = s2.get_token(id).unwrap().as_str().to_string();
        assert_ne!(t1, t2);
        assert!(!t1.contains(id.as_str()));
    }

    #[test]
    fn mark_received_reports_all_done() {
        let mut files = create_test_files();
        let second = FileId::new();
        files.insert(second.clone(), files.values().next().unwrap().clone());
        let ids: Vec<FileId> = files.keys().cloned().collect();
        let mut s = Session::new("A".to_string(), files);
        assert!(!s.mark_received(&ids[0]));
        assert!(s.mark_received(&ids[1]));
    }

    #[test]
    fn mark_received_ignores_foreign_file_id() {
        let files = create_test_files();
        let mut s = Session::new("A".to_string(), files);
        let foreign = FileId::new();
        assert!(!s.mark_received(&foreign)); // foreign id must NOT complete the session
        let real = s.files.keys().next().unwrap().clone();
        assert!(s.mark_received(&real)); // the real file completes it
    }
}
