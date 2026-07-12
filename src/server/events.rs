//! Public event stream for library consumers (the headless accept API).

use crate::protocol::{DeviceInfo, FileId, FileMetadata, SessionId};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::oneshot;

/// Events emitted by [`crate::server::LocalSendServer`].
#[derive(Debug)]
pub enum ServerEvent {
    /// A sender wants to transfer files. Respond via the [`PendingRequest`].
    /// Dropping the request (or ignoring it past the accept timeout) declines it.
    TransferRequest(PendingRequest),
    /// One file finished writing to disk.
    FileReceived {
        session_id: SessionId,
        file_id: FileId,
        file_name: String,
        path: PathBuf,
        size: u64,
        sender_alias: String,
    },
    /// All accepted files of a session arrived (or the session was cancelled).
    SessionDone { session_id: SessionId },
}

/// The consumer's answer to a transfer request.
#[derive(Debug, Clone, PartialEq)]
pub enum TransferDecision {
    Accept,
    AcceptFiles(Vec<FileId>),
    Decline,
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
