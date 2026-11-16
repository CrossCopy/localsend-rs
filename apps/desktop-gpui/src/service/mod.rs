//! The app's network service: one tokio task that owns the receiver, discovery
//! and every transfer in flight.
//!
//! Nothing in this module knows about GPUI. The window sends [`Command`]s and
//! reads [`ServiceEvent`]s; that split is what lets the whole transfer path be
//! tested headlessly (`tests/service_e2e.rs`) instead of only through a window.
//!
//! # Why the service owns the library objects
//!
//! `LocalSendServer`, `MulticastDiscovery` and `LocalSendClient` are tokio-native:
//! they bind sockets, spawn tasks and hold `oneshot` responders. The UI thread runs
//! GPUI's executor and cannot drive them. So the app owns one multi-thread runtime
//! (`crate::bridge::HostRuntime`) and this service runs on it, with `smol` channels
//! on the seam because `smol`'s channels are runtime-agnostic — the UI can await
//! one inside `cx.spawn` without pulling tokio's reactor onto the main thread.
//!
//! # Offers are answered by id, not by handle
//!
//! A `PendingRequest` must be consumed exactly once and does not belong to a view
//! that may be re-rendered and dropped. The service keeps them in `pending` keyed
//! by a [`TransferId`]; the window answers `Accept { id }` / `Decline { id }`, and a
//! request the window never answers is declined when the server stops, so a sender
//! is never left waiting on an accept timeout that belongs to us.

pub mod controller;
pub mod format;
pub mod settings;

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use thiserror::Error;

pub use settings::{Settings, DEFAULT_PORT, DEFAULT_SURFACE_OPACITY, MAX_SURFACE_OPACITY};

use localsend_rs::{DeviceInfo, DeviceType, Protocol};

/// Why the service cannot currently promise update-safe quiescence.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum UpdateQuiescenceError {
    #[error("a transfer is pending or active")]
    Busy,
    #[error("update quiescence has not been confirmed")]
    NotConfirmed,
    #[error("the network service is currently restarting")]
    RestartInProgress,
    #[error("the network service is not available")]
    Unavailable,
}

pub(crate) enum ControllerCommand {
    User(Command),
    Prepare {
        response: tokio::sync::oneshot::Sender<Result<(), UpdateQuiescenceError>>,
        restart_in_progress: bool,
    },
    Finalize(tokio::sync::oneshot::Sender<Result<(), UpdateQuiescenceError>>),
    Cancel(tokio::sync::oneshot::Sender<Result<(), UpdateQuiescenceError>>),
}

/// A transfer's identity for as long as this process runs. Offers, live progress
/// and history rows all key on it.
pub type TransferId = u64;

/// A peer on the local network, as the window needs to draw it.
///
/// `DeviceInfo` is the wire type and stays in the service layer; this is the
/// copy that survives a missed announcement and carries the moment we last saw
/// the peer, which is what makes a stale row explainable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    /// The fingerprint, which is the peer's identity across address changes.
    pub node_id: String,
    pub alias: String,
    pub model: String,
    pub device_kind: PeerKind,
    pub ip: String,
    pub port: u16,
    pub protocol: Protocol,
    pub last_seen: SystemTime,
}

/// A currently available, non-loopback IPv4 interface that can source a subnet scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkInterface {
    pub name: String,
    pub address: Ipv4Addr,
}

/// What glyph a peer gets. The protocol only distinguishes five device types, and
/// the window cares about three shapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerKind {
    Phone,
    Computer,
    Server,
}

impl Peer {
    /// A peer we can actually talk to needs an address; one that arrived without
    /// an `ip` (a malformed announcement) is not a device the user can send to.
    pub fn from_device(device: &DeviceInfo) -> Option<Self> {
        Self::from_device_at(device, SystemTime::now())
    }

    /// Convert a device using the time its response was actually observed.
    /// Subnet scans may finish after a newer multicast announcement arrives.
    pub fn from_device_at(device: &DeviceInfo, last_seen: SystemTime) -> Option<Self> {
        let ip = device.ip.clone()?;
        if device.fingerprint.is_empty() || device.port == 0 {
            return None;
        }
        Some(Self {
            node_id: device.fingerprint.clone(),
            alias: device.alias.clone(),
            model: device.device_model.clone().unwrap_or_default(),
            device_kind: match device.device_type.unwrap_or(DeviceType::Desktop) {
                DeviceType::Mobile => PeerKind::Phone,
                DeviceType::Web => PeerKind::Computer,
                DeviceType::Desktop => PeerKind::Computer,
                DeviceType::Headless | DeviceType::Server => PeerKind::Server,
            },
            ip,
            port: device.port,
            protocol: device.protocol,
            last_seen,
        })
    }

    /// The same, for a device that did not announce an address.
    ///
    /// A sender's `info` block may omit `ip` — this app's own client does, and the
    /// protocol does not require it. An *offer* does not need an address (the
    /// answer goes back over the connection the sender already holds), so the
    /// missing field must not turn a real transfer into a silent decline. The
    /// address is only used to draw the row.
    pub fn from_device_lenient(device: &DeviceInfo) -> Option<Self> {
        let mut peer = Self::from_device(device).or_else(|| {
            if device.fingerprint.is_empty() || device.port == 0 {
                return None;
            }
            Some(Peer {
                node_id: device.fingerprint.clone(),
                alias: device.alias.clone(),
                model: device.device_model.clone().unwrap_or_default(),
                device_kind: match device.device_type.unwrap_or(DeviceType::Desktop) {
                    DeviceType::Mobile => PeerKind::Phone,
                    DeviceType::Web | DeviceType::Desktop => PeerKind::Computer,
                    DeviceType::Headless | DeviceType::Server => PeerKind::Server,
                },
                ip: String::new(),
                port: device.port,
                protocol: device.protocol,
                last_seen: SystemTime::now(),
            })
        })?;
        if peer.ip.is_empty() {
            peer.ip = "unknown".to_string();
        }
        Some(peer)
    }

    /// The wire identity a client talks to: address, port, protocol and the
    /// fingerprint HTTPS pins against.
    pub fn to_device(&self) -> DeviceInfo {
        DeviceInfo {
            alias: self.alias.clone(),
            version: localsend_rs::PROTOCOL_VERSION.to_string(),
            device_model: Some(self.model.clone()),
            device_type: None,
            fingerprint: self.node_id.clone(),
            port: self.port,
            protocol: self.protocol,
            download: false,
            ip: Some(self.ip.clone()),
        }
    }

    /// `192.168.1.24:53317 · HTTPS`
    pub fn endpoint(&self) -> String {
        format!(
            "{}:{} · {}",
            self.ip,
            self.port,
            self.protocol.as_str().to_uppercase()
        )
    }
}

/// One file inside an offer, before anything has been written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfferFile {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub mime: String,
}

/// A sender's `prepare-upload` waiting for this window's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub id: TransferId,
    pub from: Peer,
    pub files: Vec<OfferFile>,
    pub total_bytes: u64,
}

impl Offer {
    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}

/// A file the user asked to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendFile {
    pub name: String,
    pub size: u64,
    pub path: PathBuf,
}

/// The receiver's state machine, as far as the window is concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerStatus {
    Starting,
    Listening {
        port: u16,
        alias: String,
        protocol: Protocol,
        fingerprint: String,
        save_dir: PathBuf,
    },
    Failed {
        message: String,
    },
    Stopped,
}

impl ServerStatus {
    pub fn is_listening(&self) -> bool {
        matches!(self, Self::Listening { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    /// Something the app worked around: worth reading, not an error.
    Warning,
    Error,
}

/// How a transfer ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferOutcome {
    Completed,
    Failed { message: String },
    Cancelled,
}

/// Everything the service tells the window. One enum so a view has one place to
/// look, and one exhaustive `match` in the session state machine.
#[derive(Clone, Debug, PartialEq)]
pub enum ServiceEvent {
    Status(ServerStatus),
    /// The latest snapshot of non-loopback IPv4 interfaces.
    NetworkInterfacesUpdated(Vec<NetworkInterface>),
    /// A selected-interface subnet scan began.
    SubnetScanStarted {
        address: Ipv4Addr,
    },
    /// A subnet scan finished. `complete` is false when its bounded deadline
    /// arrived; `found` counts peers reported through `PeerFound`.
    SubnetScanFinished {
        address: Ipv4Addr,
        complete: bool,
        found: usize,
    },
    /// Another scan is already consuming the bounded scan slot.
    SubnetScanBusy {
        address: Ipv4Addr,
    },
    /// A scan was rejected or interrupted before it could finish.
    SubnetScanFailed {
        address: Ipv4Addr,
        message: String,
    },
    /// Interface enumeration failed. The current interface snapshot is retained.
    NetworkInterfacesFailed {
        message: String,
    },
    /// A peer was seen — by multicast, or by its HTTP reply to our announcement.
    PeerFound(Peer),
    /// A sender is waiting for an answer.
    Offer(Offer),
    /// An offer from a trusted device was taken without a prompt. The window is
    /// still told, so a quick save is visible rather than silent.
    OfferAcceptedAutomatically(Offer),
    /// The offer no longer needs an answer (accepted here, or it went away).
    OfferClosed {
        id: TransferId,
        accepted: bool,
    },
    ReceiveProgress {
        id: TransferId,
        sender: String,
        file_name: String,
        file_index: usize,
        file_count: usize,
        file_done: u64,
        file_size: u64,
        session_done: u64,
        session_total: u64,
    },
    Received {
        id: TransferId,
        sender: String,
        file_name: String,
        path: PathBuf,
        size: u64,
    },
    ReceivedText {
        sender: String,
        text: String,
    },
    ReceiveFinished {
        id: TransferId,
        outcome: TransferOutcome,
    },
    SendStarted {
        id: TransferId,
        peer_alias: String,
        files: Vec<SendFile>,
        total_bytes: u64,
    },
    SendProgress {
        id: TransferId,
        file_name: String,
        file_index: usize,
        file_count: usize,
        file_done: u64,
        file_size: u64,
        session_done: u64,
        session_total: u64,
    },
    SendFinished {
        id: TransferId,
        outcome: TransferOutcome,
    },
    Notice {
        level: NoticeLevel,
        message: String,
    },
}

/// What the window can ask the service to do.
///
/// `Clone` is for the tests: the store can be built without a service and record
/// the commands it would have sent, and a test reads those back.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Accept every offered file, or only the named ones.
    Accept {
        id: TransferId,
        files: Option<Vec<String>>,
    },
    Decline {
        id: TransferId,
    },
    /// Announce ourselves again and wait for answers.
    RefreshPeers,
    /// Re-enumerate currently available local IPv4 interfaces.
    RefreshInterfaces,
    /// Sweep the selected interface's `/24` subnet.
    ScanSubnet {
        address: Ipv4Addr,
    },
    Send {
        peer: Peer,
        files: Vec<PathBuf>,
        pin: Option<String>,
    },
    CancelSend {
        id: TransferId,
    },
    /// Flip the receiver's auto-accept live; no restart involved.
    SetAutoAccept(bool),
    /// Replace the trusted-device list live; no restart involved. Starring a
    /// device must take effect without dropping the listener.
    SetFavorites(Vec<String>),
    /// Rebuild the receiver with new settings (name, port, folder, PIN, TLS).
    Restart(Box<Settings>),
}

/// The window's end of the service: a command sender and the event stream.
///
/// Dropping it drops the command sender, which ends the controller task and
/// stops the listener — the process exiting is not the only way to shut down.
pub struct ServiceHandle {
    commands: tokio::sync::mpsc::UnboundedSender<ControllerCommand>,
    update_intent: Arc<AtomicBool>,
    restart_in_progress: Arc<AtomicBool>,
    events: smol::channel::Receiver<ServiceEvent>,
}

impl ServiceHandle {
    /// Start the controller on `runtime`. Must be called from a context that can
    /// spawn onto it (the UI thread holds a `Handle`, not an `EnterGuard`).
    pub fn spawn(runtime: &tokio::runtime::Handle, settings: Settings) -> Self {
        let (commands, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, events) = smol::channel::unbounded();
        let update_intent = Arc::new(AtomicBool::new(false));
        let restart_in_progress = Arc::new(AtomicBool::new(false));
        runtime.spawn(controller::run(
            settings,
            command_rx,
            event_tx,
            update_intent.clone(),
            restart_in_progress.clone(),
        ));
        Self {
            commands,
            update_intent,
            restart_in_progress,
            events,
        }
    }

    /// Send a command. `false` means the service is gone and the window is
    /// showing state nobody is updating — worth telling the user about.
    pub fn command(&self, command: Command) -> bool {
        self.commands.send(ControllerCommand::User(command)).is_ok()
    }

    /// The event stream, for the store's pump task.
    pub fn events(&self) -> &smol::channel::Receiver<ServiceEvent> {
        &self.events
    }

    /// Close incoming and outgoing transfer admission after the caller has
    /// received explicit install confirmation.
    ///
    /// A busy result keeps the barrier closed so existing work can drain; retry
    /// this method to re-check or call [`Self::cancel_update_quiescence`] if the
    /// update is abandoned.
    pub fn prepare_update_quiescence(
        &self,
    ) -> impl std::future::Future<Output = Result<(), UpdateQuiescenceError>> {
        self.update_intent.store(true, Ordering::Release);
        let restart_in_progress = self.restart_in_progress.load(Ordering::Acquire);
        let (response, result) = tokio::sync::oneshot::channel();
        let sent = self.commands.send(ControllerCommand::Prepare {
            response,
            restart_in_progress,
        });
        async move {
            if sent.is_err() {
                return Err(UpdateQuiescenceError::Unavailable);
            }
            result
                .await
                .unwrap_or(Err(UpdateQuiescenceError::Unavailable))
        }
    }

    /// Stop discovery and release the listener after quiescence was confirmed.
    ///
    /// Success is sent only after the discovery stop request has been issued
    /// and the server has stopped and released its listener. This is a separate
    /// step because launching the replacement process while the old listener is
    /// still bound can make the replacement choose a fallback port.
    ///
    /// The current discovery trait exposes only synchronous `stop`, not a task
    /// join handle. This acknowledgement waits for that stop call and the
    /// listener shutdown; it cannot prove the detached multicast receive task
    /// has exited.
    pub fn finalize_update_stop(
        &self,
    ) -> impl std::future::Future<Output = Result<(), UpdateQuiescenceError>> {
        let (response, result) = tokio::sync::oneshot::channel();
        let sent = self.commands.send(ControllerCommand::Finalize(response));
        async move {
            sent.map_err(|_| UpdateQuiescenceError::Unavailable)?;
            result
                .await
                .unwrap_or(Err(UpdateQuiescenceError::Unavailable))
        }
    }

    /// Re-open transfer admission after an update attempt is cancelled or fails.
    ///
    /// If [`Self::finalize_update_stop`] already stopped the listener, this
    /// starts the service again with its current settings and waits for startup.
    pub fn cancel_update_quiescence(
        &self,
    ) -> impl std::future::Future<Output = Result<(), UpdateQuiescenceError>> {
        self.update_intent.store(false, Ordering::Release);
        let (response, result) = tokio::sync::oneshot::channel();
        let sent = self.commands.send(ControllerCommand::Cancel(response));
        async move {
            sent.map_err(|_| UpdateQuiescenceError::Unavailable)?;
            result
                .await
                .unwrap_or(Err(UpdateQuiescenceError::Unavailable))
        }
    }
}

/// Helper for the controller: `try_send` into the UI channel, ignoring a closed
/// receiver (the window is gone) and a full queue (a wedged UI thread must never
/// stall an upload).
pub(crate) fn emit(events: &smol::channel::Sender<ServiceEvent>, event: ServiceEvent) {
    let _ = events.try_send(event);
}

/// The identity this process presents to peers. Reuses the server's own device
/// record when it is listening, so a send and the discovery announcement name the
/// same device; otherwise it falls back to the certificate's fingerprint (a
/// stable identity across restarts) or a fresh random one.
pub(crate) fn local_identity(
    settings: &Settings,
    certificate: Option<&localsend_rs::TlsCertificate>,
    listening: Option<&DeviceInfo>,
) -> DeviceInfo {
    let mut device = match listening {
        Some(device) => device.clone(),
        None => {
            let protocol = if settings.https {
                Protocol::Https
            } else {
                Protocol::Http
            };
            let fingerprint = certificate
                .map(|certificate| certificate.fingerprint.clone())
                .unwrap_or_else(localsend_rs::generate_fingerprint);
            DeviceInfo {
                alias: settings.alias.clone(),
                version: localsend_rs::PROTOCOL_VERSION.to_string(),
                device_model: Some(localsend_rs::get_device_model()),
                device_type: Some(localsend_rs::get_device_type()),
                fingerprint,
                port: settings.port,
                protocol,
                download: false,
                ip: None,
            }
        }
    };
    // A receiver identifies the sender of a `prepare-upload` by this block. Our
    // own listener reports no `ip` (the server binds to 0.0.0.0 and cannot know
    // which interface a peer would use), so fill it in: the same machine must be
    // able to send to itself, and another instance of this app must not silently
    // drop our offer for a field the protocol made optional.
    if device.ip.is_none() {
        device.ip = localsend_rs::get_local_ip()
            .ok()
            .map(|address| address.to_string());
    }
    device
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(alias: &str, fingerprint: &str, ip: Option<&str>) -> DeviceInfo {
        DeviceInfo {
            alias: alias.to_string(),
            version: "2.1".to_string(),
            device_model: Some("iPhone 17".to_string()),
            device_type: Some(DeviceType::Mobile),
            fingerprint: fingerprint.to_string(),
            port: 53317,
            protocol: Protocol::Https,
            download: false,
            ip: ip.map(str::to_string),
        }
    }

    #[test]
    fn a_peer_without_an_address_is_not_sendable() {
        assert!(Peer::from_device(&device("iPhone", "abc", None)).is_none());
        assert!(Peer::from_device(&device("iPhone", "", Some("10.0.0.2"))).is_none());
        assert!(Peer::from_device(&device("iPhone", "abc", Some("10.0.0.2"))).is_some());
    }

    #[test]
    fn device_kinds_map_to_three_shapes() {
        let kind = |kind: DeviceType| {
            let mut device = device("x", "f", Some("10.0.0.2"));
            device.device_type = Some(kind);
            Peer::from_device(&device).unwrap().device_kind
        };
        assert_eq!(kind(DeviceType::Mobile), PeerKind::Phone);
        assert_eq!(kind(DeviceType::Desktop), PeerKind::Computer);
        assert_eq!(kind(DeviceType::Web), PeerKind::Computer);
        assert_eq!(kind(DeviceType::Server), PeerKind::Server);
        assert_eq!(kind(DeviceType::Headless), PeerKind::Server);
    }

    #[test]
    fn an_offer_from_a_sender_without_an_address_is_still_an_offer() {
        let peer = Peer::from_device_lenient(&device("iPhone", "fingerprint", None)).unwrap();
        assert_eq!(peer.node_id, "fingerprint");
        assert_eq!(peer.port, 53317);
        // A missing ip is unknown, not a reason to drop the transfer.
        assert_eq!(peer.ip, "unknown");
        assert!(Peer::from_device(&device("iPhone", "fingerprint", None)).is_none());
    }

    #[test]
    fn a_peer_round_trips_to_the_wire_identity_with_its_pin_target() {
        let peer = Peer::from_device(&device("iPhone", "fingerprint", Some("10.0.0.2"))).unwrap();
        let wire = peer.to_device();
        assert_eq!(wire.fingerprint, "fingerprint");
        assert_eq!(wire.ip.as_deref(), Some("10.0.0.2"));
        assert_eq!(wire.port, 53317);
        assert_eq!(wire.protocol, Protocol::Https);
        assert_eq!(peer.endpoint(), "10.0.0.2:53317 · HTTPS");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prepare_refuses_a_restart_already_in_progress() {
        let temp = tempfile::tempdir().unwrap();
        let settings = Settings {
            save_dir: temp.path().to_path_buf(),
            port: 0,
            https: false,
            ..Settings::default()
        };
        let handle = ServiceHandle::spawn(&tokio::runtime::Handle::current(), settings);

        handle
            .restart_in_progress
            .store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(
            handle.prepare_update_quiescence().await,
            Err(UpdateQuiescenceError::RestartInProgress)
        );
        assert!(
            !handle
                .update_intent
                .load(std::sync::atomic::Ordering::Acquire),
            "a refused preparation must not leave normal service commands frozen"
        );
        handle
            .restart_in_progress
            .store(false, std::sync::atomic::Ordering::Release);
    }
}
