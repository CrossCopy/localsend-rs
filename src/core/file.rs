use crate::error::Result;
use crate::protocol::{FileId, FileMetadata};
use mime_guess::from_path;
use std::path::Path;
use tokio::fs;

pub fn generate_file_id() -> FileId {
    FileId::new()
}

pub fn get_mime_type(path: &Path) -> String {
    from_path(path).first_or_octet_stream().to_string()
}

pub async fn build_file_metadata(path: &Path) -> Result<FileMetadata> {
    let metadata = fs::metadata(path).await?;

    Ok(FileMetadata {
        id: generate_file_id(),
        file_name: path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("unknown"))
            .to_string_lossy()
            .to_string(),
        size: metadata.len(),
        file_type: get_mime_type(path),
        sha256: None,
        preview: None,
        metadata: None,
    })
}

pub fn build_file_metadata_from_bytes(
    id: FileId,
    file_name: String,
    file_type: String,
    bytes: Vec<u8>,
) -> FileMetadata {
    let size = bytes.len() as u64;
    FileMetadata {
        id,
        file_name,
        size,
        file_type,
        sha256: None,
        preview: None,
        metadata: None,
    }
}
