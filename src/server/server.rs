use super::state::{PendingTransfer, ProgressCallback, ServerState};
use crate::protocol::{DeviceInfo, Protocol, ReceivedFile};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{Notify, RwLock, oneshot};
use tokio::task::JoinHandle;

#[cfg(feature = "https")]
use axum_server::tls_rustls::RustlsConfig;

pub struct LocalSendServer {
    device: DeviceInfo,
    save_dir: PathBuf,
    handle: Option<JoinHandle<()>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    https: bool,
    #[cfg(feature = "https")]
    tls_cert: Option<crate::crypto::TlsCertificate>,
    pending_transfer: Arc<RwLock<Option<PendingTransfer>>>,
    pending_transfer_notify: Option<Arc<Notify>>,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
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
            device_model: Some(crate::core::device::get_device_model()),
            device_type: Some(crate::core::device::get_device_type()),
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
            pending_transfer_notify: None,
            received_files,
        })
    }

    pub fn set_pending_transfer_notify(&mut self, notify: Arc<Notify>) {
        self.pending_transfer_notify = Some(notify);
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
            pending_transfer_notify: self.pending_transfer_notify.clone(),
            received_files: self.received_files.clone(),
        }));

        let router = super::routes::create_router(state.clone());

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
}
