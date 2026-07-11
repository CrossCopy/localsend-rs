use crate::protocol::{DeviceInfo, FileId, FileMetadata, ReceivedFile, SessionId};
use axum::body::Body;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Notify, RwLock, oneshot};

pub type ProgressCallback = Box<dyn Fn(String, u64, u64, f64) + Send + Sync>;

pub struct PendingTransfer {
    pub sender: DeviceInfo,
    pub files: HashMap<FileId, FileMetadata>,
    pub response_tx: oneshot::Sender<bool>,
}

pub struct ActiveSession {
    pub session_id: SessionId,
    pub files: HashMap<FileId, FileMetadata>,
    pub sender_alias: String,
    pub last_activity: std::time::Instant,
}

pub struct ServerState {
    pub device: DeviceInfo,
    pub current_session: Option<ActiveSession>,
    pub save_dir: PathBuf,
    pub _progress_callback: Option<ProgressCallback>,
    pub pending_transfer: Arc<RwLock<Option<PendingTransfer>>>,
    pub pending_transfer_notify: Option<Arc<Notify>>,
    pub received_files: Arc<RwLock<Vec<ReceivedFile>>>,
}

pub(crate) async fn write_body_to_file(body: Body, path: &Path) -> std::io::Result<u64> {
    let mut file = tokio::fs::File::create(path).await?;
    let mut bytes_written = 0u64;
    let mut stream = body.into_data_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| std::io::Error::other(e.to_string()))?;
        bytes_written += chunk.len() as u64;
        file.write_all(&chunk).await?;
    }

    file.flush().await?;
    Ok(bytes_written)
}

pub(crate) async fn publish_pending_transfer(
    pending_transfer: &Arc<RwLock<Option<PendingTransfer>>>,
    pending_transfer_notify: Option<&Arc<Notify>>,
    pending: PendingTransfer,
) {
    {
        let mut pending_guard = pending_transfer.write().await;
        *pending_guard = Some(pending);
    }

    if let Some(notify) = pending_transfer_notify {
        notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::{PendingTransfer, publish_pending_transfer, write_body_to_file};
    use axum::body::Body;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::{RwLock, oneshot};

    #[tokio::test]
    async fn write_body_to_file_writes_stream_and_returns_size() {
        let path = std::env::temp_dir().join(format!(
            "localsend-stream-upload-{}.bin",
            uuid::Uuid::new_v4()
        ));
        let body = Body::from("streamed upload content");

        let bytes_written = write_body_to_file(body, &path)
            .await
            .expect("body should stream to file");

        assert_eq!(bytes_written, 23);
        assert_eq!(
            tokio::fs::read(&path).await.expect("file should exist"),
            b"streamed upload content"
        );

        let _ = tokio::fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn publish_pending_transfer_sets_pending_and_notifies_listener() {
        let pending_transfer = Arc::new(RwLock::new(None));
        let notify = Arc::new(tokio::sync::Notify::new());
        let (response_tx, _response_rx) = oneshot::channel();
        let pending = PendingTransfer {
            sender: crate::DeviceInfo::new("sender".to_string(), 53317, crate::Protocol::Http),
            files: HashMap::new(),
            response_tx,
        };

        publish_pending_transfer(&pending_transfer, Some(&notify), pending).await;

        tokio::time::timeout(std::time::Duration::from_millis(100), notify.notified())
            .await
            .expect("pending transfer notification should wake listener");
        assert!(pending_transfer.read().await.is_some());
    }
}
