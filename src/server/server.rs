use crate::protocol::{DeviceInfo, FileMetadata, PrepareUploadRequest, PrepareUploadResponse};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
};
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

pub struct LocalSendServer {
    device: DeviceInfo,
    save_dir: PathBuf,
    handle: Option<JoinHandle<()>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    https: bool,
    #[cfg(feature = "https")]
    tls_cert: Option<crate::crypto::TlsCertificate>,
}

pub struct ActiveSession {
    session_id: String,
    _files: HashMap<String, FileMetadata>,
}

pub struct ServerState {
    device: DeviceInfo,
    current_session: Option<ActiveSession>,
    _save_dir: PathBuf,
    _progress_callback: Option<ProgressCallback>,
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
            protocol: "http".to_string(),
            download: false,
            ip: None,
        };
        Self::new_with_device(device, save_dir, false)
    }

    pub fn new_with_device(
        device: DeviceInfo,
        save_dir: PathBuf,
        https: bool,
    ) -> std::result::Result<Self, crate::error::LocalSendError> {
        Ok(Self {
            device,
            save_dir,
            handle: None,
            shutdown_tx: None,
            https,
            #[cfg(feature = "https")]
            tls_cert: None,
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
            _save_dir: self.save_dir.clone(),
            _progress_callback: progress_callback,
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

                // RustlsConfig::from_pem expects PEM bytes, not parsed DER
                let tls_config =
                    RustlsConfig::from_pem(cert_pem.into_bytes(), key_pem.into_bytes())
                        .await
                        .map_err(|e| {
                            crate::error::LocalSendError::Network(format!(
                                "Failed to create TLS config: {}",
                                e
                            ))
                        })?;

                // Parse address string into SocketAddr
                let socket_addr: std::net::SocketAddr = addr.parse().map_err(|e| {
                    crate::error::LocalSendError::Network(format!("Failed to parse address: {}", e))
                })?;

                let handle = tokio::spawn(async move {
                    let server = axum_server::bind_rustls(socket_addr, tls_config)
                        .serve(router.into_make_service());

                    tokio::select! {
                        _ = server => {},
                        _ = shutdown_rx => {},
                    }
                });

                self.handle = Some(handle);
                return Ok(());
            }
            #[cfg(not(feature = "https"))]
            {
                return Err(crate::error::LocalSendError::Network(
                    "HTTPS support not enabled. Please build with --features https".to_string(),
                ));
            }
        } else {
            let listener = TcpListener::bind(&addr).await?;

            let handle = tokio::spawn(async move {
                axum::serve(listener, router)
                    .with_graceful_shutdown(async {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .unwrap();
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

async fn handle_info(State(state): State<Arc<RwLock<ServerState>>>) -> Json<DeviceInfo> {
    let state = state.read().unwrap();
    Json(state.device.clone())
}

async fn handle_register(
    State(state): State<Arc<RwLock<ServerState>>>,
    Json(remote_device): Json<DeviceInfo>,
) -> Json<DeviceInfo> {
    tracing::debug!("Register request from {:?}", remote_device.alias);
    let state = state.read().unwrap();
    Json(state.device.clone())
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
) -> Result<Json<PrepareUploadResponse>, StatusCode> {
    let mut state = state_ref.write().unwrap();

    if state.current_session.is_some() {
        tracing::warn!("Session already exists, rejecting new session");
        return Err(StatusCode::CONFLICT);
    }

    let session_id = uuid::Uuid::new_v4().to_string();
    let mut files_map = HashMap::new();

    for (file_id, _file_meta) in &request.files {
        let token = format!("{}_{}", session_id, file_id);
        files_map.insert(file_id.clone(), token);
    }

    let session = ActiveSession {
        session_id: session_id.clone(),
        _files: request.files,
    };

    state.current_session = Some(session);

    Ok(Json(PrepareUploadResponse {
        session_id,
        files: files_map,
    }))
}

#[derive(Deserialize)]
struct UploadParams {
    #[serde(rename = "sessionId")]
    _session_id: String,
    #[serde(rename = "fileId")]
    _file_id: String,
    #[serde(rename = "token")]
    _token: String,
}

async fn handle_upload(
    State(_state): State<Arc<RwLock<ServerState>>>,
    Query(_params): Query<UploadParams>,
) -> StatusCode {
    tracing::warn!("Upload endpoint not fully implemented yet");
    StatusCode::NOT_IMPLEMENTED
}

#[derive(Deserialize)]
struct CancelParams {
    #[serde(rename = "sessionId")]
    session_id: String,
}

async fn handle_cancel(
    State(state_ref): State<Arc<RwLock<ServerState>>>,
    Query(params): Query<CancelParams>,
) -> axum::http::StatusCode {
    let mut state = state_ref.write().unwrap();

    if let Some(session) = &state.current_session {
        if session.session_id == params.session_id {
            state.current_session = None;
            tracing::info!("Session {} cancelled", params.session_id);
        }
    }

    axum::http::StatusCode::OK
}
