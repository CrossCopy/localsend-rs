use crate::protocol::{DeviceInfo, FileId, FileMetadata, ReceivedFile, SessionId};
use axum::body::Body;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;

pub type ProgressCallback = Box<dyn Fn(String, u64, u64, f64) + Send + Sync>;

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
    pub received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    pub events_tx: tokio::sync::mpsc::Sender<crate::server::events::ServerEvent>,
    pub auto_accept: bool,
    pub accept_timeout: std::time::Duration,
    pub pin_gate: crate::server::pin::PinGate,
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

#[cfg(test)]
mod tests {
    use super::write_body_to_file;
    use axum::body::Body;

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
}
