use crate::error::Result;
use crate::protocol::{FileId, FileMetadata};
use mime_guess::from_path;
use std::path::{Path, PathBuf};
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

/// Resolve a collision-free, traversal-safe save path inside `save_dir`.
/// Existing files are never overwritten: "a.txt" -> "a (1).txt" -> "a (2).txt".
pub fn unique_save_path(save_dir: &Path, file_name: &str) -> crate::Result<PathBuf> {
    let candidate = crate::path_safety::safe_join(save_dir, file_name)?;
    if !candidate.exists() {
        return Ok(candidate);
    }
    let stem = candidate
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = candidate
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let parent = candidate.parent().unwrap_or(save_dir).to_path_buf();
    for i in 1u32.. {
        let next = parent.join(format!("{stem} ({i}){ext}"));
        if !next.exists() {
            return Ok(next);
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::unique_save_path;

    #[test]
    fn unique_save_path_appends_counter_on_collision() {
        let dir = std::env::temp_dir().join(format!("lsrs-col-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = unique_save_path(&dir, "a.txt").unwrap();
        assert_eq!(first, dir.join("a.txt"));
        std::fs::write(&first, "x").unwrap();
        let second = unique_save_path(&dir, "a.txt").unwrap();
        assert_eq!(second, dir.join("a (1).txt"));
        std::fs::write(&second, "y").unwrap();
        let third = unique_save_path(&dir, "a.txt").unwrap();
        assert_eq!(third, dir.join("a (2).txt"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unique_save_path_still_rejects_traversal() {
        let dir = std::env::temp_dir();
        assert!(unique_save_path(&dir, "../evil.txt").is_err());
    }
}
