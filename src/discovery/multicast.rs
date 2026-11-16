use crate::client::{LocalSendClient, TlsTrustPolicy};
use crate::core::device::{get_device_model, get_device_type, get_local_ip};
use crate::crypto::generate_fingerprint;
use crate::discovery::Discovery;
use crate::error::LocalSendError;
use crate::protocol::{
    AnnouncementMessage, DEFAULT_MULTICAST_ADDRESS, DEFAULT_MULTICAST_PORT, DeviceInfo,
    PROTOCOL_VERSION, Protocol,
};
use if_addrs::{IfAddr, get_if_addrs};
use socket2::{Domain, Protocol as SocketProtocol, Socket, Type};
use std::collections::BTreeSet;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::{Mutex as AsyncMutex, broadcast, watch};

mod announcement;
mod interfaces;

use announcement::{AnnouncementSendSummary, send_announcement_round};
use interfaces::{select_interface_addresses, select_multicast_candidate_addresses};

pub type Result<T> = std::result::Result<T, LocalSendError>;

/// An announcement responder is best-effort discovery work, not a transfer.
/// Bound registration so a peer that accepts TCP but never answers cannot
/// prevent an awaited stop from quiescing forever.
const ANNOUNCEMENT_REGISTER_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MulticastConfig {
    pub address: Ipv4Addr,
    pub port: u16,
    pub interface_names: Option<BTreeSet<String>>,
}

impl MulticastConfig {
    pub fn new(
        address: Ipv4Addr,
        port: u16,
        interface_names: Option<BTreeSet<String>>,
    ) -> Result<Self> {
        if !address.is_multicast() {
            return Err(LocalSendError::InvalidMulticastAddress(address.to_string()));
        }
        if port == 0 {
            return Err(LocalSendError::InvalidPort(port.to_string()));
        }
        Ok(Self {
            address,
            port,
            interface_names,
        })
    }
}

impl Default for MulticastConfig {
    fn default() -> Self {
        Self {
            address: DEFAULT_MULTICAST_ADDRESS
                .parse()
                .expect("LocalSend's multicast constant must be a valid IPv4 address"),
            port: DEFAULT_MULTICAST_PORT,
            interface_names: None,
        }
    }
}

#[derive(Clone)]
pub struct MulticastDiscovery {
    local_device: DeviceInfo,
    config: MulticastConfig,
    lifecycle: Arc<DiscoveryLifecycle>,
    tx: Option<broadcast::Sender<DeviceInfo>>,
    #[cfg(feature = "https")]
    client_certificate: Option<crate::crypto::TlsCertificate>,
}

/// Coordinates the current multicast run across clones of a discovery value.
///
/// The transition lock serializes starts and awaited stops. Synchronous
/// `Discovery::stop` only signals the current run; it never blocks on network
/// work and leaves final socket release to `stop_and_wait` or the next start.
struct DiscoveryLifecycle {
    current: Mutex<Option<Arc<DiscoveryRun>>>,
    transition: AsyncMutex<()>,
}

/// One generation of discovery workers. A fresh stop signal per generation
/// prevents old workers from observing a later restart as permission to resume.
struct DiscoveryRun {
    running: AtomicBool,
    stop_tx: watch::Sender<bool>,
    active_tasks: watch::Sender<usize>,
    sockets: Mutex<Vec<Arc<UdpSocket>>>,
}

struct DiscoveryTaskGuard {
    run: Arc<DiscoveryRun>,
}

impl DiscoveryLifecycle {
    fn new() -> Self {
        Self {
            current: Mutex::new(None),
            transition: AsyncMutex::new(()),
        }
    }

    fn current(&self) -> Option<Arc<DiscoveryRun>> {
        self.current.lock().unwrap().clone()
    }

    fn clear_if_current(&self, run: &Arc<DiscoveryRun>) {
        let mut current = self.current.lock().unwrap();
        if current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, run))
        {
            *current = None;
        }
    }
}

impl DiscoveryRun {
    fn new() -> Self {
        let (stop_tx, _) = watch::channel(false);
        let (active_tasks, _) = watch::channel(0);
        Self {
            running: AtomicBool::new(true),
            stop_tx,
            active_tasks,
            sockets: Mutex::new(Vec::new()),
        }
    }

    fn request_stop(&self) {
        self.running.store(false, Ordering::Release);
        self.stop_tx.send_replace(true);
        self.clear_sockets_if_idle();
    }

    fn clear_sockets_if_idle(&self) {
        if *self.active_tasks.borrow() == 0 {
            self.sockets.lock().unwrap().clear();
        }
    }

    fn sockets(&self) -> Vec<Arc<UdpSocket>> {
        self.sockets.lock().unwrap().clone()
    }

    fn add_task_guard(self: &Arc<Self>) -> Option<DiscoveryTaskGuard> {
        if !self.running.load(Ordering::Acquire) {
            return None;
        }

        self.active_tasks.send_modify(|active| *active += 1);
        let guard = DiscoveryTaskGuard { run: self.clone() };
        if !self.running.load(Ordering::Acquire) {
            drop(guard);
            return None;
        }
        Some(guard)
    }

    fn spawn<F>(self: &Arc<Self>, future: F) -> bool
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let Some(guard) = self.add_task_guard() else {
            return false;
        };
        tokio::spawn(async move {
            let _guard = guard;
            future.await;
        });
        true
    }

    async fn wait_for_idle(&self) {
        let mut active = self.active_tasks.subscribe();
        loop {
            if *active.borrow_and_update() == 0 {
                return;
            }
            if active.changed().await.is_err() {
                return;
            }
        }
    }
}

impl Drop for DiscoveryTaskGuard {
    fn drop(&mut self) {
        self.run.active_tasks.send_modify(|active| {
            debug_assert!(*active > 0);
            *active = active.saturating_sub(1);
        });
        if !self.run.running.load(Ordering::Acquire) {
            self.run.clear_sockets_if_idle();
        }
    }
}

impl MulticastDiscovery {
    pub fn new(alias: String, port: u16, protocol: Protocol) -> Result<Self> {
        let device = DeviceInfo {
            alias,
            version: PROTOCOL_VERSION.to_string(),
            device_model: Some(get_device_model()),
            device_type: Some(get_device_type()),
            fingerprint: generate_fingerprint(),
            port,
            protocol,
            download: false,
            ip: None,
        };

        Ok(Self::new_with_device(device))
    }

    pub fn new_with_device(device: DeviceInfo) -> Self {
        Self::new_with_device_and_config(device, MulticastConfig::default())
            .expect("default multicast configuration must be valid")
    }

    pub fn new_with_device_and_config(device: DeviceInfo, config: MulticastConfig) -> Result<Self> {
        let config = MulticastConfig::new(config.address, config.port, config.interface_names)?;
        let (tx, _rx) = broadcast::channel(100);
        Ok(Self {
            local_device: device,
            config,
            lifecycle: Arc::new(DiscoveryLifecycle::new()),
            tx: Some(tx),
            #[cfg(feature = "https")]
            client_certificate: None,
        })
    }

    /// Replace the identity used by future announcements without rebuilding
    /// sockets or losing the current discovery cache.
    pub fn set_local_device(&mut self, device: DeviceInfo) {
        self.local_device = device;
    }

    /// Uses the receiver's TLS identity when answering HTTPS announcements.
    /// The same certificate is used by the HTTP fallback and file sender, so
    /// mobile peers requiring mutual TLS see one stable LocalSend identity.
    #[cfg(feature = "https")]
    pub fn set_client_certificate(&mut self, certificate: crate::crypto::TlsCertificate) {
        // HTTPS LocalSend peers identify the sender by the presented client
        // certificate. Keep the JSON identity and TLS identity inseparable so
        // current iOS peers do not discard `/register` as inconsistent.
        if self.local_device.protocol == Protocol::Https {
            self.local_device.fingerprint = certificate.fingerprint.clone();
        }
        self.client_certificate = Some(certificate);
    }

    /// Requests discovery shutdown and waits until every receive, response,
    /// and announcement task from the current run has returned. Unlike the
    /// synchronous [`Discovery::stop`], this is a quiescence boundary: after it
    /// resolves, the run's sockets have been released and restart is safe.
    ///
    /// Active HTTP registrations are allowed to finish; they are never aborted
    /// just because shutdown was requested.
    pub async fn stop_and_wait(&self) {
        let _transition = self.lifecycle.transition.lock().await;
        let Some(run) = self.lifecycle.current() else {
            return;
        };

        run.request_stop();
        run.wait_for_idle().await;
        run.sockets.lock().unwrap().clear();
        self.lifecycle.clear_if_current(&run);
    }
}

#[async_trait::async_trait]
impl Discovery for MulticastDiscovery {
    async fn start(&mut self) -> std::result::Result<(), LocalSendError> {
        let _transition = self.lifecycle.transition.lock().await;
        if let Some(previous) = self.lifecycle.current() {
            if previous.running.load(Ordering::Acquire) {
                return Err(LocalSendError::network("Discovery already running"));
            }

            previous.request_stop();
            previous.wait_for_idle().await;
            previous.sockets.lock().unwrap().clear();
            self.lifecycle.clear_if_current(&previous);
        }

        if self.tx.is_none() {
            let (tx, _rx) = broadcast::channel(100);
            self.tx = Some(tx);
        }

        let run = Arc::new(DiscoveryRun::new());
        *self.lifecycle.current.lock().unwrap() = Some(run.clone());

        let bind_addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, self.config.port));
        let sockets = match Self::multicast_interfaces(self.config.interface_names.as_ref())
            .and_then(|interfaces| {
                interfaces
                    .into_iter()
                    .map(|interface| {
                        create_reusable_udp_socket(&bind_addr, interface, self.config.address)
                            .map(Arc::new)
                    })
                    .collect::<Result<Vec<_>>>()
            }) {
            Ok(sockets) => sockets,
            Err(error) => {
                run.request_stop();
                self.lifecycle.clear_if_current(&run);
                return Err(error);
            }
        };

        if !run.running.load(Ordering::Acquire) {
            self.lifecycle.clear_if_current(&run);
            return Err(LocalSendError::network(
                "Discovery was stopped while starting",
            ));
        }
        *run.sockets.lock().unwrap() = sockets.clone();

        for socket in sockets {
            let tx = self.tx.as_ref().unwrap().clone();
            let local_fingerprint = self.local_device.fingerprint.clone();
            let local_device = self.local_device.clone();
            let multicast_addr = SocketAddr::from((self.config.address, self.config.port));
            let mut stop = run.stop_tx.subscribe();
            let receiver_run = run.clone();
            #[cfg(feature = "https")]
            let client_certificate = self.client_certificate.clone();

            run.spawn(async move {
                let mut buf = vec![0u8; 65536];

                loop {
                    tokio::select! {
                        biased;
                        changed = stop.changed() => {
                            if changed.is_err() || *stop.borrow() {
                                break;
                            }
                        }
                        result = socket.recv_from(&mut buf) => {
                            let Ok((len, src)) = result else {
                                continue;
                            };
                            if len == 0 {
                                continue;
                            }

                            let msg = match String::from_utf8(buf[..len].to_vec()) {
                                Ok(s) => s,
                                Err(_) => continue,
                            };
                            let Ok(announcement) =
                                serde_json::from_str::<AnnouncementMessage>(&msg)
                            else {
                                continue;
                            };
                            if announcement.fingerprint == local_fingerprint {
                                continue;
                            }

                            let device = DeviceInfo {
                                alias: announcement.alias.clone(),
                                version: announcement.version.clone(),
                                device_model: announcement.device_model.clone(),
                                device_type: announcement.device_type,
                                fingerprint: announcement.fingerprint.clone(),
                                port: announcement.port,
                                protocol: announcement.protocol,
                                download: announcement.download,
                                ip: Some(src.ip().to_string()),
                            };
                            let is_announcement =
                                announcement.announce || announcement.announcement.unwrap_or(false);
                            let _ = tx.send(device.clone());

                            if is_announcement && receiver_run.running.load(Ordering::Acquire) {
                                let response_local_device = local_device.clone();
                                let response_socket = socket.clone();
                                let response_run = receiver_run.clone();
                                #[cfg(feature = "https")]
                                let client_certificate = client_certificate.clone();

                                receiver_run.spawn(async move {
                                    Self::respond_to_announcement(
                                        &device,
                                        &response_local_device,
                                        &response_socket,
                                        multicast_addr,
                                        &response_run,
                                        #[cfg(feature = "https")]
                                        client_certificate,
                                    )
                                    .await;
                                });
                            }
                        }
                    }
                }
            });
        }

        if !run.running.load(Ordering::Acquire) {
            run.wait_for_idle().await;
            run.sockets.lock().unwrap().clear();
            self.lifecycle.clear_if_current(&run);
            return Err(LocalSendError::network(
                "Discovery was stopped while starting",
            ));
        }

        Ok(())
    }

    fn stop(&mut self) {
        if let Some(run) = self.lifecycle.current() {
            run.request_stop();
        }
        self.tx = None;
    }

    async fn announce_presence(&self) -> std::result::Result<(), LocalSendError> {
        let run = self
            .lifecycle
            .current()
            .ok_or_else(|| LocalSendError::network("Discovery not started"))?;
        let _task = run
            .add_task_guard()
            .ok_or_else(|| LocalSendError::network("Discovery not started"))?;
        let sockets = run.sockets();
        if sockets.is_empty() {
            return Err(LocalSendError::network("Discovery not started"));
        }

        let announcement = AnnouncementMessage {
            alias: self.local_device.alias.clone(),
            version: self.local_device.version.clone(),
            device_model: self.local_device.device_model.clone(),
            device_type: self.local_device.device_type,
            fingerprint: self.local_device.fingerprint.clone(),
            port: self.local_device.port,
            protocol: self.local_device.protocol,
            download: self.local_device.download,
            announce: true,
            announcement: Some(true),
        };

        let msg = serde_json::to_string(&announcement)?;
        let buf = msg.as_bytes();
        let multicast_addr = SocketAddr::from((self.config.address, self.config.port));

        // Send announcement multiple times with delays to improve reliability.
        // The task guard makes stop_and_wait wait rather than release sockets
        // or restart while these announcements are still in flight.
        let delays = [100, 500, 2000];
        let mut summary = AnnouncementSendSummary::default();
        let mut attempt = 0;
        let mut stop = run.stop_tx.subscribe();
        for delay in delays {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                _ = stop.changed() => {
                    return Err(LocalSendError::network("Discovery stopped during announcement"));
                }
            }
            if !run.running.load(Ordering::Acquire) {
                return Err(LocalSendError::network(
                    "Discovery stopped during announcement",
                ));
            }
            send_announcement_round(&sockets, buf, multicast_addr, attempt, &mut summary).await;
            attempt += 1;
        }

        for delay in [2000, 4000] {
            if !summary.needs_recovery_retry() {
                break;
            }
            tracing::warn!(
                delay_ms = delay,
                "every LocalSend multicast interface failed; retrying after a startup grace period"
            );
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                _ = stop.changed() => {
                    return Err(LocalSendError::network("Discovery stopped during announcement"));
                }
            }
            if !run.running.load(Ordering::Acquire) {
                return Err(LocalSendError::network(
                    "Discovery stopped during announcement",
                ));
            }
            send_announcement_round(&sockets, buf, multicast_addr, attempt, &mut summary).await;
            attempt += 1;
        }

        summary.finish()
    }

    fn on_discovered<F>(&mut self, callback: F)
    where
        F: Fn(DeviceInfo) + Send + Sync + 'static,
    {
        let tx = if let Some(ref t) = self.tx {
            t.clone()
        } else {
            return;
        };

        let mut rx = tx.subscribe();
        tokio::spawn(async move {
            while let Ok(device) = rx.recv().await {
                callback(device);
            }
        });
    }

    fn get_known_devices(&self) -> Vec<DeviceInfo> {
        vec![]
    }
}

impl MulticastDiscovery {
    fn multicast_interfaces(interface_names: Option<&BTreeSet<String>>) -> Result<Vec<Ipv4Addr>> {
        let addresses = get_if_addrs()
            .map_err(|error| {
                LocalSendError::network(format!("Failed to list interfaces: {error}"))
            })?
            .into_iter()
            .filter(|interface| !interface.is_loopback())
            .filter_map(|interface| match interface.addr {
                IfAddr::V4(address) => Some((interface.name, address.ip, address.netmask)),
                IfAddr::V6(_) => None,
            });
        let addresses = addresses.collect::<Vec<_>>();
        let primary = interface_names
            .is_none()
            .then(|| get_local_ip().ok())
            .flatten();
        let mut interfaces = select_multicast_candidate_addresses(
            addresses.iter().cloned(),
            interface_names,
            primary,
        );
        tracing::debug!(
            ?primary,
            configured_interfaces = ?interface_names,
            candidates = ?interfaces,
            "selected LocalSend multicast interface candidates"
        );

        if interfaces.is_empty() && primary.is_some() && interface_names.is_none() {
            interfaces = select_interface_addresses(addresses, None, None);
        }

        if interfaces.is_empty() && interface_names.is_none() {
            Ok(vec![Ipv4Addr::UNSPECIFIED])
        } else {
            Ok(interfaces)
        }
    }

    #[cfg(feature = "https")]
    fn client_for_announcement(
        local_device: DeviceInfo,
        target_device: &DeviceInfo,
        client_certificate: Option<&crate::crypto::TlsCertificate>,
    ) -> Result<LocalSendClient> {
        match target_device.protocol {
            Protocol::Http => Ok(LocalSendClient::new(local_device)),
            Protocol::Https => {
                let policy = TlsTrustPolicy::PinnedFingerprint(target_device.fingerprint.clone());
                match client_certificate {
                    Some(certificate) => LocalSendClient::with_trust_policy_and_client_certificate(
                        local_device,
                        policy,
                        certificate,
                    ),
                    None => LocalSendClient::with_trust_policy(local_device, policy),
                }
            }
        }
    }

    #[cfg(not(feature = "https"))]
    fn client_for_announcement(
        local_device: DeviceInfo,
        target_device: &DeviceInfo,
    ) -> Result<LocalSendClient> {
        match target_device.protocol {
            Protocol::Http => Ok(LocalSendClient::new(local_device)),
            Protocol::Https => LocalSendClient::with_trust_policy(
                local_device,
                TlsTrustPolicy::PinnedFingerprint(target_device.fingerprint.clone()),
            ),
        }
    }

    pub async fn scan(
        &mut self,
        duration: Duration,
        devices: Arc<RwLock<Vec<DeviceInfo>>>,
    ) -> Result<()> {
        if self
            .lifecycle
            .current()
            .is_none_or(|run| !run.running.load(Ordering::Acquire))
        {
            self.start().await?;
        }

        // Register a callback to update the devices list during the scan.
        let devices_clone = devices.clone();
        self.on_discovered(move |device| {
            let mut guard = devices_clone.write().unwrap();
            if !guard.iter().any(|d| d.fingerprint == device.fingerprint) {
                guard.push(device);
            }
        });

        // Clear devices.
        devices.write().unwrap().clear();

        // Announce.
        self.announce_presence().await?;

        // Wait for responses.
        tokio::time::sleep(duration).await;

        Ok(())
    }

    async fn respond_to_announcement(
        target_device: &DeviceInfo,
        local_device: &DeviceInfo,
        socket: &UdpSocket,
        multicast_addr: SocketAddr,
        run: &DiscoveryRun,
        #[cfg(feature = "https")] client_certificate: Option<crate::crypto::TlsCertificate>,
    ) {
        tracing::debug!(
            "Responding to announcement from {} ({:?})",
            target_device.alias,
            target_device.ip
        );

        // The discovery announcement contains the peer's certificate fingerprint.
        // Use it for HTTPS registration instead of system CA verification.
        #[cfg(feature = "https")]
        let client_result = Self::client_for_announcement(
            local_device.clone(),
            target_device,
            client_certificate.as_ref(),
        );
        #[cfg(not(feature = "https"))]
        let client_result = Self::client_for_announcement(local_device.clone(), target_device);

        match client_result {
            Ok(client) => match tokio::time::timeout(
                ANNOUNCEMENT_REGISTER_TIMEOUT,
                client.register(target_device),
            )
            .await
            {
                Ok(Ok(_)) => {
                    tracing::debug!(
                        "Successfully registered with {} via HTTP",
                        target_device.alias
                    );
                    return;
                }
                Ok(Err(error)) => {
                    // If HTTP failed, fall back to UDP only while this run is
                    // active. A failed registration completing during shutdown
                    // must not announce an identity from the stopped run.
                    tracing::debug!(
                        "HTTP registration failed ({}), falling back to UDP...",
                        error
                    );
                }
                Err(_) => {
                    tracing::debug!(
                        "HTTP registration with {} timed out, falling back to UDP...",
                        target_device.alias
                    );
                }
            },
            Err(error) => {
                tracing::debug!(
                    "Could not configure pinned registration ({}), falling back to UDP...",
                    error
                );
            }
        }

        if !run.running.load(Ordering::Acquire) {
            return;
        }

        // Fallback: Send UDP response.
        let announcement = AnnouncementMessage {
            alias: local_device.alias.clone(),
            version: local_device.version.clone(),
            device_model: local_device.device_model.clone(),
            device_type: local_device.device_type,
            fingerprint: local_device.fingerprint.clone(),
            port: local_device.port,
            protocol: local_device.protocol,
            download: local_device.download,
            announce: false,
            announcement: Some(false),
        };

        if let Ok(msg) = serde_json::to_string(&announcement) {
            let buf = msg.as_bytes();
            if let Err(e) = socket.send_to(buf, multicast_addr).await {
                tracing::debug!("Failed to send UDP fallback response: {}", e);
            } else {
                tracing::debug!("Sent UDP fallback response to multicast group");
            }
        }
    }
}

/// Creates a UDP socket with port reuse enabled.
///
/// This is critical for LocalSend discovery because:
/// 1. The protocol uses a fixed multicast port (53317).
/// 2. Multiple instances (e.g., a background receiver and a short-lived discovery command)
///    need to join the same multicast group simultaneously.
///
/// By enabling SO_REUSEADDR (and SO_REUSEPORT on Unix), the OS allows multiple
/// processes to bind to the same UDP port. For multicast traffic, the OS will
/// clone incoming packets and deliver them to all participating sockets.
fn create_reusable_udp_socket(
    bind_addr: &SocketAddr,
    interface: Ipv4Addr,
    multicast_addr: Ipv4Addr,
) -> Result<UdpSocket> {
    let domain = if bind_addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };

    let socket = Socket::new(domain, Type::DGRAM, Some(SocketProtocol::UDP))
        .map_err(|e| LocalSendError::network(format!("Failed to create socket: {}", e)))?;

    // Enable address reuse (supported on most platforms including Windows).
    socket
        .set_reuse_address(true)
        .map_err(|e| LocalSendError::network(format!("Failed to set reuse_address: {}", e)))?;

    // Enable port reuse on Unix platforms to allow multiple processes to bind exactly to the same port.
    #[cfg(all(unix, not(target_os = "solaris"), not(target_os = "illumos")))]
    socket
        .set_reuse_port(true)
        .map_err(|e| LocalSendError::network(format!("Failed to set reuse_port: {}", e)))?;

    socket
        .bind(&(*bind_addr).into())
        .map_err(|e| LocalSendError::network(format!("Failed to bind to {}: {}", bind_addr, e)))?;

    socket
        .join_multicast_v4(&multicast_addr, &interface)
        .map_err(|error| {
            LocalSendError::network(format!("Failed to join multicast on {interface}: {error}"))
        })?;
    socket.set_multicast_if_v4(&interface).map_err(|error| {
        LocalSendError::network(format!(
            "Failed to select multicast interface {interface}: {error}"
        ))
    })?;

    // Convert to tokio UdpSocket after configuring the multicast interface.
    let std_socket: std::net::UdpSocket = socket.into();
    std_socket
        .set_nonblocking(true)
        .map_err(|e| LocalSendError::network(format!("Failed to set non-blocking: {}", e)))?;

    UdpSocket::from_std(std_socket)
        .map_err(|e| LocalSendError::network(format!("Failed to convert to tokio socket: {}", e)))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod lifecycle_tests;
