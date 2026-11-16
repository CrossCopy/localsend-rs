//! The controller task: it owns the receiver, discovery and every transfer, and
//! turns library events into the window's [`ServiceEvent`] stream.
//!
//! # One owner, one thread
//!
//! All mutable state here is touched by exactly one task. Commands arrive on a
//! `tokio::mpsc` channel, server events arrive through a pump task that forwards
//! them onto the same select loop, and send tasks report completion the same way.
//! That is why there are no locks around `pending`, `sends` or `receive_sessions`:
//! a shared map would be a second owner for state that must have exactly one.
//!
//! # The announcement doubles as a peer list
//!
//! A LocalSend peer answers our multicast announcement with an HTTP `/register`,
//! which arrives as [`ServerEvent::PeerRegistered`] — so the same action that makes
//! us visible also fills the device list. Multicast answers feed the same list
//! through the discovery callback; both go out as [`ServiceEvent::PeerFound`] and
//! the session state machine deduplicates by fingerprint.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use localsend_rs::client::client::ProgressCallback;
use localsend_rs::{
    build_file_metadata, local_ipv4_interfaces, DeviceInfo, Discovery, FileId, HttpDiscovery,
    LocalSendClient, LocalSendError, LocalSendServer, MulticastDiscovery, Protocol, ScanOutcome,
    SessionId, TlsCertificate, TlsTrustPolicy,
};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use super::{
    emit, local_identity, Command, ControllerCommand, NetworkInterface, NoticeLevel, Offer,
    OfferFile, Peer, SendFile, ServerStatus, ServiceEvent, Settings, TransferId, TransferOutcome,
    UpdateQuiescenceError,
};
use localsend_rs::server::{PendingRequest, ServerEvent};

/// Re-announce this often. Peers prune devices that go quiet, and a laptop that
/// wakes from sleep should not need the user to press "refresh" to be found.
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(20);

/// Progress callbacks fire per network chunk. At a few hundred MB/s that is
/// thousands of events per second for a single file, all for a bar that can only
/// show sixty frames — so the task reports at most this often.
const PROGRESS_INTERVAL_MS: u64 = 40;

/// One scan at a time keeps rapid clicks from multiplying the bounded per-scan
/// HTTP concurrency.
const SUBNET_SCAN_DEADLINE: Duration = Duration::from_secs(10);

/// Messages that come from something other than the window.
enum Internal {
    /// A receiver event, forwarded by the pump task of the current server.
    Server(ServerEvent),
    /// A send task reached its terminal outcome and can no longer issue network
    /// requests. The controller removes it before publishing completion.
    SendDone(TransferId, TransferOutcome),
    /// A bounded interface scan finished off-controller.
    SubnetScanDone {
        scan_id: u64,
        address: Ipv4Addr,
        result: Result<ScanOutcome, String>,
    },
}

struct ActiveSubnetScan {
    id: u64,
    task: tokio::task::JoinHandle<()>,
}

type NetworkInterfaceProvider =
    Arc<dyn Fn() -> Result<Vec<NetworkInterface>, String> + Send + Sync>;

struct SendTask {
    abort: AbortHandle,
    peer: Peer,
    /// Filled in once `prepare-upload` answered, so a cancel can tell the peer
    /// which session it is abandoning. Empty while the offer is still unanswered.
    session: Arc<Mutex<Option<SessionId>>>,
    /// Linearizes completion against a concurrent user cancellation.
    terminal_claimed: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpdatePhase {
    Running,
    Draining,
    Quiescent,
    StoppedForUpdate,
}

pub(crate) async fn run(
    settings: Settings,
    mut commands: mpsc::UnboundedReceiver<ControllerCommand>,
    events: smol::channel::Sender<ServiceEvent>,
    update_intent: Arc<std::sync::atomic::AtomicBool>,
    restart_in_progress: Arc<std::sync::atomic::AtomicBool>,
) {
    let (internal_tx, mut internal_rx) = mpsc::unbounded_channel();
    let mut controller = Controller {
        settings,
        events,
        internal_tx,
        pending: HashMap::new(),
        receive_sessions: HashMap::new(),
        receive_files: HashMap::new(),
        sends: HashMap::new(),
        server: None,
        identity: None,
        discovery: None,
        certificate: None,
        network_interfaces: Vec::new(),
        interface_provider: Arc::new(system_network_interfaces),
        subnet_scans: HashMap::new(),
        next_id: 1,
        next_scan_id: 1,
        discovery_failure_reported: false,
        update_phase: UpdatePhase::Running,
        receive_admission_closed: false,
        update_intent,
        restart_in_progress,
    };
    controller.start().await;

    let mut ticker = tokio::time::interval(ANNOUNCE_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick resolves immediately; the start-up announcement already
    // happened, so consume it instead of announcing twice.
    ticker.tick().await;

    loop {
        tokio::select! {
            biased;
            command = commands.recv() => match command {
                // The window dropped its handle: the process is shutting down.
                None => break,
                Some(ControllerCommand::User(command)) => controller.handle(command).await,
                Some(ControllerCommand::Prepare {
                    response,
                    restart_in_progress,
                }) => {
                    let result = controller
                        .prepare_update_quiescence(restart_in_progress)
                        .await;
                    if response.send(result).is_err() {
                        let _ = controller.cancel_update_quiescence().await;
                    }
                }
                Some(ControllerCommand::Finalize(response)) => {
                    let result = controller.finalize_update_stop().await;
                    if response.send(result).is_err() && result.is_ok() {
                        let _ = controller.cancel_update_quiescence().await;
                    }
                }
                Some(ControllerCommand::Cancel(response)) => {
                    let result = controller.cancel_update_quiescence().await;
                    let _ = response.send(result);
                }
            },
            internal = internal_rx.recv() => match internal {
                None => break,
                Some(Internal::Server(event)) => controller.on_server_event(event),
                Some(Internal::SendDone(id, outcome)) => {
                    controller.sends.remove(&id);
                    controller.emit(ServiceEvent::SendFinished { id, outcome });
                }
                Some(Internal::SubnetScanDone {
                    scan_id,
                    address,
                    result,
                }) => controller
                    .on_subnet_scan_done(scan_id, address, result)
                    .await,
            },
            _ = ticker.tick() => controller.announce().await,
        }
    }

    controller.shutdown().await;
}

struct Controller {
    settings: Settings,
    events: smol::channel::Sender<ServiceEvent>,
    internal_tx: mpsc::UnboundedSender<Internal>,
    pending: HashMap<TransferId, PendingRequest>,
    receive_sessions: HashMap<String, TransferId>,
    /// File names per active receive, in the order they first reported, so a
    /// progress line can say "2 of 3" without the wire event carrying an index.
    receive_files: HashMap<TransferId, Vec<String>>,
    sends: HashMap<TransferId, SendTask>,
    server: Option<LocalSendServer>,
    identity: Option<DeviceInfo>,
    discovery: Option<MulticastDiscovery>,
    certificate: Option<TlsCertificate>,
    network_interfaces: Vec<NetworkInterface>,
    interface_provider: NetworkInterfaceProvider,
    subnet_scans: HashMap<Ipv4Addr, ActiveSubnetScan>,
    next_id: TransferId,
    next_scan_id: u64,
    discovery_failure_reported: bool,
    update_phase: UpdatePhase,
    receive_admission_closed: bool,
    update_intent: Arc<std::sync::atomic::AtomicBool>,
    restart_in_progress: Arc<std::sync::atomic::AtomicBool>,
}

impl Controller {
    fn protocol(&self) -> Protocol {
        if self.settings.https {
            Protocol::Https
        } else {
            Protocol::Http
        }
    }

    fn emit(&self, event: ServiceEvent) {
        emit(&self.events, event);
    }

    fn notice(&self, level: NoticeLevel, message: impl Into<String>) {
        self.emit(ServiceEvent::Notice {
            level,
            message: message.into(),
        });
    }

    fn next_id(&mut self) -> TransferId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    // ---------------------------------------------------------------- lifecycle

    async fn start(&mut self) {
        self.emit(ServiceEvent::Status(ServerStatus::Starting));
        self.refresh_network_interfaces().await;

        if let Err(error) = tokio::fs::create_dir_all(&self.settings.save_dir).await {
            self.notice(
                NoticeLevel::Error,
                format!(
                    "Cannot create the save folder {}: {error}",
                    self.settings.save_dir.display()
                ),
            );
        }

        self.certificate = if self.settings.https {
            load_certificate().await
        } else {
            None
        };

        match self.listen().await {
            Ok((server, device)) => {
                self.identity = Some(device.clone());
                let status = ServerStatus::Listening {
                    port: device.port,
                    alias: device.alias.clone(),
                    protocol: device.protocol,
                    fingerprint: device.fingerprint.clone(),
                    save_dir: self.settings.save_dir.clone(),
                };
                self.server = Some(server);
                self.emit(ServiceEvent::Status(status));
                self.start_discovery(device).await;
            }
            Err(message) => {
                // Without a listener there is nothing to announce: a device that
                // shows up in every peer list and then refuses the connection is
                // worse than one that stays quiet until the user fixes the port.
                self.emit(ServiceEvent::Status(ServerStatus::Failed {
                    message: message.clone(),
                }));
                self.notice(NoticeLevel::Error, message);
            }
        }
    }

    /// Bind the receiver, stepping aside if the port is taken.
    ///
    /// Running the official LocalSend app next to this one is a normal thing to
    /// do, and both want 53317. Rather than failing to start — which leaves the
    /// app looking broken and unable to send either — we take an OS-assigned
    /// port and say so. Peers still find us, because the multicast announcement
    /// carries whatever port we ended up on.
    async fn listen(&mut self) -> Result<(LocalSendServer, DeviceInfo), String> {
        match self.build_server(self.settings.port).await {
            Ok(pair) => Ok(pair),
            Err(BindFailure::InUse) if self.settings.port != 0 => {
                let requested = self.settings.port;
                let (server, device) = self
                    .build_server(0)
                    .await
                    .map_err(|failure| failure.describe(&self.settings))?;
                self.notice(
                    NoticeLevel::Warning,
                    format!(
                        "Port {requested} is already in use — another LocalSend app is probably \
                         running. Listening on port {} instead; nearby devices can still find \
                         this one. Turn the other app off and press Apply to get {requested} back.",
                        device.port
                    ),
                );
                Ok((server, device))
            }
            Err(failure) => Err(failure.describe(&self.settings)),
        }
    }

    async fn build_server(
        &mut self,
        port: u16,
    ) -> Result<(LocalSendServer, DeviceInfo), BindFailure> {
        let mut builder = LocalSendServer::builder()
            .alias(self.settings.alias.clone())
            .port(port)
            .save_dir(self.settings.save_dir.clone())
            .protocol(self.protocol())
            .auto_accept(self.settings.auto_accept);
        if let Some(pin) = self.settings.pin.clone() {
            builder = builder.pin(pin);
        }
        if let Some(certificate) = self.certificate.clone() {
            builder = builder.tls_certificate(certificate);
        }

        let (server, mut server_events) = builder.build().await.map_err(BindFailure::from_error)?;

        // Forward this server's events onto the controller loop. The pump dies
        // with the receiver when the server is replaced, which is why a restart
        // cannot deliver one server's offer to the next one's session table.
        let internal = self.internal_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = server_events.recv().await {
                if internal.send(Internal::Server(event)).is_err() {
                    break;
                }
            }
        });

        let device = server.device().clone();
        Ok((server, device))
    }

    async fn start_discovery(&mut self, device: DeviceInfo) {
        let mut discovery = MulticastDiscovery::new_with_device(device.clone());
        if let Some(certificate) = self.certificate.clone() {
            discovery.set_client_certificate(certificate);
        }

        let events = self.events.clone();
        let own_fingerprint = device.fingerprint.clone();
        discovery.on_discovered(move |found| {
            if found.fingerprint == own_fingerprint {
                return;
            }
            if let Some(peer) = Peer::from_device(&found) {
                emit(&events, ServiceEvent::PeerFound(peer));
            }
        });

        match Discovery::start(&mut discovery).await {
            Ok(()) => {
                let _ = discovery.announce_presence().await;
                self.discovery = Some(discovery);
            }
            Err(error) => {
                if !self.discovery_failure_reported {
                    self.discovery_failure_reported = true;
                    self.notice(
                        NoticeLevel::Error,
                        format!(
                            "Local discovery is unavailable ({error}). This device still answers peers \
                             that contact it directly."
                        ),
                    );
                }
            }
        }
    }

    async fn announce(&mut self) {
        if self.server.is_none() {
            return;
        }
        if self.discovery.is_none() {
            // A network that came up after launch (Wi-Fi joined, cable plugged in)
            // is a second chance, not a failure to report twice.
            if let Some(device) = self.identity.clone() {
                self.start_discovery(device).await;
            }
            return;
        }
        if let Some(discovery) = &self.discovery {
            let _ = discovery.announce_presence().await;
        }
    }

    /// Replace the interface snapshot from the operating system. Scans whose
    /// source address disappeared are cancelled and reported rather than left
    /// showing as active indefinitely.
    async fn refresh_network_interfaces(&mut self) {
        match (self.interface_provider)() {
            Ok(interfaces) => {
                self.network_interfaces = interfaces;
                self.emit(ServiceEvent::NetworkInterfacesUpdated(
                    self.network_interfaces.clone(),
                ));
                self.cancel_disappeared_scans().await;
            }
            Err(error) => self.emit(ServiceEvent::NetworkInterfacesFailed {
                message: error.to_string(),
            }),
        }
    }

    async fn scan_selected_interface(&mut self, address: Ipv4Addr) {
        // The command payload is not a capability: check against a fresh system
        // snapshot before binding a client socket or probing the subnet.
        let current_interfaces = match (self.interface_provider)() {
            Ok(interfaces) => interfaces,
            Err(error) => {
                let message = error;
                self.emit(ServiceEvent::NetworkInterfacesFailed {
                    message: message.clone(),
                });
                self.emit(ServiceEvent::SubnetScanFailed { address, message });
                return;
            }
        };
        self.network_interfaces = current_interfaces;
        self.emit(ServiceEvent::NetworkInterfacesUpdated(
            self.network_interfaces.clone(),
        ));
        self.cancel_disappeared_scans().await;

        if !self
            .network_interfaces
            .iter()
            .any(|interface| interface.address == address)
        {
            self.emit(ServiceEvent::SubnetScanFailed {
                address,
                message: "the selected address is not an active local IPv4 interface".to_string(),
            });
            return;
        }
        if !self.subnet_scans.is_empty() {
            self.emit(ServiceEvent::SubnetScanBusy { address });
            return;
        }
        let Some(identity) = self.identity.clone() else {
            self.emit(ServiceEvent::SubnetScanFailed {
                address,
                message: "the LocalSend receiver is not listening".to_string(),
            });
            return;
        };

        let discovery = match (identity.protocol, self.certificate.as_ref()) {
            (Protocol::Https, Some(certificate)) => {
                HttpDiscovery::for_device_on_interface_with_client_certificate(
                    identity.clone(),
                    address,
                    certificate,
                )
            }
            _ => HttpDiscovery::for_device_on_interface(identity.clone(), address),
        };
        let discovery = match discovery {
            Ok(discovery) => discovery,
            Err(error) => {
                self.emit(ServiceEvent::SubnetScanFailed {
                    address,
                    message: error.to_string(),
                });
                return;
            }
        };

        let scan_id = self.next_scan_id;
        self.next_scan_id += 1;
        // Sweep the configured LocalSend port, not the receiver's ephemeral
        // fallback port. The device identity above retains the receiver's real
        // fingerprint and advertised identity.
        let target_port = self.settings.port;
        self.emit(ServiceEvent::SubnetScanStarted { address });
        let internal = self.internal_tx.clone();
        let task = tokio::spawn(async move {
            let result = discovery
                .scan_subnet_on_port_within(&address.to_string(), target_port, SUBNET_SCAN_DEADLINE)
                .await
                .map_err(|error| error.to_string());
            let _ = internal.send(Internal::SubnetScanDone {
                scan_id,
                address,
                result,
            });
        });
        self.subnet_scans
            .insert(address, ActiveSubnetScan { id: scan_id, task });
    }

    async fn cancel_disappeared_scans(&mut self) {
        let disappeared = self
            .subnet_scans
            .keys()
            .copied()
            .filter(|address| {
                !self
                    .network_interfaces
                    .iter()
                    .any(|interface| interface.address == *address)
            })
            .collect::<Vec<_>>();
        for address in disappeared {
            if let Some(scan) = self.subnet_scans.remove(&address) {
                scan.task.abort();
                let _ = scan.task.await;
                self.emit(ServiceEvent::SubnetScanFailed {
                    address,
                    message: "the selected network interface is no longer available".to_string(),
                });
            }
        }
    }

    async fn on_subnet_scan_done(
        &mut self,
        scan_id: u64,
        address: Ipv4Addr,
        result: Result<ScanOutcome, String>,
    ) {
        let Some(scan) = self.subnet_scans.get(&address) else {
            return;
        };
        if scan.id != scan_id {
            return;
        }
        self.subnet_scans.remove(&address);

        let current_interfaces = match (self.interface_provider)() {
            Ok(interfaces) => interfaces,
            Err(message) => {
                self.emit(ServiceEvent::NetworkInterfacesFailed {
                    message: message.clone(),
                });
                self.emit(ServiceEvent::SubnetScanFailed { address, message });
                return;
            }
        };
        self.network_interfaces = current_interfaces;
        self.emit(ServiceEvent::NetworkInterfacesUpdated(
            self.network_interfaces.clone(),
        ));
        self.cancel_disappeared_scans().await;
        if !self
            .network_interfaces
            .iter()
            .any(|interface| interface.address == address)
        {
            self.emit(ServiceEvent::SubnetScanFailed {
                address,
                message: "the selected network interface is no longer available".to_string(),
            });
            return;
        }

        match result {
            Ok(outcome) => {
                let found = outcome.devices.len();
                for observed in outcome.devices {
                    if let Some(peer) = Peer::from_device_at(&observed.device, observed.observed_at)
                    {
                        self.emit(ServiceEvent::PeerFound(peer));
                    }
                }
                self.emit(ServiceEvent::SubnetScanFinished {
                    address,
                    complete: outcome.complete,
                    found,
                });
            }
            Err(message) => self.emit(ServiceEvent::SubnetScanFailed { address, message }),
        }
    }

    async fn handle(&mut self, command: Command) {
        match command {
            Command::Accept { id, files } => {
                let Some(request) = self.pending.remove(&id) else {
                    return;
                };
                match files {
                    Some(ids) if ids.is_empty() => {
                        request.decline();
                        self.emit(ServiceEvent::OfferClosed {
                            id,
                            accepted: false,
                        });
                    }
                    Some(ids) => {
                        request.accept_files(ids.into_iter().map(FileId::from_string).collect());
                        self.emit(ServiceEvent::OfferClosed { id, accepted: true });
                    }
                    None => {
                        request.accept();
                        self.emit(ServiceEvent::OfferClosed { id, accepted: true });
                    }
                }
            }
            Command::Decline { id } => {
                if let Some(request) = self.pending.remove(&id) {
                    request.decline();
                    self.emit(ServiceEvent::OfferClosed {
                        id,
                        accepted: false,
                    });
                }
            }
            Command::RefreshPeers => self.announce().await,
            Command::RefreshInterfaces => self.refresh_network_interfaces().await,
            Command::ScanSubnet { address } => self.scan_selected_interface(address).await,
            Command::Send { peer, files, pin } => self.start_send(peer, files, pin).await,
            Command::CancelSend { id } => self.cancel_send(id).await,
            Command::SetAutoAccept(yes) => {
                self.settings.auto_accept = yes;
                if let Some(server) = &self.server {
                    server.set_auto_accept(yes);
                }
            }
            Command::SetFavorites(favorites) => {
                self.settings.favorites = favorites;
                self.settings.normalize_in_place();
            }
            Command::Restart(settings) => {
                if self.update_phase != UpdatePhase::Running
                    || self.update_intent.load(Ordering::Acquire)
                {
                    self.notice(
                        NoticeLevel::Warning,
                        "Cancel the update attempt before restarting the network service.",
                    );
                    return;
                }
                self.restart_in_progress.store(true, Ordering::Release);
                if self.update_intent.load(Ordering::Acquire) {
                    self.restart_in_progress.store(false, Ordering::Release);
                    self.notice(
                        NoticeLevel::Warning,
                        "Cancel the update attempt before restarting the network service.",
                    );
                    return;
                }
                self.stop_listener().await;
                self.settings = *settings;
                self.start().await;
                self.restart_in_progress.store(false, Ordering::Release);
            }
        }
    }

    /// Tear the listener down. Offers still waiting for an answer are declined
    /// here: a restart is this side's decision, and the sender should not sit
    /// through an accept timeout that will never be met.
    async fn stop_listener(&mut self) {
        self.cancel_subnet_scans_and_wait("the network service stopped before the scan finished")
            .await;
        for (_, request) in std::mem::take(&mut self.pending) {
            request.decline();
        }
        self.receive_sessions.clear();
        self.receive_files.clear();
        if let Some(mut discovery) = self.discovery.take() {
            Discovery::stop(&mut discovery);
        }
        if let Some(mut server) = self.server.take() {
            server.stop().await;
        }
        self.identity = None;
        self.emit(ServiceEvent::Status(ServerStatus::Stopped));
    }

    async fn shutdown(&mut self) {
        for (_, task) in self.sends.drain() {
            task.abort.abort();
        }
        self.stop_listener().await;
    }

    async fn prepare_update_quiescence(
        &mut self,
        restart_was_in_progress: bool,
    ) -> Result<(), UpdateQuiescenceError> {
        if restart_was_in_progress {
            self.update_intent.store(false, Ordering::Release);
            return Err(UpdateQuiescenceError::RestartInProgress);
        }
        if self.update_phase == UpdatePhase::StoppedForUpdate {
            return Ok(());
        }
        let Some(server) = self.server.as_ref() else {
            self.update_intent.store(false, Ordering::Release);
            return Err(UpdateQuiescenceError::Unavailable);
        };
        self.update_phase = UpdatePhase::Draining;
        self.receive_admission_closed = false;
        match server.prepare_update_quiescence().await {
            Ok(()) => self.receive_admission_closed = true,
            Err(localsend_rs::server::ServerQuiescenceError::Busy) => {
                return Err(UpdateQuiescenceError::Busy);
            }
            Err(localsend_rs::server::ServerQuiescenceError::NotRunning) => {
                self.update_phase = UpdatePhase::Running;
                self.update_intent.store(false, Ordering::Release);
                return Err(UpdateQuiescenceError::Unavailable);
            }
            Err(localsend_rs::server::ServerQuiescenceError::NotConfirmed) => {
                self.update_phase = UpdatePhase::Running;
                self.update_intent.store(false, Ordering::Release);
                return Err(UpdateQuiescenceError::NotConfirmed);
            }
        }
        if !self.sends.is_empty() {
            return Err(UpdateQuiescenceError::Busy);
        }
        for (_, request) in std::mem::take(&mut self.pending) {
            request.decline();
        }
        self.update_phase = UpdatePhase::Quiescent;
        Ok(())
    }

    async fn finalize_update_stop(&mut self) -> Result<(), UpdateQuiescenceError> {
        match self.update_phase {
            UpdatePhase::StoppedForUpdate => return Ok(()),
            UpdatePhase::Quiescent => {}
            UpdatePhase::Running | UpdatePhase::Draining => {
                return Err(UpdateQuiescenceError::NotConfirmed);
            }
        }
        if !self.sends.is_empty() {
            return Err(UpdateQuiescenceError::Busy);
        }

        self.cancel_subnet_scans_and_wait("the service is stopping for an update")
            .await;

        let Some(server) = self.server.as_mut() else {
            return Err(UpdateQuiescenceError::Unavailable);
        };
        match server.stop_for_update().await {
            Ok(()) => {}
            Err(localsend_rs::server::ServerQuiescenceError::Busy) => {
                return Err(UpdateQuiescenceError::Busy);
            }
            Err(localsend_rs::server::ServerQuiescenceError::NotConfirmed) => {
                return Err(UpdateQuiescenceError::NotConfirmed);
            }
            Err(localsend_rs::server::ServerQuiescenceError::NotRunning) => {
                return Err(UpdateQuiescenceError::Unavailable);
            }
        }
        if let Some(discovery) = self.discovery.take() {
            discovery.stop_and_wait().await;
        }
        self.server = None;
        self.identity = None;
        self.receive_sessions.clear();
        self.receive_files.clear();
        self.emit(ServiceEvent::Status(ServerStatus::Stopped));
        self.update_phase = UpdatePhase::StoppedForUpdate;
        self.receive_admission_closed = true;
        Ok(())
    }

    async fn cancel_subnet_scans_and_wait(&mut self, message: &str) {
        let scans = std::mem::take(&mut self.subnet_scans);
        for scan in scans.values() {
            scan.task.abort();
        }
        for (address, scan) in scans {
            let _ = scan.task.await;
            self.emit(ServiceEvent::SubnetScanFailed {
                address,
                message: message.to_string(),
            });
        }
    }

    async fn cancel_update_quiescence(&mut self) -> Result<(), UpdateQuiescenceError> {
        match self.update_phase {
            UpdatePhase::Running => return Ok(()),
            UpdatePhase::StoppedForUpdate => {
                self.start().await;
                if self.server.is_none() {
                    return Err(UpdateQuiescenceError::Unavailable);
                }
            }
            UpdatePhase::Draining | UpdatePhase::Quiescent => {
                let Some(server) = self.server.as_ref() else {
                    return Err(UpdateQuiescenceError::Unavailable);
                };
                match server.cancel_update_quiescence().await {
                    Ok(()) => {}
                    Err(localsend_rs::server::ServerQuiescenceError::NotRunning) => {
                        return Err(UpdateQuiescenceError::Unavailable);
                    }
                    Err(localsend_rs::server::ServerQuiescenceError::NotConfirmed) => {
                        return Err(UpdateQuiescenceError::NotConfirmed);
                    }
                    Err(localsend_rs::server::ServerQuiescenceError::Busy) => {
                        unreachable!("cancelling an update barrier cannot report a busy server")
                    }
                }
            }
        }
        self.update_phase = UpdatePhase::Running;
        self.receive_admission_closed = false;
        self.update_intent.store(false, Ordering::Release);
        Ok(())
    }

    // ------------------------------------------------------------ receive side

    fn on_server_event(&mut self, event: ServerEvent) {
        match event {
            ServerEvent::PeerRegistered(device) => {
                let own = self
                    .identity
                    .as_ref()
                    .map(|identity| identity.fingerprint.as_str());
                match Peer::from_device(&device) {
                    Some(peer) if own != Some(peer.node_id.as_str()) => {
                        self.emit(ServiceEvent::PeerFound(peer));
                    }
                    _ => {}
                }
            }
            ServerEvent::TransferRequest(request) => {
                if self.receive_admission_closed {
                    request.decline();
                    return;
                }
                let Some(from) = Peer::from_device_lenient(request.sender()) else {
                    // No address means no reply is possible; declining is the
                    // honest answer rather than leaving the sender hanging.
                    request.decline();
                    return;
                };
                let mut files: Vec<OfferFile> = request
                    .files()
                    .values()
                    .map(|metadata| OfferFile {
                        id: metadata.id.as_str().to_string(),
                        name: metadata.file_name.clone(),
                        size: metadata.size,
                        mime: metadata.file_type.clone(),
                    })
                    .collect();
                // HashMap order is not an order; sort so the prompt does not
                // shuffle its file list between two renders.
                files.sort_by(|left, right| left.name.cmp(&right.name));
                let total_bytes = files.iter().map(|file| file.size).sum();

                // Quick save. The narrow form applies to favorites only, so a
                // phone the user starred can drop files without a prompt while a
                // stranger still gets one. Either way the transfer is announced:
                // the window must be able to show what arrived unasked.
                if self.settings.auto_accepts_from(&from.node_id) {
                    request.accept();
                    let id = self.next_id();
                    self.emit(ServiceEvent::OfferAcceptedAutomatically(Offer {
                        id,
                        from,
                        files,
                        total_bytes,
                    }));
                    return;
                }

                let id = self.next_id();
                self.pending.insert(id, request);
                self.emit(ServiceEvent::Offer(Offer {
                    id,
                    from,
                    files,
                    total_bytes,
                }));
            }
            ServerEvent::FileReceiveProgress {
                session_id,
                file_name,
                sender_alias,
                bytes_received,
                total_bytes,
                file_bytes_received,
                file_size,
                file_count,
                ..
            } => {
                let id = self.receive_transfer(session_id.as_str(), &sender_alias);
                let names = self.receive_files.entry(id).or_default();
                if !names.iter().any(|name| name == &file_name) {
                    names.push(file_name.clone());
                }
                let file_index = names
                    .iter()
                    .position(|name| name == &file_name)
                    .map(|index| index + 1)
                    .unwrap_or(1);
                self.emit(ServiceEvent::ReceiveProgress {
                    id,
                    sender: sender_alias,
                    file_name,
                    file_index,
                    file_count,
                    file_done: file_bytes_received,
                    file_size,
                    session_done: bytes_received,
                    session_total: total_bytes,
                });
            }
            ServerEvent::FileReceived {
                session_id,
                file_name,
                path,
                size,
                sender_alias,
                ..
            } => {
                let id = self.receive_transfer(session_id.as_str(), &sender_alias);
                self.emit(ServiceEvent::Received {
                    id,
                    sender: sender_alias,
                    file_name,
                    path,
                    size,
                });
            }
            ServerEvent::TextReceived {
                sender_alias, text, ..
            } => {
                self.emit(ServiceEvent::ReceivedText {
                    sender: sender_alias,
                    text,
                });
            }
            ServerEvent::SessionDone { session_id } => {
                if let Some(id) = self.receive_sessions.remove(session_id.as_str()) {
                    self.receive_files.remove(&id);
                    self.emit(ServiceEvent::ReceiveFinished {
                        id,
                        outcome: TransferOutcome::Completed,
                    });
                }
            }
            // The browser share page is a separate feature this shell does not
            // expose; its events carry nothing the window could draw.
            ServerEvent::WebShareRequest(_)
            | ServerEvent::WebShareDownloadProgress { .. }
            | ServerEvent::WebShareSessionDone { .. } => {}
        }
    }

    fn receive_transfer(&mut self, session: &str, sender: &str) -> TransferId {
        if let Some(id) = self.receive_sessions.get(session) {
            return *id;
        }
        let id = self.next_id();
        self.receive_sessions.insert(session.to_string(), id);
        // The first progress event creates the row; the session state machine
        // needs no separate "started" event because every field it draws with is
        // already in this one.
        let _ = sender;
        id
    }

    // --------------------------------------------------------------- send side

    async fn start_send(&mut self, peer: Peer, files: Vec<PathBuf>, pin: Option<String>) {
        if self.update_phase != UpdatePhase::Running || self.update_intent.load(Ordering::Acquire) {
            self.notice(
                NoticeLevel::Warning,
                "Sending is paused while an update is being prepared.",
            );
            return;
        }
        if files.is_empty() {
            self.notice(NoticeLevel::Error, "Choose at least one file first.");
            return;
        }

        let mut plans = Vec::with_capacity(files.len());
        for path in &files {
            match std::fs::metadata(path) {
                Ok(metadata) if metadata.is_file() => plans.push(SendFile {
                    name: file_name(path),
                    size: metadata.len(),
                    path: path.clone(),
                }),
                Ok(_) => {
                    self.notice(
                        NoticeLevel::Error,
                        format!("{} is a folder, not a file.", path.display()),
                    );
                    return;
                }
                Err(error) => {
                    self.notice(
                        NoticeLevel::Error,
                        format!("{} cannot be read: {error}", path.display()),
                    );
                    return;
                }
            }
        }
        let total_bytes = plans.iter().map(|plan| plan.size).sum();
        let id = self.next_id();
        self.emit(ServiceEvent::SendStarted {
            id,
            peer_alias: peer.alias.clone(),
            files: plans.clone(),
            total_bytes,
        });

        let identity = local_identity(
            &self.settings,
            self.certificate.as_ref(),
            self.identity.as_ref(),
        );
        let client = match client_for(&identity, &peer, self.certificate.as_ref()) {
            Ok(client) => client,
            Err(error) => {
                self.emit(ServiceEvent::SendFinished {
                    id,
                    outcome: TransferOutcome::Failed {
                        message: error.to_string(),
                    },
                });
                return;
            }
        };

        let session = Arc::new(Mutex::new(None));
        let terminal_claimed = Arc::new(AtomicBool::new(false));
        let events = self.events.clone();
        let internal = self.internal_tx.clone();
        let slot = session.clone();
        let task_terminal_claimed = terminal_claimed.clone();
        // The task needs the peer to address the upload; the controller keeps its
        // own copy so a cancel can still talk to the same device.
        let task_peer = peer.clone();
        let task = tokio::spawn(async move {
            let outcome = send_all(id, &client, &task_peer, &plans, &pin, &events, &slot).await;
            if !task_terminal_claimed.swap(true, Ordering::AcqRel) {
                let _ = internal.send(Internal::SendDone(id, outcome));
            }
        });

        self.sends.insert(
            id,
            SendTask {
                abort: task.abort_handle(),
                peer,
                session,
                terminal_claimed,
            },
        );
    }

    async fn cancel_send(&mut self, id: TransferId) {
        let Some(task) = self.sends.get(&id) else {
            return;
        };
        if task.terminal_claimed.swap(true, Ordering::AcqRel) {
            // Completion already won; its queued SendDone removes the task and
            // publishes the real result instead of a contradictory cancellation.
            return;
        }
        let Some(task) = self.sends.remove(&id) else {
            return;
        };
        // Abort the upload task first: a task that is mid-body is holding the
        // connection, and telling the peer "cancelled" before the body stops
        // would race the two.
        task.abort.abort();
        let session = task.session.lock().ok().and_then(|slot| slot.clone());
        if let Some(session) = session {
            let identity = local_identity(
                &self.settings,
                self.certificate.as_ref(),
                self.identity.as_ref(),
            );
            if let Ok(client) = client_for(&identity, &task.peer, self.certificate.as_ref()) {
                let _ = client.cancel(&task.peer.to_device(), &session).await;
            }
        }
        self.emit(ServiceEvent::SendFinished {
            id,
            outcome: TransferOutcome::Cancelled,
        });
    }
}

async fn load_certificate() -> Option<TlsCertificate> {
    match tokio::task::spawn_blocking(localsend_rs::load_or_generate_default_tls_certificate).await
    {
        Ok(Ok(certificate)) => Some(certificate),
        Ok(Err(error)) => {
            eprintln!("localsend-gpui: could not load the TLS identity: {error}");
            None
        }
        Err(error) => {
            eprintln!("localsend-gpui: TLS identity task failed: {error}");
            None
        }
    }
}

/// The client that talks to `peer`. HTTPS pins the peer's announced fingerprint —
/// the only trust anchor this app has, exactly as the CLI does — and presents our
/// own certificate when the peer asks for one.
pub(crate) fn client_for(
    identity: &DeviceInfo,
    peer: &Peer,
    certificate: Option<&TlsCertificate>,
) -> localsend_rs::Result<LocalSendClient> {
    if peer.protocol == Protocol::Https {
        let policy = TlsTrustPolicy::new([peer.node_id.clone()]);
        match certificate {
            Some(certificate) => LocalSendClient::with_trust_policy_and_client_certificate(
                identity.clone(),
                policy,
                certificate,
            ),
            None => LocalSendClient::with_trust_policy(identity.clone(), policy),
        }
    } else {
        Ok(LocalSendClient::new(identity.clone()))
    }
}

/// The whole send half for one batch: build metadata, prepare, upload in order,
/// reporting progress as it goes. A failure stops the batch and says which file
/// it was — a partial transfer that claims success is the one outcome nobody can
/// act on.
async fn send_all(
    id: TransferId,
    client: &LocalSendClient,
    peer: &Peer,
    plans: &[SendFile],
    pin: &Option<String>,
    events: &smol::channel::Sender<ServiceEvent>,
    session: &Arc<Mutex<Option<SessionId>>>,
) -> TransferOutcome {
    let target = peer.to_device();

    // Registration is a courtesy, not a prerequisite: `prepare-upload` carries
    // the same identity. A peer that is in our list from a multicast answer may
    // never have heard of us, so this is worth one attempt.
    let _ = client.register(&target).await;

    let mut metadata = HashMap::with_capacity(plans.len());
    let mut order = Vec::with_capacity(plans.len());
    for plan in plans {
        match build_file_metadata(&plan.path).await {
            Ok(meta) => {
                order.push((meta.id.clone(), plan.clone()));
                metadata.insert(meta.id.clone(), meta);
            }
            Err(error) => {
                return TransferOutcome::Failed {
                    message: format!("{} could not be read: {error}", plan.name),
                };
            }
        }
    }

    let prepared = match client
        .prepare_upload(&target, metadata, pin.as_deref())
        .await
    {
        Ok(prepared) => prepared,
        Err(error) => {
            return TransferOutcome::Failed {
                message: describe_prepare_failure(pin, error),
            };
        }
    };

    // 204: the receiver took everything it needed from the offer itself (the
    // text-message path). Nothing to upload, and nothing failed.
    if prepared.session_id.as_str().is_empty() {
        return TransferOutcome::Completed;
    }
    if let Ok(mut slot) = session.lock() {
        *slot = Some(prepared.session_id.clone());
    }

    let file_count = order.len();
    let total_bytes: u64 = plans.iter().map(|plan| plan.size).sum();
    let mut session_done: u64 = 0;

    for (index, (file_id, plan)) in order.iter().enumerate() {
        // The receiver may accept a subset; a file it did not take has no token
        // and is not ours to upload.
        let Some(token) = prepared.files.get(file_id) else {
            continue;
        };
        let base = session_done;
        let events = events.clone();
        let last_emit = Arc::new(AtomicU64::new(0));
        let name = plan.name.clone();
        let progress: ProgressCallback = Box::new(move |sent, total, _elapsed| {
            let now = now_millis();
            let previous = last_emit.load(Ordering::Relaxed);
            if sent < total && now.saturating_sub(previous) < PROGRESS_INTERVAL_MS {
                return;
            }
            last_emit.store(now, Ordering::Relaxed);
            emit(
                &events,
                ServiceEvent::SendProgress {
                    id,
                    file_name: name.clone(),
                    file_index: index + 1,
                    file_count,
                    file_done: sent,
                    file_size: total,
                    session_done: base + sent,
                    session_total: total_bytes,
                },
            );
        });

        if let Err(error) = client
            .upload_file(
                &target,
                &prepared.session_id,
                file_id,
                token,
                &plan.path,
                Some(progress),
            )
            .await
        {
            return TransferOutcome::Failed {
                message: format!("{} did not arrive: {error}", plan.name),
            };
        }
        session_done += plan.size;
    }

    TransferOutcome::Completed
}

/// Why the receiver would not bind, in the two shapes that need different answers.
enum BindFailure {
    /// Somebody else holds the port — usually the official app, side by side.
    InUse,
    /// Anything else, already phrased for the notice banner.
    Other(String),
}

impl BindFailure {
    fn from_error(error: LocalSendError) -> Self {
        match error {
            LocalSendError::Io { source } if source.kind() == std::io::ErrorKind::AddrInUse => {
                Self::InUse
            }
            other => Self::Other(other.to_string()),
        }
    }

    /// The sentence to show a person. `InUse` only reaches here when there is no
    /// fallback left, i.e. the OS-assigned port was taken too.
    fn describe(&self, settings: &Settings) -> String {
        match self {
            Self::InUse => format!(
                "Ports {} and any free port are both unavailable — something else on this \
                 machine is holding them. Change the port in Settings and try again.",
                settings.port
            ),
            Self::Other(message) if message.to_lowercase().contains("permission denied") => {
                format!(
                    "Port {} cannot be opened (permission denied). Use a port above 1023.",
                    settings.port
                )
            }
            Self::Other(message) => format!("The receiver could not start: {message}"),
        }
    }
}

fn describe_prepare_failure(pin: &Option<String>, error: localsend_rs::LocalSendError) -> String {
    use localsend_rs::LocalSendError;
    match error {
        LocalSendError::InvalidPin => "That PIN was not accepted.".to_string(),
        LocalSendError::RateLimited => {
            "Too many PIN attempts — wait a few minutes before trying again.".to_string()
        }
        LocalSendError::Rejected { .. } => "The receiving device declined the files.".to_string(),
        LocalSendError::SessionBlocked => {
            "The receiving device is busy with another transfer.".to_string()
        }
        other => match pin {
            Some(_) => format!("The transfer could not start: {other}"),
            None => format!("The transfer could not start: {other} (does it need a PIN?)"),
        },
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn system_network_interfaces() -> Result<Vec<NetworkInterface>, String> {
    local_ipv4_interfaces()
        .map(|interfaces| {
            interfaces
                .into_iter()
                .map(|(name, address)| NetworkInterface { name, address })
                .collect()
        })
        .map_err(|error| error.to_string())
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::ServiceHandle;
    use localsend_rs::discovery::http::ObservedDevice;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(signal) = self.0.take() {
                let _ = signal.send(());
            }
        }
    }

    async fn next_event(events: &smol::channel::Receiver<ServiceEvent>) -> ServiceEvent {
        tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("service event timed out")
            .expect("service event stream closed")
    }

    fn controller_for_events(events: smol::channel::Sender<ServiceEvent>) -> Controller {
        let (internal_tx, _internal_rx) = mpsc::unbounded_channel();
        Controller {
            settings: Settings::default(),
            events,
            internal_tx,
            pending: HashMap::new(),
            receive_sessions: HashMap::new(),
            receive_files: HashMap::new(),
            sends: HashMap::new(),
            server: None,
            identity: None,
            discovery: None,
            certificate: None,
            network_interfaces: Vec::new(),
            interface_provider: Arc::new(|| {
                Ok(vec![NetworkInterface {
                    name: "test".to_string(),
                    address: Ipv4Addr::new(192, 168, 1, 10),
                }])
            }),
            subnet_scans: HashMap::new(),
            next_id: 1,
            next_scan_id: 1,
            discovery_failure_reported: false,
            update_phase: UpdatePhase::Running,
            receive_admission_closed: false,
            update_intent: Arc::new(AtomicBool::new(false)),
            restart_in_progress: Arc::new(AtomicBool::new(false)),
        }
    }

    #[tokio::test]
    async fn a_partial_scan_emits_peers_then_a_finished_event() {
        let (events_tx, events_rx) = smol::channel::unbounded();
        let mut controller = controller_for_events(events_tx);
        let address = Ipv4Addr::new(192, 168, 1, 10);
        let scan_task = tokio::spawn(std::future::pending::<()>());
        let scan_abort = scan_task.abort_handle();
        controller.subnet_scans.insert(
            address,
            ActiveSubnetScan {
                id: 7,
                task: scan_task,
            },
        );
        let mut device = DeviceInfo::new("LAN peer".to_string(), 53317, Protocol::Http);
        device.fingerprint = "lan-peer-fingerprint".to_string();
        device.ip = Some("192.168.1.24".to_string());

        controller
            .on_subnet_scan_done(
                7,
                address,
                Ok(ScanOutcome {
                    devices: vec![ObservedDevice {
                        device,
                        observed_at: SystemTime::UNIX_EPOCH + Duration::from_secs(7),
                    }],
                    complete: false,
                }),
            )
            .await;

        assert!(matches!(
            events_rx.try_recv().expect("interface refresh"),
            ServiceEvent::NetworkInterfacesUpdated(_)
        ));
        match events_rx.try_recv().expect("peer event") {
            ServiceEvent::PeerFound(peer) => {
                assert_eq!(peer.node_id, "lan-peer-fingerprint");
                assert_eq!(peer.ip, "192.168.1.24");
                assert_eq!(
                    peer.last_seen,
                    SystemTime::UNIX_EPOCH + Duration::from_secs(7)
                );
            }
            other => panic!("expected PeerFound before scan completion, got {other:?}"),
        }
        assert_eq!(
            events_rx.try_recv().expect("partial completion event"),
            ServiceEvent::SubnetScanFinished {
                address,
                complete: false,
                found: 1,
            }
        );
        assert!(!controller.subnet_scans.contains_key(&address));
        scan_abort.abort();
    }

    #[tokio::test]
    async fn a_scan_error_clears_its_running_state() {
        let (events_tx, events_rx) = smol::channel::unbounded();
        let mut controller = controller_for_events(events_tx);
        let address = Ipv4Addr::new(192, 168, 1, 10);
        let scan_task = tokio::spawn(std::future::pending::<()>());
        let scan_abort = scan_task.abort_handle();
        controller.subnet_scans.insert(
            address,
            ActiveSubnetScan {
                id: 8,
                task: scan_task,
            },
        );

        controller
            .on_subnet_scan_done(8, address, Err("probe setup failed".to_string()))
            .await;

        assert!(matches!(
            events_rx.try_recv().expect("interface refresh"),
            ServiceEvent::NetworkInterfacesUpdated(_)
        ));
        assert_eq!(
            events_rx.try_recv().expect("scan failure event"),
            ServiceEvent::SubnetScanFailed {
                address,
                message: "probe setup failed".to_string(),
            }
        );
        assert!(!controller.subnet_scans.contains_key(&address));
        scan_abort.abort();
    }

    #[tokio::test]
    async fn scan_completion_discards_results_after_its_source_interface_disappears() {
        let (events_tx, events_rx) = smol::channel::unbounded();
        let mut controller = controller_for_events(events_tx);
        let address = Ipv4Addr::new(192, 168, 1, 10);
        let interface_present = Arc::new(AtomicBool::new(true));
        let provider_state = interface_present.clone();
        controller.interface_provider = Arc::new(move || {
            Ok(if provider_state.load(Ordering::SeqCst) {
                vec![NetworkInterface {
                    name: "test".to_string(),
                    address,
                }]
            } else {
                Vec::new()
            })
        });
        controller.refresh_network_interfaces().await;
        assert_eq!(
            events_rx
                .try_recv()
                .expect("interface available at scan start"),
            ServiceEvent::NetworkInterfacesUpdated(vec![NetworkInterface {
                name: "test".to_string(),
                address,
            }])
        );
        let scan_task = tokio::spawn(std::future::pending::<()>());
        let scan_abort = scan_task.abort_handle();
        controller.subnet_scans.insert(
            address,
            ActiveSubnetScan {
                id: 9,
                task: scan_task,
            },
        );
        interface_present.store(false, Ordering::SeqCst);
        let mut device = DeviceInfo::new("disconnected peer".to_string(), 53317, Protocol::Http);
        device.fingerprint = "disconnected-peer-fingerprint".to_string();
        device.ip = Some("192.168.1.24".to_string());

        controller
            .on_subnet_scan_done(
                9,
                address,
                Ok(ScanOutcome {
                    devices: vec![ObservedDevice {
                        device,
                        observed_at: SystemTime::UNIX_EPOCH + Duration::from_secs(9),
                    }],
                    complete: true,
                }),
            )
            .await;

        assert_eq!(
            events_rx.try_recv().expect("fresh interface snapshot"),
            ServiceEvent::NetworkInterfacesUpdated(Vec::new())
        );
        assert!(matches!(
            events_rx.try_recv().expect("interface-disappeared error"),
            ServiceEvent::SubnetScanFailed {
                address: failed_address,
                ..
            } if failed_address == address
        ));
        assert!(matches!(
            events_rx.try_recv(),
            Err(smol::channel::TryRecvError::Empty)
        ));
        assert!(!controller.subnet_scans.contains_key(&address));
        scan_abort.abort();
    }

    #[tokio::test]
    async fn finalize_update_cancels_and_joins_scans_before_late_results_can_publish() {
        let output = tempfile::tempdir().expect("save directory");
        let (server, _server_events) = LocalSendServer::builder()
            .alias("scan-update-test")
            .port(0)
            .save_dir(output.path())
            .protocol(Protocol::Http)
            .build()
            .await
            .expect("start receiver");
        server
            .prepare_update_quiescence()
            .await
            .expect("prepare receiver for update");
        let (events_tx, events_rx) = smol::channel::unbounded();
        let mut controller = controller_for_events(events_tx);
        controller.server = Some(server);
        controller.update_phase = UpdatePhase::Quiescent;

        let address = Ipv4Addr::new(192, 168, 1, 10);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let scan_task = tokio::spawn(async move {
            let _drop_signal = DropSignal(Some(dropped_tx));
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        started_rx.await.expect("scan task started");
        let scan_abort = scan_task.abort_handle();
        controller.subnet_scans.insert(
            address,
            ActiveSubnetScan {
                id: 10,
                task: scan_task,
            },
        );

        controller
            .finalize_update_stop()
            .await
            .expect("stop the quiescent receiver");

        assert!(tokio::time::timeout(Duration::from_secs(1), dropped_rx)
            .await
            .expect("scan task was not cancelled")
            .is_ok());
        assert!(scan_abort.is_finished(), "the scan task must be joined");
        assert!(controller.subnet_scans.is_empty());

        let mut saw_scan_cleared = false;
        let mut saw_stopped = false;
        while let Ok(event) = events_rx.try_recv() {
            match event {
                ServiceEvent::SubnetScanFailed {
                    address: failed, ..
                } => {
                    assert_eq!(failed, address);
                    saw_scan_cleared = true;
                }
                ServiceEvent::Status(ServerStatus::Stopped) => saw_stopped = true,
                ServiceEvent::PeerFound(_) => panic!("finalized scan must not publish stale peers"),
                _ => {}
            }
        }
        assert!(saw_scan_cleared);
        assert!(saw_stopped);

        let replacement_task = tokio::spawn(std::future::pending::<()>());
        let replacement_abort = replacement_task.abort_handle();
        controller.subnet_scans.insert(
            address,
            ActiveSubnetScan {
                id: 11,
                task: replacement_task,
            },
        );
        let mut stale_device = DeviceInfo::new("stale peer".to_string(), 53317, Protocol::Http);
        stale_device.fingerprint = "stale-peer-fingerprint".to_string();
        stale_device.ip = Some("192.168.1.24".to_string());
        controller
            .on_subnet_scan_done(
                10,
                address,
                Ok(ScanOutcome {
                    devices: vec![ObservedDevice {
                        device: stale_device,
                        observed_at: SystemTime::UNIX_EPOCH + Duration::from_secs(10),
                    }],
                    complete: true,
                }),
            )
            .await;
        assert!(matches!(
            events_rx.try_recv(),
            Err(smol::channel::TryRecvError::Empty)
        ));
        assert_eq!(
            controller.subnet_scans.get(&address).map(|scan| scan.id),
            Some(11),
            "the old completion must not clear a new scan on the same address"
        );
        replacement_abort.abort();
    }

    #[tokio::test]
    async fn a_rapid_repeat_scan_is_reported_busy_without_starting_another() {
        let Some((_, address)) = local_ipv4_interfaces()
            .expect("list local interfaces")
            .into_iter()
            .next()
        else {
            // There is no interface address a real service could scan on this host.
            return;
        };
        let (events_tx, events_rx) = smol::channel::unbounded();
        let mut controller = controller_for_events(events_tx);
        controller.interface_provider = Arc::new(move || {
            Ok(vec![NetworkInterface {
                name: "test".to_string(),
                address,
            }])
        });
        let task = tokio::spawn(std::future::pending::<()>());
        let scan_abort = task.abort_handle();
        controller
            .subnet_scans
            .insert(address, ActiveSubnetScan { id: 1, task });

        controller.scan_selected_interface(address).await;

        assert!(matches!(
            events_rx.try_recv().expect("fresh interface snapshot"),
            ServiceEvent::NetworkInterfacesUpdated(_)
        ));
        assert_eq!(
            events_rx.try_recv().expect("busy scan event"),
            ServiceEvent::SubnetScanBusy { address }
        );
        assert!(controller.subnet_scans.contains_key(&address));
        scan_abort.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interface_refresh_and_rejected_subnet_scan_leave_receiver_running() {
        let output = tempfile::tempdir().expect("save directory");
        let handle = ServiceHandle::spawn(
            &tokio::runtime::Handle::current(),
            Settings {
                save_dir: output.path().to_path_buf(),
                port: 0,
                https: false,
                ..Settings::default()
            },
        );

        let mut initial_interfaces = false;
        let listening_port = loop {
            match next_event(handle.events()).await {
                ServiceEvent::NetworkInterfacesUpdated(interfaces) => {
                    initial_interfaces = true;
                    assert!(interfaces
                        .iter()
                        .all(|interface| !interface.address.is_loopback()));
                }
                ServiceEvent::Status(ServerStatus::Listening { port, .. }) => break port,
                ServiceEvent::Status(ServerStatus::Failed { message }) => {
                    panic!("receiver failed to start: {message}")
                }
                _ => {}
            }
        };
        assert!(
            initial_interfaces,
            "interfaces are published during startup"
        );

        assert!(handle.command(Command::RefreshInterfaces));
        loop {
            if let ServiceEvent::NetworkInterfacesUpdated(_) = next_event(handle.events()).await {
                break;
            }
        }

        assert!(handle.command(Command::ScanSubnet {
            address: Ipv4Addr::new(203, 0, 113, 77),
        }));
        let failure = loop {
            match next_event(handle.events()).await {
                ServiceEvent::SubnetScanFailed { address, message } => {
                    break (address, message);
                }
                ServiceEvent::SubnetScanStarted { .. } => {
                    panic!("a non-local address must never begin probing")
                }
                _ => {}
            }
        };
        assert_eq!(failure.0, Ipv4Addr::new(203, 0, 113, 77));
        assert!(failure.1.contains("not an active local IPv4 interface"));

        // The selected-interface commands neither restart the receiver nor
        // replace its ephemeral listener.
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", listening_port))
            .await
            .expect("the original receiver remains bound");
        stream
            .write_all(
                b"GET /api/localsend/v2/info HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write info request");
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
            .await
            .expect("receiver response timed out")
            .expect("read receiver response");
        assert!(
            String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200"),
            "receiver still answers on its original port"
        );
    }
}
