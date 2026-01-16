use sha2::Digest;
use crate::error::Result;

/// Compute SHA-256 hash of bytes
pub fn sha256_from_bytes(data: &[u8]) -> String {
    let hash = sha2::Sha256::digest(data);
    format!("{:x}", hash)
}

/// Compute SHA-256 hash of a file
pub async fn sha256_from_file(path: &std::path::Path) -> Result<String> {
    let contents = tokio::fs::read(path).await?;
    Ok(sha256_from_bytes(&contents))
}
