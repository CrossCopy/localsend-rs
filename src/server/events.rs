//! Public event stream for library consumers (the headless accept API).

use crate::protocol::{DeviceInfo, FileId, FileMetadata, SessionId};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::oneshot;

/// Events emitted by [`crate::server::LocalSendServer`].
#[derive(Debug)]
pub enum ServerEvent {
    /// A peer registered itself with us over HTTP.
    ///
    /// This is how a healthy LocalSend client answers an announcement: it POSTs
    /// `/register` straight back to the announcer and only falls back to a
    /// multicast reply if that fails. So on a working network the *reply to our
    /// own announcement arrives here*, on the HTTP door, and never reaches
    /// multicast discovery at all.
    ///
    /// The handler used to log this and drop it, which made a registration
    /// invisible to anything tracking who is still out there — a peer answering
    /// exactly as the protocol intends looked identical to one that had gone.
    PeerRegistered(DeviceInfo),
    /// A sender wants to transfer files. Respond via the [`PendingRequest`].
    /// Dropping the request (or ignoring it past the accept timeout) declines it.
    TransferRequest(PendingRequest),
    /// A LocalSend text message accepted from its inline `preview` payload.
    /// Text is never persisted automatically; consumers may offer explicit
    /// copy/save actions appropriate to their platform.
    TextReceived {
        session_id: SessionId,
        text: String,
        sender_alias: String,
    },
    /// A browser is waiting for approval to download the active Web Share.
    WebShareRequest(PendingWebShareRequest),
    WebShareDownloadProgress {
        session_id: SessionId,
        file_id: FileId,
        bytes_sent: u64,
        total_bytes: u64,
    },
    WebShareSessionDone {
        session_id: SessionId,
    },
    /// Cumulative payload bytes written for an active receive session.
    FileReceiveProgress {
        session_id: SessionId,
        file_id: FileId,
        file_name: String,
        sender_alias: String,
        bytes_received: u64,
        total_bytes: u64,
        file_count: usize,
    },
    /// One file finished writing to disk.
    FileReceived {
        session_id: SessionId,
        file_id: FileId,
        file_name: String,
        path: PathBuf,
        size: u64,
        sender_alias: String,
        /// Retained for source compatibility. First-class text messages are
        /// emitted as [`ServerEvent::TextReceived`].
        message_text: Option<String>,
    },
    /// All accepted files of a session arrived (or the session was cancelled).
    SessionDone {
        session_id: SessionId,
    },
}

#[derive(Clone, Debug)]
pub struct PendingWebShareRequest {
    session_id: SessionId,
    ip: std::net::IpAddr,
}

impl PendingWebShareRequest {
    pub(crate) fn new(session_id: SessionId, ip: std::net::IpAddr) -> Self {
        Self { session_id, ip }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn ip(&self) -> std::net::IpAddr {
        self.ip
    }
}

/// The consumer's answer to a transfer request.
#[derive(Debug, Clone, PartialEq)]
pub enum TransferDecision {
    Accept,
    AcceptFiles(Vec<FileId>),
    Decline,
    /// The offer cannot be accepted by anybody, and saying "declined" would be
    /// a lie that costs the sender a retry.
    ///
    /// This exists because a host's admission rules can be stricter than the
    /// protocol's. A receiver may require the `sha256` LocalSend leaves
    /// optional, refuse two offered files that claim one name rather than
    /// renaming one of them, or reject a file name it cannot make safe. None of
    /// those is a person saying no, and none of them changes if the sender asks
    /// again — so the answer is **400**, not the 403 a decline gets.
    ///
    /// The reason is for this receiver's log. It is not put on the wire: a
    /// stranger on the LAN learns that its offer was malformed and nothing about
    /// what this device requires.
    Refuse {
        reason: String,
    },
}

/// Handle to answer an incoming `prepare-upload`. Consume it exactly once.
#[derive(Debug)]
pub struct PendingRequest {
    sender: DeviceInfo,
    files: HashMap<FileId, FileMetadata>,
    responder: oneshot::Sender<TransferDecision>,
}

impl PendingRequest {
    // Not yet called outside tests: handler wiring lands in Task 2.2.
    #[allow(dead_code)]
    pub(crate) fn new(
        sender: DeviceInfo,
        files: HashMap<FileId, FileMetadata>,
    ) -> (Self, oneshot::Receiver<TransferDecision>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                sender,
                files,
                responder: tx,
            },
            rx,
        )
    }

    pub fn sender(&self) -> &DeviceInfo {
        &self.sender
    }

    pub fn files(&self) -> &HashMap<FileId, FileMetadata> {
        &self.files
    }

    /// Accept every offered file. No-op if the sender already timed out.
    pub fn accept(self) {
        let _ = self.responder.send(TransferDecision::Accept);
    }

    /// Accept a subset of the offered files (empty = decline).
    pub fn accept_files(self, ids: Vec<FileId>) {
        let _ = self.responder.send(TransferDecision::AcceptFiles(ids));
    }

    pub fn decline(self) {
        let _ = self.responder.send(TransferDecision::Decline);
    }

    /// Refuse the offer as unusable. See [`TransferDecision::Refuse`] — this is
    /// not a decline, and the sender is told 400 rather than 403.
    pub fn refuse(self, reason: impl Into<String>) {
        let _ = self.responder.send(TransferDecision::Refuse {
            reason: reason.into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{DeviceInfo, Protocol};
    use std::collections::HashMap;

    fn req() -> (
        PendingRequest,
        tokio::sync::oneshot::Receiver<TransferDecision>,
    ) {
        let sender = DeviceInfo::new("s".to_string(), 53317, Protocol::Http);
        PendingRequest::new(sender, HashMap::new())
    }

    #[tokio::test]
    async fn accept_sends_accept_decision() {
        let (r, rx) = req();
        r.accept();
        assert!(matches!(rx.await, Ok(TransferDecision::Accept)));
    }

    #[tokio::test]
    async fn decline_sends_decline_decision() {
        let (r, rx) = req();
        r.decline();
        assert!(matches!(rx.await, Ok(TransferDecision::Decline)));
    }

    #[tokio::test]
    async fn dropping_request_closes_channel() {
        let (r, rx) = req();
        drop(r);
        assert!(rx.await.is_err()); // handler treats closed channel as decline
    }
}
