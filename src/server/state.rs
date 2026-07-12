use crate::protocol::DeviceInfo;
use axum::body::Body;
use futures_util::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::io::AsyncWriteExt;

pub type ProgressCallback = Box<dyn Fn(String, u64, u64, f64) + Send + Sync>;

pub struct ServerState {
    pub device: DeviceInfo,
    pub current_session: Option<crate::core::Session>,
    pub save_dir: PathBuf,
    pub _progress_callback: Option<ProgressCallback>,
    pub events_tx: tokio::sync::mpsc::Sender<crate::server::events::ServerEvent>,
    /// Shared with [`crate::server::LocalSendServer`] so a live
    /// `set_auto_accept` toggle is observed by the request handler.
    pub auto_accept: Arc<AtomicBool>,
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
