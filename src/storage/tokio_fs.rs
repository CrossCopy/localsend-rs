use crate::error::Result;
use crate::storage::traits::FileSystem;
use async_trait::async_trait;
use std::path::Path;

/// Default file system implementation using tokio::fs
#[derive(Clone, Default)]
pub struct TokioFileSystem;

#[async_trait]
impl FileSystem for TokioFileSystem {
    async fn read(&self, path: &Path) -> Result<Vec<u8>> {
        Ok(tokio::fs::read(path).await?)
    }

    async fn write(&self, path: &Path, data: &[u8]) -> Result<()> {
        Ok(tokio::fs::write(path, data).await?)
    }

    async fn exists(&self, path: &Path) -> bool {
        tokio::fs::try_exists(path).await.unwrap_or(false)
    }

    async fn metadata(&self, path: &Path) -> Result<std::fs::Metadata> {
        Ok(tokio::fs::metadata(path).await?)
    }

    async fn create_dir_all(&self, path: &Path) -> Result<()> {
        Ok(tokio::fs::create_dir_all(path).await?)
    }

    async fn remove_file(&self, path: &Path) -> Result<()> {
        Ok(tokio::fs::remove_file(path).await?)
    }
}
