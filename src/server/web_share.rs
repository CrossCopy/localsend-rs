use crate::protocol::{DeviceInfo, FileId, FileMetadata, SessionId};
use crate::server::pin::{PinGate, PinVerdict};
use axum::Json;
use axum::body::Body;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{Stream, stream};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::RwLock;
use tokio_util::io::ReaderStream;

/// Everything the Web Share half of the protocol needs, and nothing else.
///
/// # Why this is its own state rather than a field of `ServerState`
///
/// The five handlers below read exactly four things: the device this daemon
/// announces, the event channel, the accept timeout, and the share itself. They
/// touch none of the receive half — no session store, no save directory, no
/// upload gate. Holding them in `ServerState` therefore made Web Share
/// unmountable by anything that does not use this crate's own router, which is
/// the position `xross-adapter-localsend` is in: it serves the upload half from
/// its own axum router, over its own admission policy, and had no way to serve
/// the download half at all.
///
/// So the state is split and the routes are exported ([`web_share_router`]).
/// `ServerState` holds one of these behind its own lock, and this crate's router
/// mounts the same function every other consumer does — which is what keeps the
/// two from drifting.
///
/// `device` is a copy of `ServerState::device` and that duplication is
/// deliberate rather than overlooked: the only use of it here is building the
/// `prepare-download` response, where `download` is forced true regardless. A
/// consumer that changes its announced alias mid-share would show the old one on
/// that one response; a shared lock across both halves would cost every upload a
/// contended acquire for a field that is written approximately never.
pub struct WebShareHost {
    pub device: DeviceInfo,
    pub events_tx: tokio::sync::mpsc::Sender<crate::server::events::ServerEvent>,
    pub accept_timeout: std::time::Duration,
    pub share: Option<WebShareState>,
}

impl WebShareHost {
    pub fn new(
        device: DeviceInfo,
        events_tx: tokio::sync::mpsc::Sender<crate::server::events::ServerEvent>,
        accept_timeout: std::time::Duration,
    ) -> Self {
        Self {
            device,
            events_tx,
            accept_timeout,
            share: None,
        }
    }
}

/// The Web Share routes, over a state a consumer owns.
///
/// Mounted by this crate's own router and by any other. `GET /` is the browser
/// page; the two API routes are the reverse-mode half of the protocol. All five
/// answer 403 while no share is running, which is what stops the page existing
/// on a device that never offered one.
pub fn web_share_router(host: Arc<RwLock<WebShareHost>>) -> axum::Router {
    use axum::routing::{get, post};
    axum::Router::new()
        .route(
            "/api/localsend/v2/prepare-download",
            post(handle_prepare_download),
        )
        .route("/api/localsend/v2/download", get(handle_download))
        .route("/", get(handle_web_index))
        .route("/main.js", get(handle_web_js))
        .route("/i18n.json", get(handle_web_i18n))
        .with_state(host)
}

#[derive(Clone, Debug)]
pub enum WebShareSource {
    Inline(Arc<Vec<u8>>),
    Path(PathBuf),
}

#[derive(Clone, Debug)]
pub struct WebShareFile {
    pub metadata: FileMetadata,
    pub source: WebShareSource,
}

impl WebShareFile {
    pub fn inline(name: impl Into<String>, bytes: Vec<u8>) -> Self {
        let name = name.into();
        let id = FileId::new();
        Self {
            metadata: FileMetadata {
                id: id.clone(),
                file_name: name.clone(),
                size: bytes.len() as u64,
                file_type: mime_guess::from_path(&name)
                    .first_or_octet_stream()
                    .to_string(),
                sha256: None,
                preview: None,
                metadata: None,
            },
            source: WebShareSource::Inline(Arc::new(bytes)),
        }
    }

    pub async fn path(path: PathBuf, display_name: impl Into<String>) -> crate::Result<Self> {
        let name = display_name.into();
        let metadata = tokio::fs::metadata(&path).await?;
        let id = FileId::new();
        Ok(Self {
            metadata: FileMetadata {
                id: id.clone(),
                file_name: name.clone(),
                size: metadata.len(),
                file_type: mime_guess::from_path(&name)
                    .first_or_octet_stream()
                    .to_string(),
                sha256: None,
                preview: None,
                metadata: None,
            },
            source: WebShareSource::Path(path),
        })
    }
}

#[derive(Debug)]
/// One browser talking to a running share. `pub` for the same reason
/// [`WebShareState`] is.
pub struct WebShareSession {
    pub ip: IpAddr,
    pub approved: bool,
    pub response_tx: Option<tokio::sync::oneshot::Sender<bool>>,
    pub downloaded_files: HashSet<FileId>,
}

#[derive(Debug)]
/// One running Web Share.
///
/// `pub` rather than `pub(crate)` since 2026-08-22, because [`WebShareHost`] is
/// public and a private field type inside a public struct is a wall a consumer
/// hits with no way through.
pub struct WebShareState {
    pub files: HashMap<FileId, WebShareFile>,
    pub sessions: HashMap<SessionId, WebShareSession>,
    pub auto_accept: bool,
    pub pin_gate: PinGate,
}

impl WebShareState {
    pub fn new(files: Vec<WebShareFile>, pin: Option<String>, auto_accept: bool) -> Self {
        Self {
            files: files
                .into_iter()
                .map(|file| (file.metadata.id.clone(), file))
                .collect(),
            sessions: HashMap::new(),
            auto_accept,
            pin_gate: PinGate::new(pin),
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct PrepareDownloadQuery {
    #[serde(rename = "sessionId")]
    session_id: Option<SessionId>,
    pin: Option<String>,
}

#[derive(Serialize)]
struct PrepareDownloadResponse {
    info: DeviceInfo,
    #[serde(rename = "sessionId")]
    session_id: SessionId,
    files: HashMap<FileId, FileMetadata>,
}

async fn handle_prepare_download(
    State(state_ref): State<Arc<RwLock<WebShareHost>>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<PrepareDownloadQuery>,
) -> Response {
    let mut state = state_ref.write().await;
    let device = state.device.clone();
    let events_tx = state.events_tx.clone();
    let Some(web) = state.share.as_mut() else {
        return (StatusCode::FORBIDDEN, "Web share not initialized").into_response();
    };

    if let Some(session_id) = query.session_id
        && web
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.approved && session.ip == peer.ip())
    {
        return prepare_response(device, session_id, web);
    }

    match web.pin_gate.check(query.pin.as_deref(), peer.ip()) {
        PinVerdict::Ok => {}
        PinVerdict::Unauthorized => return StatusCode::UNAUTHORIZED.into_response(),
        PinVerdict::LockedOut => return StatusCode::TOO_MANY_REQUESTS.into_response(),
    }

    let session_id = SessionId::new();
    let approved = web.auto_accept;
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    web.sessions.insert(
        session_id.clone(),
        WebShareSession {
            ip: peer.ip(),
            approved,
            response_tx: (!approved).then_some(response_tx),
            downloaded_files: HashSet::new(),
        },
    );
    if approved {
        let _ = events_tx.try_send(crate::server::events::ServerEvent::WebShareRequest(
            crate::server::events::PendingWebShareRequest::new(session_id.clone(), peer.ip()),
        ));
        return prepare_response(device, session_id, web);
    }

    let timeout = state.accept_timeout;
    drop(state);
    if events_tx
        .send(crate::server::events::ServerEvent::WebShareRequest(
            crate::server::events::PendingWebShareRequest::new(session_id.clone(), peer.ip()),
        ))
        .await
        .is_err()
    {
        return StatusCode::FORBIDDEN.into_response();
    }

    let accepted = matches!(
        tokio::time::timeout(timeout, response_rx).await,
        Ok(Ok(true))
    );
    let mut state = state_ref.write().await;
    let device = state.device.clone();
    let Some(web) = state.share.as_mut() else {
        return StatusCode::FORBIDDEN.into_response();
    };
    if !accepted {
        web.sessions.remove(&session_id);
        return (StatusCode::FORBIDDEN, "File transfer rejected").into_response();
    }
    if let Some(session) = web.sessions.get_mut(&session_id) {
        session.approved = true;
        session.response_tx = None;
    }
    prepare_response(device, session_id, web)
}

fn prepare_response(
    mut device: DeviceInfo,
    session_id: SessionId,
    web: &WebShareState,
) -> Response {
    device.download = true;
    Json(PrepareDownloadResponse {
        info: device,
        session_id,
        files: web
            .files
            .iter()
            .map(|(id, file)| (id.clone(), file.metadata.clone()))
            .collect(),
    })
    .into_response()
}

#[derive(Deserialize)]
pub(crate) struct DownloadQuery {
    #[serde(rename = "sessionId")]
    session_id: SessionId,
    #[serde(rename = "fileId")]
    file_id: FileId,
}

async fn handle_download(
    State(state_ref): State<Arc<RwLock<WebShareHost>>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<DownloadQuery>,
) -> Response {
    let (file, events_tx) = {
        let state = state_ref.read().await;
        let Some(web) = state.share.as_ref() else {
            return StatusCode::FORBIDDEN.into_response();
        };
        if !web
            .sessions
            .get(&query.session_id)
            .is_some_and(|session| session.approved && session.ip == peer.ip())
        {
            return StatusCode::FORBIDDEN.into_response();
        }
        let Some(file) = web.files.get(&query.file_id) else {
            return StatusCode::FORBIDDEN.into_response();
        };
        (file.clone(), state.events_tx.clone())
    };

    let (source, length): (
        Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>,
        u64,
    ) = match &file.source {
        WebShareSource::Inline(bytes) => (
            Box::pin(stream::once({
                let bytes = Bytes::from(bytes.as_ref().clone());
                async move { Ok(bytes) }
            })),
            bytes.len() as u64,
        ),
        WebShareSource::Path(path) => {
            let Ok(handle) = tokio::fs::File::open(path).await else {
                return StatusCode::NOT_FOUND.into_response();
            };
            let Ok(metadata) = handle.metadata().await else {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            };
            (Box::pin(ReaderStream::new(handle)), metadata.len())
        }
    };
    let body = Body::from_stream(WebShareProgressStream {
        inner: source,
        state: state_ref,
        events_tx,
        session_id: query.session_id.clone(),
        file_id: query.file_id.clone(),
        bytes_sent: 0,
        total_bytes: length,
        finished: false,
    });

    let safe_name = file.metadata.file_name.replace(['/', '\\'], "-");
    let disposition = format!("attachment; filename=\"{}\"", safe_name.replace('"', ""));
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, value);
    }
    if let Ok(value) = HeaderValue::from_str(&length.to_string()) {
        response.headers_mut().insert(header::CONTENT_LENGTH, value);
    }
    response
}

struct WebShareProgressStream {
    inner: Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>,
    state: Arc<RwLock<WebShareHost>>,
    events_tx: tokio::sync::mpsc::Sender<crate::server::events::ServerEvent>,
    session_id: SessionId,
    file_id: FileId,
    bytes_sent: u64,
    total_bytes: u64,
    finished: bool,
}

impl Stream for WebShareProgressStream {
    type Item = std::io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                self.bytes_sent = self.bytes_sent.saturating_add(bytes.len() as u64);
                let _ = self.events_tx.try_send(
                    crate::server::events::ServerEvent::WebShareDownloadProgress {
                        session_id: self.session_id.clone(),
                        file_id: self.file_id.clone(),
                        bytes_sent: self.bytes_sent,
                        total_bytes: self.total_bytes,
                    },
                );
                if self.bytes_sent >= self.total_bytes {
                    self.finish_file();
                }
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(None) if !self.finished => {
                self.finish_file();
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

impl WebShareProgressStream {
    fn finish_file(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let state = self.state.clone();
        let events_tx = self.events_tx.clone();
        let session_id = self.session_id.clone();
        let file_id = self.file_id.clone();
        tokio::spawn(async move {
            let session_done = {
                let mut state = state.write().await;
                let Some(web) = state.share.as_mut() else {
                    return;
                };
                let file_count = web.files.len();
                let Some(session) = web.sessions.get_mut(&session_id) else {
                    return;
                };
                session.downloaded_files.insert(file_id);
                session.downloaded_files.len() == file_count
            };
            if session_done {
                let _ = events_tx
                    .send(crate::server::events::ServerEvent::WebShareSessionDone { session_id })
                    .await;
            }
        });
    }
}

async fn handle_web_index(State(state_ref): State<Arc<RwLock<WebShareHost>>>) -> Response {
    if state_ref.read().await.share.is_none() {
        return (
            StatusCode::FORBIDDEN,
            include_str!("../../assets/web/error-403.html"),
        )
            .into_response();
    }
    axum::response::Html(include_str!("../../assets/web/index.html")).into_response()
}

async fn handle_web_js(State(state_ref): State<Arc<RwLock<WebShareHost>>>) -> Response {
    if state_ref.read().await.share.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../../assets/web/main.js"),
    )
        .into_response()
}

async fn handle_web_i18n(State(state_ref): State<Arc<RwLock<WebShareHost>>>) -> Response {
    if state_ref.read().await.share.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    Json(serde_json::json!({
        "waiting": "Waiting for approval…",
        "enterPin": "Enter PIN",
        "invalidPin": "Invalid PIN",
        "tooManyAttempts": "Too many attempts",
        "rejected": "Transfer rejected",
        "files": "Files",
        "fileName": "File name",
        "size": "Size"
    }))
    .into_response()
}
