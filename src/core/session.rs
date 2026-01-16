use crate::protocol::{FileId, FileMetadata, SessionId, Token};
use std::collections::HashMap;
use std::time::Instant;

/// Active file transfer session
#[derive(Clone, Debug)]
pub struct Session {
    pub id: SessionId,
    pub files: HashMap<FileId, FileMetadata>,
    pub tokens: HashMap<FileId, Token>,
    pub sender_alias: String,
    pub created_at: Instant,
    pub last_activity: Instant,
}

impl Session {
    /// Create a new session
    pub fn new(sender_alias: String, files: HashMap<FileId, FileMetadata>) -> Self {
        let id = SessionId::new();
        let now = Instant::now();

        // Generate tokens for each file
        let tokens = files
            .keys()
            .map(|file_id| (file_id.clone(), Token::new(&id, file_id)))
            .collect();

        Self {
            id,
            files,
            tokens,
            sender_alias,
            created_at: now,
            last_activity: now,
        }
    }

    /// Update the last activity timestamp
    pub fn touch(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Check if the session has timed out (default: 5 minutes)
    pub fn is_timed_out(&self, timeout_secs: u64) -> bool {
        self.last_activity.elapsed().as_secs() > timeout_secs
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

    /// Check if all files have been uploaded
    pub fn is_complete(&self, uploaded_files: &[FileId]) -> bool {
        self.files.len() == uploaded_files.len()
            && uploaded_files.iter().all(|id| self.files.contains_key(id))
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
        assert!(!session.is_timed_out(300));
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
        assert!(!session.is_timed_out(1));

        // Sleep for 2 seconds
        thread::sleep(Duration::from_secs(2));

        // Should timeout after 1 second threshold
        assert!(session.is_timed_out(1));

        // Touch should reset timeout
        session.touch();
        assert!(!session.is_timed_out(1));
    }

    #[test]
    fn test_completion_check() {
        let files = create_test_files();
        let file_ids: Vec<FileId> = files.keys().cloned().collect();
        let session = Session::new("Test".to_string(), files);

        // Not complete with empty list
        assert!(!session.is_complete(&[]));

        // Complete with all files
        assert!(session.is_complete(&file_ids));

        // Not complete with extra file
        let mut extra_ids = file_ids.clone();
        extra_ids.push(FileId::new());
        assert!(!session.is_complete(&extra_ids));
    }
}
