use crate::protocol::DeviceInfo;
use crate::protocol::{FileId, SessionId, Token};
use crate::server::crosscopy_authorized::{
    CrossCopyAuthorizedPrepareMetadata, CrossCopyAuthorizedUploadOwner,
};
use axum::body::Body;
use futures_util::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

pub struct ServerState {
    pub device: DeviceInfo,
    pub current_session: Option<crate::core::Session>,
    pub save_dir: PathBuf,
    pub events_tx: tokio::sync::mpsc::Sender<crate::server::events::ServerEvent>,
    /// Shared with [`crate::server::LocalSendServer`] so a live
    /// `set_auto_accept` toggle is observed by the request handler.
    pub auto_accept: Arc<AtomicBool>,
    pub accept_timeout: std::time::Duration,
    pub receive_rate_limit_bytes_per_second: Option<u64>,
    pub pin_gate: crate::server::pin::PinGate,
    pub web_share: Option<crate::server::web_share::WebShareState>,
    /// Optional host-owned gate for File-v3 protected prepares.  `None` is the
    /// production-compatible default and makes the reserved header fail closed.
    pub crosscopy_authorized_upload_gate:
        Option<std::sync::Arc<dyn crate::server::CrossCopyAuthorizedUploadGate>>,
    /// A protected session is deliberately separate from `current_session` so
    /// standard and CrossCopy-issued LocalSend tokens can never be confused.
    pub crosscopy_authorized_session: Option<CrossCopyAuthorizedSession>,
    /// An upload whose owner was moved into `receive`. It remains cancellable
    /// by exact session id until the body future returns.
    pub crosscopy_authorized_active_upload: Option<CrossCopyAuthorizedActiveUpload>,
    /// Once orderly shutdown begins, protected admission is closed before any
    /// in-flight owner is terminalized. Standard LocalSend shutdown behavior
    /// remains unchanged.
    pub crosscopy_authorized_stopping: bool,
}

pub(crate) struct CrossCopyAuthorizedActiveUpload {
    pub(crate) session_id: SessionId,
    pub(crate) cancellation: CancellationToken,
}

pub(crate) struct CrossCopyAuthorizedSession {
    pub(crate) session_id: SessionId,
    pub(crate) file_id: FileId,
    pub(crate) upload_token: Token,
    pub(crate) metadata: CrossCopyAuthorizedPrepareMetadata,
    pub(crate) owner: Box<dyn CrossCopyAuthorizedUploadOwner>,
    pub(crate) created_at: std::time::Instant,
}

impl CrossCopyAuthorizedSession {
    pub(crate) fn new(
        metadata: CrossCopyAuthorizedPrepareMetadata,
        owner: Box<dyn CrossCopyAuthorizedUploadOwner>,
    ) -> Self {
        Self {
            file_id: metadata.file_id().clone(),
            metadata,
            session_id: SessionId::new(),
            upload_token: Token::random(),
            owner,
            created_at: std::time::Instant::now(),
        }
    }

    pub(crate) fn is_timed_out(&self, seconds: u64) -> bool {
        self.created_at.elapsed().as_secs() >= seconds
    }
}

pub(crate) async fn write_body_to_file_with_progress<F>(
    body: Body,
    path: &Path,
    rate_limit_bytes_per_second: Option<u64>,
    mut progress: F,
) -> std::io::Result<u64>
where
    F: FnMut(u64),
{
    let mut file = tokio::fs::File::create(path).await?;
    let mut bytes_written = 0u64;
    let mut stream = body.into_data_stream();
    let started_at = tokio::time::Instant::now();
    let rate_limit_bytes_per_second = rate_limit_bytes_per_second.filter(|rate| *rate > 0);

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| std::io::Error::other(e.to_string()))?;
        bytes_written += chunk.len() as u64;
        file.write_all(&chunk).await?;
        if let Some(rate) = rate_limit_bytes_per_second {
            let target = std::time::Duration::from_secs_f64(bytes_written as f64 / rate as f64);
            let delay = target.saturating_sub(started_at.elapsed());
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        }
        progress(bytes_written);
    }

    file.flush().await?;
    Ok(bytes_written)
}

#[cfg(test)]
mod tests {
    use super::write_body_to_file_with_progress;
    use axum::body::{Body, Bytes};
    use futures_util::stream;
    use std::convert::Infallible;

    #[tokio::test]
    async fn write_body_to_file_writes_stream_and_returns_size() {
        let path = std::env::temp_dir().join(format!(
            "localsend-stream-upload-{}.bin",
            uuid::Uuid::new_v4()
        ));
        let body = Body::from("streamed upload content");

        let bytes_written = write_body_to_file_with_progress(body, &path, None, |_| {})
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
    async fn write_body_to_file_reports_cumulative_bytes_for_each_chunk() {
        let path = std::env::temp_dir().join(format!(
            "localsend-progress-upload-{}.bin",
            uuid::Uuid::new_v4()
        ));
        let chunks = stream::iter([
            Ok::<_, Infallible>(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"de")),
            Ok(Bytes::from_static(b"fghi")),
        ]);
        let body = Body::from_stream(chunks);
        let mut samples = Vec::new();

        let bytes_written = write_body_to_file_with_progress(body, &path, None, |cumulative| {
            samples.push(cumulative);
        })
        .await
        .expect("body should stream with progress");

        assert_eq!(samples, vec![3, 5, 9]);
        assert_eq!(bytes_written, 9);
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"abcdefghi");

        let _ = tokio::fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn write_body_to_file_can_throttle_real_stream_consumption() {
        let path = std::env::temp_dir().join(format!(
            "localsend-throttled-upload-{}.bin",
            uuid::Uuid::new_v4()
        ));
        let body = Body::from(vec![0_u8; 4_096]);
        let started_at = tokio::time::Instant::now();

        let bytes_written = write_body_to_file_with_progress(body, &path, Some(8_192), |_| {})
            .await
            .expect("throttled body should stream to file");

        assert_eq!(bytes_written, 4_096);
        assert!(started_at.elapsed() >= std::time::Duration::from_millis(450));
        assert_eq!(tokio::fs::metadata(&path).await.unwrap().len(), 4_096);

        let _ = tokio::fs::remove_file(path).await;
    }
}
