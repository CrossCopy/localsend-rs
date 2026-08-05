use crate::error::Result;
use crate::protocol::{FileId, FileMetadata};
use crosscopy_safe_fs::{CollisionPolicy, PendingReceiveFile, SafeReceiveError, SafeReceiveRoot};
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

/// Pin the receiver-selected root and atomically create one collision-renamed
/// destination beneath it. The returned display path is diagnostic only; the
/// pending file's held writer remains the filesystem authority until commit.
pub(crate) async fn create_pending_receive(
    save_dir: &Path,
    file_name: &str,
) -> std::result::Result<PendingReceiveFile, SafeReceiveError> {
    // Preserve LocalSend's existing cross-platform filename restrictions;
    // the returned PathBuf is deliberately discarded because authority comes
    // only from crosscopy-safe-fs's held directory/file handles below.
    crate::path_safety::safe_join(save_dir, file_name)
        .map_err(|_| SafeReceiveError::UnsafeRelativePath)?;
    SafeReceiveRoot::open_or_create(save_dir)
        .await?
        .create_file(file_name, CollisionPolicy::Rename)
        .await
}

#[cfg(test)]
mod tests {
    use super::create_pending_receive;

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_abort_detects_substitution_and_preserves_replacement() {
        use crosscopy_safe_fs::SafeReceiveError;
        use tokio::io::AsyncWriteExt;

        let dir = tempfile::tempdir().expect("receive root");
        let mut pending = create_pending_receive(dir.path(), "replace.txt")
            .await
            .expect("create pending receive");
        pending
            .writer()
            .write_all(b"owned partial")
            .await
            .expect("write owned partial");
        let named_path = pending.display_path().to_owned();
        let displaced_path = dir.path().join("displaced-owned.txt");
        std::fs::rename(&named_path, &displaced_path).expect("substitute pending name");
        std::fs::write(&named_path, b"replacement").expect("plant replacement");

        assert_eq!(
            pending.abort().await,
            Err(SafeReceiveError::EntrySubstituted)
        );
        assert_eq!(std::fs::read(&named_path).unwrap(), b"replacement");
        assert_eq!(std::fs::read(&displaced_path).unwrap(), b"owned partial");
    }
}
