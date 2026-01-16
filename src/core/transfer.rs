use crate::error::{LocalSendError, Result};
use crate::protocol::{DeviceInfo, FileId, FileMetadata, SessionId};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// Transfer state machine for managing file transfer lifecycle
#[derive(Clone, Debug)]
pub enum TransferState {
    /// No active transfer
    Idle,
    
    /// Waiting for user to accept/reject the transfer
    WaitingForAcceptance {
        sender: DeviceInfo,
        files: HashMap<FileId, FileMetadata>,
        timeout: Instant,
    },
    
    /// Transfer is actively in progress
    Transferring {
        session_id: SessionId,
        sender: String,
        files: HashMap<FileId, FileMetadata>,
        completed: HashSet<FileId>,
    },
    
    /// Transfer completed successfully
    Completed {
        session_id: SessionId,
        total_files: usize,
    },
    
    /// Transfer was cancelled
    Cancelled {
        reason: String,
    },
}

impl TransferState {
    /// Create a new transfer awaiting acceptance
    pub fn new_pending(sender: DeviceInfo, files: HashMap<FileId, FileMetadata>) -> Self {
        Self::WaitingForAcceptance {
            sender,
            files,
            timeout: Instant::now(),
        }
    }

    /// Accept the transfer and transition to Transferring state
    pub fn accept(self, session_id: SessionId) -> Result<Self> {
        match self {
            Self::WaitingForAcceptance { sender, files, .. } => {
                Ok(Self::Transferring {
                    session_id,
                    sender: sender.alias,
                    files,
                    completed: HashSet::new(),
                })
            }
            _ => Err(LocalSendError::invalid_state(
                "Cannot accept transfer from current state",
            )),
        }
    }

    /// Reject the transfer
    pub fn reject(self, reason: impl Into<String>) -> Result<Self> {
        match self {
            Self::WaitingForAcceptance { .. } => {
                Ok(Self::Cancelled {
                    reason: reason.into(),
                })
            }
            _ => Err(LocalSendError::invalid_state(
                "Cannot reject transfer from current state",
            )),
        }
    }

    /// Mark a file as completed
    pub fn complete_file(mut self, file_id: FileId) -> Result<Self> {
        match &mut self {
            Self::Transferring {
                completed,
                files,
                session_id,
                ..
            } => {
                completed.insert(file_id);
                
                // Check if all files are completed
                if completed.len() == files.len() {
                    return Ok(Self::Completed {
                        session_id: session_id.clone(),
                        total_files: files.len(),
                    });
                }
                
                Ok(self)
            }
            _ => Err(LocalSendError::invalid_state(
                "Cannot complete file from current state",
            )),
        }
    }

    /// Cancel the transfer
    pub fn cancel(self, reason: impl Into<String>) -> Self {
        Self::Cancelled {
            reason: reason.into(),
        }
    }

    /// Check if the transfer is active
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::WaitingForAcceptance { .. } | Self::Transferring { .. }
        )
    }

    /// Check if the transfer is completed
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }

    /// Get the session ID if in Transferring or Completed state
    pub fn session_id(&self) -> Option<&SessionId> {
        match self {
            Self::Transferring { session_id, .. } | Self::Completed { session_id, .. } => {
                Some(session_id)
            }
            _ => None,
        }
    }

    /// Check if a specific file has been completed
    pub fn is_file_completed(&self, file_id: &FileId) -> bool {
        match self {
            Self::Transferring { completed, .. } => completed.contains(file_id),
            Self::Completed { .. } => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{DeviceType, Protocol};

    fn create_test_device() -> DeviceInfo {
        DeviceInfo {
            alias: "Test Device".to_string(),
            version: "2.1".to_string(),
            device_model: None,
            device_type: Some(DeviceType::Desktop),
            fingerprint: "test123".to_string(),
            port: 53317,
            protocol: Protocol::Https,
            download: false,
            ip: Some("192.168.1.100".to_string()),
        }
    }

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
    fn test_transfer_state_lifecycle() {
        let device = create_test_device();
        let files = create_test_files();

        // Initial state
        let state = TransferState::new_pending(device, files.clone());
        assert!(state.is_active());
        assert!(!state.is_completed());

        // Accept transfer
        let session_id = SessionId::new();
        let state = state.accept(session_id.clone()).unwrap();
        assert!(state.is_active());
        assert_eq!(state.session_id(), Some(&session_id));

        // Complete file
        let file_id = files.keys().next().unwrap().clone();
        let state = state.complete_file(file_id).unwrap();
        assert!(state.is_completed());
    }

    #[test]
    fn test_transfer_rejection() {
        let device = create_test_device();
        let files = create_test_files();

        let state = TransferState::new_pending(device, files);
        let state = state.reject("User declined").unwrap();

        assert!(!state.is_active());
        assert!(!state.is_completed());
    }

    #[test]
    fn test_invalid_state_transitions() {
        // Cannot accept from Idle
        let result = TransferState::Idle.accept(SessionId::new());
        assert!(result.is_err());

        // Cannot complete file from WaitingForAcceptance
        let device = create_test_device();
        let files = create_test_files();
        let state = TransferState::new_pending(device, files.clone());
        let file_id = files.keys().next().unwrap().clone();
        let result = state.complete_file(file_id);
        assert!(result.is_err());
    }
}
