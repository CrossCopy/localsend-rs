use crate::error::Result;
use async_trait::async_trait;
use std::path::Path;

/// File system abstraction for testability and flexibility
#[async_trait]
pub trait FileSystem: Send + Sync {
    /// Read entire file contents
    async fn read(&self, path: &Path) -> Result<Vec<u8>>;

    /// Write data to a file
    async fn write(&self, path: &Path, data: &[u8]) -> Result<()>;

    /// Check if a file or directory exists
    async fn exists(&self, path: &Path) -> bool;

    /// Get file metadata (size, modified time, etc.)
    async fn metadata(&self, path: &Path) -> Result<std::fs::Metadata>;

    /// Create directory and all parent directories
    async fn create_dir_all(&self, path: &Path) -> Result<()>;

    /// Delete a file
    async fn remove_file(&self, path: &Path) -> Result<()>;
}
