use crate::protocol::{
    DeviceInfo, FileId, FileMetadata, PrepareUploadRequest, PrepareUploadResponse, Protocol,
    ReceivedFile, SessionId,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Local;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

#[cfg(feature = "https")]
use axum_server::tls_rustls::RustlsConfig;

pub type ProgressCallback = Box<dyn Fn(String, u64, u64, f64) + Send + Sync>;

pub struct PendingTransfer {
    pub sender: DeviceInfo,
    pub files: HashMap<FileId, FileMetadata>,
    pub response_tx: oneshot::Sender<bool>,
}

pub struct LocalSendServer {
    device: DeviceInfo,
    save_dir: PathBuf,
    handle: Option<JoinHandle<()>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    https: bool,
    #[cfg(feature = "https")]
    tls_cert: Option<crate::crypto::TlsCertificate>,
    pending_transfer: Arc<RwLock<Option<PendingTransfer>>>,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
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
    pub received_files: Arc<RwLock<Vec<ReceivedFile>>>,
}

impl LocalSendServer {
    pub fn new(
        alias: String,
        port: u16,
        save_dir: PathBuf,
    ) -> std::result::Result<Self, crate::error::LocalSendError> {
        let device = DeviceInfo {
            alias,
            version: crate::protocol::PROTOCOL_VERSION.to_string(),
            device_model: Some(crate::device::get_device_model()),
            device_type: Some(crate::device::get_device_type()),
            fingerprint: crate::crypto::generate_fingerprint(),
            port,
            protocol: Protocol::Http,
            download: false,
            ip: None,
        };
        Self::new_with_device(
            device,
            save_dir,
            false,
            Arc::new(RwLock::new(None)),
            Arc::new(RwLock::new(Vec::new())),
        )
    }

    pub fn new_with_device(
        device: DeviceInfo,
        save_dir: PathBuf,
        https: bool,
        pending_transfer: Arc<RwLock<Option<PendingTransfer>>>,
        received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    ) -> std::result::Result<Self, crate::error::LocalSendError> {
        Ok(Self {
            device,
            save_dir,
            handle: None,
            shutdown_tx: None,
            https,
            #[cfg(feature = "https")]
            tls_cert: None,
            pending_transfer,
            received_files,
        })
    }

    #[cfg(feature = "https")]
    pub fn set_tls_certificate(&mut self, cert: crate::crypto::TlsCertificate) {
        self.tls_cert = Some(cert);
    }

    pub async fn start(
        &mut self,
        progress_callback: Option<ProgressCallback>,
    ) -> std::result::Result<(), crate::error::LocalSendError> {
        let state = Arc::new(RwLock::new(ServerState {
            device: self.device.clone(),
            current_session: None,
            save_dir: self.save_dir.clone(),
            _progress_callback: progress_callback,
            pending_transfer: self.pending_transfer.clone(),
            received_files: self.received_files.clone(),
        }));

        let router = Self::create_router(state.clone());

        let addr = format!("0.0.0.0:{}", self.device.port);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        self.shutdown_tx = Some(shutdown_tx);

        if self.https {
            #[cfg(feature = "https")]
            {
                let (cert_pem, key_pem) = if let Some(ref cert) = self.tls_cert {
                    (cert.cert_pem.clone(), cert.key_pem.clone())
                } else {
                    let cert = crate::crypto::generate_tls_certificate()?;
                    (cert.cert_pem, cert.key_pem)
                };

                let tls_config =
                    RustlsConfig::from_pem(cert_pem.into_bytes(), key_pem.into_bytes())
                        .await
                        .map_err(|e| {
                            crate::error::LocalSendError::network(format!(
                                "TLS config error: {}",
                                e
                            ))
                        })?;

                let socket_addr: std::net::SocketAddr = addr.parse().map_err(|e| {
                    crate::error::LocalSendError::network(format!("Failed to parse address: {}", e))
                })?;

                let handle = tokio::spawn(async move {
                    tracing::info!("Starting HTTPS server on {}", socket_addr);
                    let server = axum_server::bind_rustls(socket_addr, tls_config)
                        .serve(router.into_make_service());

                    tokio::select! {
                        res = server => {
                            if let Err(e) = res {
                                tracing::error!("HTTPS server error: {}", e);
                            }
                        }
                        _ = shutdown_rx => {
                            tracing::info!("Stopping HTTPS server");
                        }
                    }
                });

                self.handle = Some(handle);
                Ok(())
            }
            #[cfg(not(feature = "https"))]
            {
                return Err(crate::error::LocalSendError::network(
                    "HTTPS support not enabled. Please build with --features https",
                ));
            }
        } else {
            let listener = TcpListener::bind(&addr).await?;
            tracing::info!("Starting HTTP server on {}", addr);

            let handle = tokio::spawn(async move {
                let server = axum::serve(listener, router).with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                });

                if let Err(e) = server.await {
                    tracing::error!("HTTP server error: {}", e);
                }
            });

            self.handle = Some(handle);
            Ok(())
        }
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }

    fn create_router(state: Arc<RwLock<ServerState>>) -> Router {
        Router::new()
            .route("/api/localsend/v2/info", get(handle_info))
            .route("/api/localsend/v2/register", post(handle_register))
            .route(
                "/api/localsend/v2/prepare-upload",
                post(handle_prepare_upload),
            )
            .route("/api/localsend/v2/upload", post(handle_upload))
            .route("/api/localsend/v2/cancel", post(handle_cancel))
            .with_state(state)
    }
}

async fn handle_info(State(state): State<Arc<RwLock<ServerState>>>) -> Response {
    let state = state.read().unwrap();
    Json(state.device.clone()).into_response()
}

async fn handle_register(
    State(state): State<Arc<RwLock<ServerState>>>,
    Json(remote_device): Json<DeviceInfo>,
) -> Response {
    tracing::debug!("Register request from {:?}", remote_device.alias);
    let state = state.read().unwrap();
    Json(state.device.clone()).into_response()
}

#[derive(Deserialize)]
struct PrepareUploadParams {
    #[serde(rename = "pin")]
    _pin: Option<String>,
}

async fn handle_prepare_upload(
    State(state_ref): State<Arc<RwLock<ServerState>>>,
    Query(_params): Query<PrepareUploadParams>,
    Json(request): Json<PrepareUploadRequest>,
) -> Response {
    use crate::protocol::{SessionId, Token};

    let session_id = SessionId::new();
    let mut files_map = HashMap::new();

    // Check if it's a text message (all files have non-empty preview and small size)
    let is_message = !request.files.is_empty()
        && request
            .files
            .values()
            .all(|f| f.preview.is_some() && f.size < 1024 * 1024);

    for file_id in request.files.keys() {
        let token = Token::new(&session_id, file_id);
        files_map.insert(file_id.clone(), token);
    }

    let (pending_transfer_arc, _sender_info, response_rx) = {
        let mut state = state_ref.write().unwrap();

        // Check for existing session timeout (e.g. 5 minutes or session finished)
        if let Some(session) = &state.current_session {
            if session.last_activity.elapsed().as_secs() > 300 {
                state.current_session = None;
            } else {
                tracing::warn!("Session already exists, rejecting new session");
                return StatusCode::CONFLICT.into_response();
            }
        }

        let session = ActiveSession {
            session_id: session_id.clone(),
            files: request.files.clone(),
            sender_alias: request.info.alias.clone(),
            last_activity: std::time::Instant::now(),
        };

        state.current_session = Some(session);

        let (response_tx, response_rx) = oneshot::channel();
        let pending = PendingTransfer {
            sender: request.info.clone(),
            files: request.files.clone(),
            response_tx,
        };

        // Notify UI
        {
            let mut pending_guard = state.pending_transfer.write().unwrap();
            *pending_guard = Some(pending);
        }

        (
            state.pending_transfer.clone(),
            request.info.clone(),
            response_rx,
        )
    };

    // Wait for user or timeout
    let accepted = match tokio::time::timeout(std::time::Duration::from_secs(60), response_rx).await
    {
        Ok(Ok(val)) => val,
        _ => false,
    };

    if !accepted {
        let mut pending_guard = pending_transfer_arc.write().unwrap();
        *pending_guard = None;

        let mut state = state_ref.write().unwrap();
        state.current_session = None;
        tracing::info!("Transfer rejected by user or timeout");
        return StatusCode::FORBIDDEN.into_response();
    }

    // Refresh last activity on acceptance
    {
        let mut state = state_ref.write().unwrap();
        if let Some(s) = &mut state.current_session {
            s.last_activity = std::time::Instant::now();
        }
    }

    // If it's a message, return 204 No Content
    if is_message {
        let mut pending_guard = pending_transfer_arc.write().unwrap();
        *pending_guard = None;

        let mut state = state_ref.write().unwrap();

        // Save messages to files and update TUI list
        for file in request.files.values() {
            if let Some(content) = &file.preview {
                let now = Local::now();
                let time_str = now.format("%Y-%m-%d %H:%M:%S").to_string();
                let filename = format!(
                    "message_{}_{}.txt",
                    now.format("%Y%m%d_%H%M%S"),
                    file.file_name.replace("/", "_")
                );
                let path = state.save_dir.join(filename.clone());
                if let Err(e) = std::fs::write(&path, content) {
                    tracing::error!("Failed to save message to {:?}: {}", path, e);
                } else {
                    tracing::info!("Saved message to {:?}", path);

                    // Update TUI list
                    let mut files_list = state.received_files.write().unwrap();
                    files_list.push(ReceivedFile {
                        file_name: filename,
                        size: content.len() as u64,
                        sender: request.info.alias.clone(),
                        time: time_str,
                    });
                }
            }
        }

        state.current_session = None;
        return StatusCode::NO_CONTENT.into_response();
    }

    Json(PrepareUploadResponse {
        session_id,
        files: files_map,
    })
    .into_response()
}

#[derive(Deserialize)]
struct UploadParams {
    #[serde(rename = "sessionId")]
    session_id: SessionId,
    #[serde(rename = "fileId")]
    file_id: FileId,
    #[serde(rename = "token")]
    token: crate::protocol::Token,
}

async fn handle_upload(
    State(state_ref): State<Arc<RwLock<ServerState>>>,
    Query(params): Query<UploadParams>,
    body: Bytes,
) -> Response {
    let mut state = state_ref.write().unwrap();

    // Verify session
    let (file_name, session_id) = if let Some(session) = &state.current_session {
        if session.session_id != params.session_id {
            tracing::warn!(
                "Upload rejected: Session ID mismatch. Expected {}, got {}",
                session.session_id,
                params.session_id
            );
            return StatusCode::FORBIDDEN.into_response();
        }

        // Verify token
        let expected_token = crate::protocol::Token::new(&session.session_id, &params.file_id);
        if params.token.as_str() != expected_token.as_str() {
            tracing::warn!("Upload rejected: Token mismatch");
            return StatusCode::FORBIDDEN.into_response();
        }

        // Find file metadata
        if let Some(meta) = session.files.get(&params.file_id) {
            (meta.file_name.clone(), session.session_id.clone())
        } else {
            tracing::warn!(
                "Upload rejected: File ID {} not found in session",
                params.file_id
            );
            return StatusCode::NOT_FOUND.into_response();
        }
    } else {
        tracing::warn!("Upload rejected: No active session");
        return StatusCode::FORBIDDEN.into_response();
    };

    let save_path = state.save_dir.join(&file_name);

    // Ensure parent directory exists
    if let Some(parent) = save_path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::error!("Failed to create directory {:?}: {}", parent, e);
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let body_len = body.len() as u64;

    if let Err(e) = std::fs::write(&save_path, body) {
        tracing::error!("Failed to save file to {:?}: {}", save_path, e);
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    tracing::info!("Received file: {:?} for session {}", save_path, session_id);

    // Update TUI list
    {
        let sender = state
            .current_session
            .as_ref()
            .map(|s| s.sender_alias.clone())
            .unwrap_or_else(|| "Unknown".to_string());
        let time_str = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let mut files_list = state.received_files.write().unwrap();
        files_list.push(ReceivedFile {
            file_name,
            size: body_len,
            sender,
            time: time_str,
        });
    }

    // Update last activity and check if session is complete (simple heuristic: 1 file for now)
    // In a real LocalSend implementation, we'd wait for all files.
    if let Some(s) = &mut state.current_session {
        s.last_activity = std::time::Instant::now();
        // For simplicity, we clear session after one file if it was the only one
        if s.files.len() <= 1 {
            state.current_session = None;
        }
    }

    StatusCode::OK.into_response()
}

#[derive(Deserialize)]
struct CancelParams {
    #[serde(rename = "sessionId")]
    session_id: SessionId,
}

async fn handle_cancel(
    State(state_ref): State<Arc<RwLock<ServerState>>>,
    Query(params): Query<CancelParams>,
) -> Response {
    let mut state = state_ref.write().unwrap();

    if let Some(session) = &state.current_session
        && session.session_id.as_str() == params.session_id.as_str()
    {
        state.current_session = None;
        tracing::info!("Session {} cancelled", params.session_id);
    }

    StatusCode::OK.into_response()
}
