use super::events::ServerEvent;
use super::state::{PendingTransfer, ProgressCallback, ServerState};
use crate::protocol::{DeviceInfo, Protocol, ReceivedFile};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{RwLock, mpsc, oneshot};
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
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    events_rx: Option<mpsc::Receiver<ServerEvent>>,
    auto_accept: bool,
    accept_timeout: Duration,
    /// Stored now; enforcement lands in Task 2.6.
    #[allow(dead_code)]
    pin: Option<String>,
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
        _pending_transfer: Arc<RwLock<Option<PendingTransfer>>>,
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
            received_files,
            events_rx: None,
            auto_accept: false,
            accept_timeout: Duration::from_secs(60),
            pin: None,
        })
    }

    /// Private constructor used by [`LocalSendServerBuilder::build`]. Holds the
    /// fields `new_with_device` used to take, plus `pin`/`auto_accept`/`accept_timeout`.
    /// `pin` is stored now; enforcement lands in Task 2.6.
    fn from_parts(
        device: DeviceInfo,
        save_dir: PathBuf,
        https: bool,
        pin: Option<String>,
        auto_accept: bool,
        accept_timeout: Duration,
    ) -> std::result::Result<Self, crate::error::LocalSendError> {
        Ok(Self {
            device,
            save_dir,
            handle: None,
            shutdown_tx: None,
            https,
            #[cfg(feature = "https")]
            tls_cert: None,
            received_files: Arc::new(RwLock::new(Vec::new())),
            events_rx: None,
            auto_accept,
            accept_timeout,
            pin,
        })
    }

    /// Returns the actual bound port. If the server was started with an
    /// ephemeral port (`0`), this reflects the OS-assigned port after
    /// `start()`/`builder().build()` has returned.
    pub fn port(&self) -> u16 {
        self.device.port
    }

    pub fn device(&self) -> &DeviceInfo {
        &self.device
    }

    pub fn builder() -> LocalSendServerBuilder {
        LocalSendServerBuilder {
            alias: "LocalSend-Rust".to_string(),
            port: crate::protocol::DEFAULT_HTTP_PORT,
            save_dir: PathBuf::from("./downloads"),
            protocol: Protocol::Http,
            pin: None,
            auto_accept: false,
            accept_timeout: Duration::from_secs(60),
        }
    }

    /// No longer wired to anything (Task 2.2 replaced the rendezvous with the
    /// public event stream). Kept as a no-op until Task 2.5 removes it.
    #[deprecated(
        note = "no-op since Task 2.2; use take_events()/set_auto_accept() instead, removed in Task 2.5"
    )]
    pub fn set_pending_transfer_notify(&mut self, _notify: Arc<tokio::sync::Notify>) {}

    /// Take the event receiver. Returns `Some` once, after `start()`.
    pub fn take_events(&mut self) -> Option<mpsc::Receiver<ServerEvent>> {
        self.events_rx.take()
    }

    pub fn set_auto_accept(&mut self, yes: bool) {
        self.auto_accept = yes;
    }

    #[cfg(feature = "https")]
    pub fn set_tls_certificate(&mut self, cert: crate::crypto::TlsCertificate) {
        self.tls_cert = Some(cert);
    }

    pub async fn start(
        &mut self,
        progress_callback: Option<ProgressCallback>,
    ) -> std::result::Result<(), crate::error::LocalSendError> {
        let (events_tx, events_rx) = mpsc::channel(64);
        self.events_rx = Some(events_rx);

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

                // Bind before spawn so the real (possibly OS-assigned) port is
                // known before the ServerState/router are built.
                let std_listener = std::net::TcpListener::bind(&addr)?;
                std_listener.set_nonblocking(true)?;
                let bound_port = std_listener.local_addr()?.port();
                self.device.port = bound_port;

                let state = Arc::new(RwLock::new(ServerState {
                    device: self.device.clone(),
                    current_session: None,
                    save_dir: self.save_dir.clone(),
                    _progress_callback: progress_callback,
                    received_files: self.received_files.clone(),
                    events_tx,
                    auto_accept: self.auto_accept,
                    accept_timeout: self.accept_timeout,
                }));
                let router = super::routes::create_router(state.clone());

                let server = axum_server::from_tcp_rustls(std_listener, tls_config)
                    .map_err(|e| {
                        crate::error::LocalSendError::network(format!(
                            "Failed to serve HTTPS listener: {}",
                            e
                        ))
                    })?
                    .serve(router.into_make_service());

                let handle = tokio::spawn(async move {
                    tracing::info!("Starting HTTPS server on port {}", bound_port);

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
                Err(crate::error::LocalSendError::network(
                    "HTTPS support not enabled. Please build with --features https",
                ))
            }
        } else {
            // Bind before spawn so the real (possibly OS-assigned) port is
            // known before the ServerState/router are built.
            let listener = TcpListener::bind(&addr).await?;
            let bound_port = listener.local_addr()?.port();
            self.device.port = bound_port;
            tracing::info!("Starting HTTP server on port {}", bound_port);

            let state = Arc::new(RwLock::new(ServerState {
                device: self.device.clone(),
                current_session: None,
                save_dir: self.save_dir.clone(),
                _progress_callback: progress_callback,
                received_files: self.received_files.clone(),
                events_tx,
                auto_accept: self.auto_accept,
                accept_timeout: self.accept_timeout,
            }));
            let router = super::routes::create_router(state.clone());

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

/// Builder for [`LocalSendServer`]; the canonical construction path.
///
/// `build()` binds the listener, starts serving, and returns the server
/// together with its [`ServerEvent`] receiver — the server is already
/// listening when `build()` returns. Pass `port(0)` for an OS-assigned
/// ephemeral port, then read the real port back via [`LocalSendServer::port`].
pub struct LocalSendServerBuilder {
    alias: String,
    port: u16,
    save_dir: PathBuf,
    protocol: Protocol,
    pin: Option<String>,
    auto_accept: bool,
    accept_timeout: Duration,
}

impl LocalSendServerBuilder {
    pub fn alias(mut self, alias: impl Into<String>) -> Self {
        self.alias = alias.into();
        self
    }

    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn save_dir(mut self, dir: impl AsRef<std::path::Path>) -> Self {
        self.save_dir = dir.as_ref().to_path_buf();
        self
    }

    pub fn protocol(mut self, protocol: Protocol) -> Self {
        self.protocol = protocol;
        self
    }

    pub fn pin(mut self, pin: impl Into<String>) -> Self {
        self.pin = Some(pin.into());
        self
    }

    pub fn auto_accept(mut self, yes: bool) -> Self {
        self.auto_accept = yes;
        self
    }

    pub fn accept_timeout(mut self, d: Duration) -> Self {
        self.accept_timeout = d;
        self
    }

    pub async fn build(self) -> crate::Result<(LocalSendServer, mpsc::Receiver<ServerEvent>)> {
        let https = matches!(self.protocol, Protocol::Https);

        #[cfg(feature = "https")]
        let tls_cert = if https {
            Some(crate::crypto::generate_tls_certificate()?)
        } else {
            None
        };
        #[cfg(not(feature = "https"))]
        if https {
            return Err(crate::error::LocalSendError::network(
                "HTTPS support not enabled; build with --features https",
            ));
        }

        // HTTPS identity = SHA-256 of the cert (spec); HTTP = random string.
        let fingerprint = {
            #[cfg(feature = "https")]
            if let Some(ref cert) = tls_cert {
                cert.fingerprint.clone()
            } else {
                crate::crypto::generate_fingerprint()
            }
            #[cfg(not(feature = "https"))]
            crate::crypto::generate_fingerprint()
        };

        let device = DeviceInfo {
            alias: self.alias,
            version: crate::protocol::PROTOCOL_VERSION.to_string(),
            device_model: Some(crate::core::device::get_device_model()),
            device_type: Some(crate::core::device::get_device_type()),
            fingerprint,
            port: self.port,
            protocol: self.protocol,
            download: false,
            ip: None,
        };

        let mut server = LocalSendServer::from_parts(
            device,
            self.save_dir,
            https,
            self.pin,
            self.auto_accept,
            self.accept_timeout,
        )?;
        #[cfg(feature = "https")]
        if let Some(cert) = tls_cert {
            server.set_tls_certificate(cert);
        }
        server.start(None).await?;
        let events = server.take_events().expect("events available after start");
        Ok((server, events))
    }
}
