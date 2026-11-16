//! The LocalSend node an MCP session owns: a receiver, a discoverer and a client.
//!
//! An MCP server is a stdio child process of an AI client, so it cannot borrow
//! the window's network stack — and it should not: the window may not be running
//! at all. Instead each MCP session brings up its own node, named
//! `<device> (MCP)` with its own TLS identity, so it appears in other machines'
//! device lists as itself rather than impersonating the window.
//!
//! # Why the inbox is a journal
//!
//! See [`crate::mcp::inbox`]. The short version: the process dies when the AI
//! client closes the conversation, and "what did I receive?" must still have an
//! answer on the next one.
//!
//! # Receiving is a decision, and it is made explicit
//!
//! A node has no window to show an offer in, so the three modes are:
//!
//! * **auto-accept** (the default) — every offer is taken and journalled. This is
//!   the only mode in which an AI can be *notified* that a file arrived, because
//!   nothing needs a human in the loop.
//! * **ask** (`--ask`) — offers are held for [`OFFER_TIMEOUT`], listed by
//!   `list_offers`, and answered with `accept_offer` / `decline_offer`.
//! * **off** (`--no-receive`) — send and discover only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use localsend_rs::server::{PendingRequest, ServerEvent};
use localsend_rs::{
    build_file_metadata, DeviceInfo, Discovery, HttpDiscovery, LocalSendClient, LocalSendServer,
    MulticastDiscovery, Protocol, TlsCertificate,
};
use serde::Serialize;
use tokio::sync::{mpsc, Notify};

use super::inbox::{now_millis, Journal, ReceivedFile};
use crate::service::controller::client_for;
use crate::service::{Peer, Settings};

/// How long an offer waits for an answer in `--ask` mode. The library's default
/// is a window-sized pause; a model may need a round trip through a human, so it
/// gets five minutes.
pub const OFFER_TIMEOUT: Duration = Duration::from_secs(300);

/// Re-announce this often while the session lives. Peers prune devices that go
/// quiet, and an AI session can run for hours.
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(20);

/// How many entries stay in memory. The journal keeps everything; this only
/// bounds a running answer to "list what arrived".
const MAX_IN_MEMORY: usize = 1000;

/// The suffix that keeps this node distinguishable from the window on the same
/// machine. Without it a peer list shows two devices with one name, and picking
/// the wrong one sends a file into a process that is not listening.
pub const ALIAS_SUFFIX: &str = " (MCP)";

/// The protocol's own limit, in characters.
const ALIAS_LIMIT: usize = 32;

/// Everything an MCP session needs to come up.
#[derive(Clone, Debug, PartialEq)]
pub struct McpConfig {
    /// Alias, port, save folder, PIN and TLS scheme, shared with the window.
    pub settings: Settings,
    pub auto_accept: bool,
    pub receive: bool,
    /// Where the node keeps its own TLS identity and inbox journal. Separate from
    /// the window's, so the two are different devices to every peer.
    pub state_dir: PathBuf,
}

impl McpConfig {
    /// The defaults for a session: receive and auto-accept with the window's
    /// preferences, in a state directory beside the window's settings file.
    pub fn from_settings(settings: Settings) -> Self {
        Self {
            settings,
            auto_accept: true,
            receive: true,
            state_dir: default_state_dir(),
        }
    }

    /// `<alias> (MCP)`, trimmed to the protocol's 32-character limit.
    pub fn alias(&self) -> String {
        alias_with_suffix(&self.settings.alias)
    }

    /// This node's TLS identity: separate files from the window's, so the
    /// fingerprint differs and a peer that pinned the window does not confuse
    /// the two.
    pub fn identity_paths(&self) -> (PathBuf, PathBuf) {
        (
            self.state_dir.join("certificate.pem"),
            self.state_dir.join("private-key.pem"),
        )
    }

    pub fn journal_path(&self) -> PathBuf {
        self.state_dir.join("inbox.jsonl")
    }
}

/// The per-user application-support directory the window already uses.
pub fn default_state_dir() -> PathBuf {
    Settings::config_path()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .map(|dir| dir.join("mcp"))
        .unwrap_or_else(|| std::env::temp_dir().join("localsend-gpui-mcp"))
}

fn alias_with_suffix(alias: &str) -> String {
    let alias = alias.trim();
    let budget = ALIAS_LIMIT.saturating_sub(ALIAS_SUFFIX.chars().count());
    let trimmed: String = alias.chars().take(budget).collect();
    format!("{}{ALIAS_SUFFIX}", trimmed.trim_end())
}

/// One file that left this machine.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SentFile {
    pub name: String,
    pub path: String,
    pub size: u64,
}

/// The result of one `send_files` call, shaped for a model to read.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SendReport {
    pub device: String,
    pub endpoint: String,
    pub protocol: String,
    pub bytes: u64,
    pub files: Vec<SentFile>,
    /// `None` when everything arrived; otherwise the sentence explaining what did
    /// not, which is the part a caller can act on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SendReport {
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }
}

/// An offer held for an answer in `--ask` mode.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OfferSummary {
    pub id: u64,
    pub from: String,
    pub sender_id: String,
    pub files: Vec<String>,
    pub bytes: u64,
    pub expires_in_ms: u64,
}

/// What the node is, for a caller that wants to know what it is talking to.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StatusReport {
    pub alias: String,
    pub port: u16,
    pub protocol: String,
    pub fingerprint: String,
    pub listening: bool,
    pub auto_accept: bool,
    pub save_dir: String,
    pub journal: String,
    pub devices: usize,
    pub offers: usize,
    pub received: usize,
    pub pid: u32,
}

struct Held {
    request: PendingRequest,
    summary: OfferSummary,
    expires: Instant,
}

/// The state the pump task owns and every tool reads. One `Mutex` with short
/// critical sections: nothing here is held across an await, and nothing in it is
/// expensive enough to need a finer split.
struct Shared {
    alias: String,
    port: u16,
    protocol: Protocol,
    fingerprint: String,
    listening: bool,
    auto_accept: bool,
    save_dir: PathBuf,
    peers: HashMap<String, Peer>,
    inbox: Vec<ReceivedFile>,
    offers: HashMap<u64, Held>,
    next_offer: u64,
    /// Fired once per journalled entry; `wait_for_received` and the MCP resource
    /// notification both hang off it.
    new_file: Arc<Notify>,
    /// Fired when an offer arrives in `--ask` mode.
    new_offer: Arc<Notify>,
}

impl Shared {
    fn identity(&self) -> DeviceInfo {
        DeviceInfo {
            alias: self.alias.clone(),
            version: localsend_rs::PROTOCOL_VERSION.to_string(),
            device_model: Some(localsend_rs::get_device_model()),
            device_type: Some(localsend_rs::get_device_type()),
            fingerprint: self.fingerprint.clone(),
            port: self.port,
            protocol: self.protocol,
            download: false,
            ip: None,
        }
    }
}

/// The node.
pub struct Node {
    shared: Arc<Mutex<Shared>>,
    journal: Journal,
    certificate: Option<TlsCertificate>,
    settings: Settings,
    announce: mpsc::UnboundedSender<()>,
    listening: bool,
}

impl Node {
    /// Bring the node up. On return the receiver is listening (when asked to) and
    /// discovery has started; the pump task keeps both alive for as long as the
    /// process runs.
    pub async fn start(config: McpConfig) -> Result<Arc<Self>, String> {
        std::fs::create_dir_all(&config.settings.save_dir).map_err(|error| {
            format!(
                "cannot create the receive folder {}: {error}",
                config.settings.save_dir.display()
            )
        })?;
        std::fs::create_dir_all(&config.state_dir).map_err(|error| {
            format!(
                "cannot create the MCP state folder {}: {error}",
                config.state_dir.display()
            )
        })?;

        // The aliased name is what we announce, what the server reports and what
        // a send presents; deciding it once keeps all three agreeing.
        let mut settings = config.settings.clone();
        settings.alias = config.alias();

        let protocol = if settings.https {
            Protocol::Https
        } else {
            Protocol::Http
        };
        let certificate = if protocol == Protocol::Https {
            load_certificate(&config).await
        } else {
            None
        };

        let journal = Journal::new(config.journal_path());
        let mut inbox = journal.read();
        if inbox.len() > MAX_IN_MEMORY {
            inbox.drain(..inbox.len() - MAX_IN_MEMORY);
        }

        let shared = Arc::new(Mutex::new(Shared {
            alias: settings.alias.clone(),
            port: settings.port,
            protocol,
            fingerprint: String::new(),
            listening: false,
            auto_accept: config.auto_accept,
            save_dir: settings.save_dir.clone(),
            peers: HashMap::new(),
            inbox,
            offers: HashMap::new(),
            next_offer: 1,
            new_file: Arc::new(Notify::new()),
            new_offer: Arc::new(Notify::new()),
        }));

        // The receiver. A port conflict is not fatal here either: the window, the
        // official app and this node can all be up at once, and the multicast
        // announcement carries whichever port we ended up on.
        let mut server: Option<LocalSendServer> = None;
        let mut events: Option<mpsc::Receiver<ServerEvent>> = None;

        if config.receive {
            let (bound, server_events) = build_server(&settings, &certificate, &config).await?;
            let device = bound.device().clone();
            {
                let mut state = lock(&shared);
                state.port = device.port;
                state.fingerprint = device.fingerprint.clone();
                state.listening = true;
            }
            server = Some(bound);
            events = Some(server_events);
        } else {
            let device = crate::service::local_identity(&settings, certificate.as_ref(), None);
            let mut state = lock(&shared);
            state.fingerprint = device.fingerprint.clone();
            state.port = settings.port;
        }

        let device = lock(&shared).identity();
        let discovery = start_discovery(&device, certificate.as_ref(), &shared).await;

        let (announce, announce_rx) = mpsc::unbounded_channel();
        let node = Arc::new(Self {
            shared: shared.clone(),
            journal: journal.clone(),
            certificate,
            settings,
            announce,
            listening: server.is_some(),
        });

        // The pump owns the server: dropping it would stop the listener, so it
        // travels with the task that consumes its events.
        tokio::spawn(pump(
            shared,
            journal,
            server,
            events,
            announce_rx,
            discovery,
            config.auto_accept,
        ));

        Ok(node)
    }

    fn state(&self) -> MutexGuard<'_, Shared> {
        lock(&self.shared)
    }

    /// The devices seen so far, sorted so two answers to one question agree.
    pub fn devices(&self) -> Vec<Peer> {
        let mut peers: Vec<Peer> = self.state().peers.values().cloned().collect();
        peers.sort_by(|left, right| {
            left.alias
                .to_lowercase()
                .cmp(&right.alias.to_lowercase())
                .then_with(|| left.node_id.cmp(&right.node_id))
        });
        peers
    }

    /// Announce and give the network `wait` to answer. The answers arrive through
    /// the same pump as everything else, so this is a sleep, not a collection.
    pub async fn discover(&self, wait: Duration) -> Vec<Peer> {
        let _ = self.announce.send(());
        tokio::time::sleep(wait).await;
        self.devices()
    }

    /// Resolve a target a model was given: an alias, a fingerprint, an
    /// `address:port`, or an address already in the device list.
    pub async fn resolve(&self, target: &str) -> Result<Peer, String> {
        let wanted = target.trim();
        if wanted.is_empty() {
            return Err("Give a device: an alias, a fingerprint, or `192.168.1.5:53317`.".into());
        }
        let peers = self.devices();
        if let Some(found) = peers.iter().find(|peer| peer.node_id == wanted) {
            return Ok(found.clone());
        }
        if let Some(found) = peers
            .iter()
            .find(|peer| peer.alias.eq_ignore_ascii_case(wanted))
        {
            return Ok(found.clone());
        }
        if let Some(found) = peers
            .iter()
            .find(|peer| peer.ip == wanted || format!("{}:{}", peer.ip, peer.port) == wanted)
        {
            return Ok(found.clone());
        }

        // Not in the list. If it looks like an address, ask it directly: a model
        // that was told "send it to 192.168.1.5:53317" must not need a multicast
        // round trip first.
        let Some((host, port)) = split_host_port(wanted) else {
            return Err(no_device_message(wanted, &peers));
        };
        let discovery = HttpDiscovery::for_device(self.identity())
            .map_err(|error| format!("cannot probe {wanted}: {error}"))?;
        match discovery.probe_peer(host, port).await {
            Some(device) => Peer::from_device(&device)
                .ok_or_else(|| format!("{wanted} answered, but not with a usable device record.")),
            None => Err(format!(
                "Nothing answered at {wanted}. Check the port and that LocalSend is running there."
            )),
        }
    }

    /// Send files to a resolved device. One report for the batch, with the first
    /// failure named — a partial transfer that claims success is unactionable.
    pub async fn send(&self, peer: &Peer, paths: &[PathBuf], pin: Option<String>) -> SendReport {
        let mut report = SendReport {
            device: peer.alias.clone(),
            endpoint: peer.endpoint(),
            protocol: peer.protocol.as_str().to_uppercase(),
            bytes: 0,
            files: Vec::new(),
            error: None,
        };

        if paths.is_empty() {
            report.error = Some("No files were named, so nothing was sent.".into());
            return report;
        }
        for path in paths {
            match std::fs::metadata(path) {
                Ok(metadata) if metadata.is_file() => report.files.push(SentFile {
                    name: file_name(path),
                    path: path.display().to_string(),
                    size: metadata.len(),
                }),
                Ok(_) => {
                    report.error = Some(format!("{} is a folder, not a file.", path.display()));
                    return report;
                }
                Err(error) => {
                    report.error = Some(format!("{} cannot be read: {error}", path.display()));
                    return report;
                }
            }
        }
        report.bytes = report.files.iter().map(|file| file.size).sum();

        let identity = crate::service::local_identity(
            &self.settings,
            self.certificate.as_ref(),
            Some(&self.state().identity()),
        );
        let client = match client_for(&identity, peer, self.certificate.as_ref()) {
            Ok(client) => client,
            Err(error) => {
                report.error = Some(format!("Cannot talk to {}: {error}", peer.alias));
                return report;
            }
        };
        if let Err(error) = send_batch(&client, peer, paths, pin.as_deref()).await {
            report.error = Some(error);
        }
        report
    }

    /// Everything received, newest first, capped at `limit`.
    pub fn received(&self, limit: usize) -> Vec<ReceivedFile> {
        let state = self.state();
        let limit = limit.clamp(1, MAX_IN_MEMORY);
        state.inbox.iter().rev().take(limit).cloned().collect()
    }

    /// How much has been received so far — a cursor for [`Self::wait_for_new`].
    pub fn received_count(&self) -> usize {
        self.state().inbox.len()
    }

    /// Wait until at least one entry exists past `cursor`, or the timeout passes.
    ///
    /// This is the reliable notification path. MCP's
    /// `notifications/resources/updated` only reaches a client that subscribed,
    /// so a model that wants to know when a file arrives calls this instead of
    /// polling in a loop of its own.
    pub async fn wait_for_new(&self, cursor: usize, timeout: Duration) -> Vec<ReceivedFile> {
        let deadline = Instant::now() + timeout;
        loop {
            // Register before checking: a file that lands between the check and
            // the await would otherwise be missed until the timeout.
            let notified = self.state().new_file.clone();
            let fresh: Vec<ReceivedFile> = {
                let state = self.state();
                if state.inbox.len() > cursor {
                    state.inbox[cursor..].to_vec()
                } else {
                    Vec::new()
                }
            };
            if !fresh.is_empty() {
                return fresh;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Vec::new();
            }
            if tokio::time::timeout(remaining, notified.notified())
                .await
                .is_err()
            {
                return Vec::new();
            }
        }
    }

    /// Offers waiting for an answer. Expired ones are dropped, which declines
    /// them: the sender must not sit through a timeout nobody will answer.
    pub fn offers(&self) -> Vec<OfferSummary> {
        let mut state = self.state();
        let now = Instant::now();
        state.offers.retain(|_, held| held.expires > now);
        let mut offers: Vec<OfferSummary> = state
            .offers
            .values()
            .map(|held| OfferSummary {
                expires_in_ms: held.expires.saturating_duration_since(now).as_millis() as u64,
                ..held.summary.clone()
            })
            .collect();
        offers.sort_by_key(|offer| offer.id);
        offers
    }

    /// Take one offer. `None` when the id is unknown or has expired — dropping a
    /// request declines it, so an unknown id is a transfer the sender already saw
    /// fail.
    pub fn accept_offer(&self, id: u64) -> Option<OfferSummary> {
        let held = self.state().offers.remove(&id)?;
        held.request.accept();
        Some(held.summary)
    }

    /// Refuse one offer on purpose, rather than by letting it time out.
    pub fn decline_offer(&self, id: u64) -> Option<OfferSummary> {
        let held = self.state().offers.remove(&id)?;
        held.request.decline();
        Some(held.summary)
    }

    /// An offer arriving is worth waking a waiting caller for, even in `--ask`
    /// mode where no file has landed yet.
    pub fn offer_signal(&self) -> Arc<Notify> {
        self.state().new_offer.clone()
    }

    /// Fires once per journalled entry.
    pub fn file_signal(&self) -> Arc<Notify> {
        self.state().new_file.clone()
    }

    pub fn status(&self) -> StatusReport {
        let state = self.state();
        StatusReport {
            alias: state.alias.clone(),
            port: state.port,
            protocol: state.protocol.as_str().to_uppercase(),
            fingerprint: state.fingerprint.clone(),
            listening: state.listening,
            auto_accept: state.auto_accept,
            save_dir: state.save_dir.display().to_string(),
            journal: self.journal.path().display().to_string(),
            devices: state.peers.len(),
            offers: state.offers.len(),
            received: state.inbox.len(),
            pid: std::process::id(),
        }
    }

    /// Whether this node has a receiver at all.
    pub fn is_listening(&self) -> bool {
        self.listening
    }

    fn identity(&self) -> DeviceInfo {
        self.state().identity()
    }
}

/// A poisoned lock is not a reason to stop listening: the data behind it is plain
/// state, not an invariant a panic could have half-updated.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

async fn load_certificate(config: &McpConfig) -> Option<TlsCertificate> {
    let (certificate, key) = config.identity_paths();
    match tokio::task::spawn_blocking(move || {
        localsend_rs::load_or_generate_tls_certificate(certificate, key)
    })
    .await
    {
        Ok(Ok(certificate)) => Some(certificate),
        Ok(Err(error)) => {
            eprintln!("localsend-gpui mcp: could not load the TLS identity: {error}");
            None
        }
        Err(error) => {
            eprintln!("localsend-gpui mcp: TLS identity task failed: {error}");
            None
        }
    }
}

async fn build_server(
    settings: &Settings,
    certificate: &Option<TlsCertificate>,
    config: &McpConfig,
) -> Result<(LocalSendServer, mpsc::Receiver<ServerEvent>), String> {
    let protocol = if settings.https {
        Protocol::Https
    } else {
        Protocol::Http
    };
    let build = |port: u16| {
        let mut builder = LocalSendServer::builder()
            .alias(settings.alias.clone())
            .port(port)
            .save_dir(settings.save_dir.clone())
            .protocol(protocol)
            .accept_timeout(OFFER_TIMEOUT)
            .auto_accept(config.auto_accept);
        if let Some(pin) = settings.pin.clone() {
            builder = builder.pin(pin);
        }
        if let Some(certificate) = certificate.clone() {
            builder = builder.tls_certificate(certificate);
        }
        builder.build()
    };

    match build(settings.port).await {
        Ok(pair) => Ok(pair),
        Err(error) if port_in_use(&error) && settings.port != 0 => {
            let requested = settings.port;
            let fallback = build(0).await.map_err(|error| {
                format!("the receiver could not start on {requested} or on a free port: {error}")
            })?;
            eprintln!(
                "localsend-gpui mcp: port {requested} is in use (another LocalSend app?), \
                 listening on {} instead",
                fallback.0.port()
            );
            Ok(fallback)
        }
        Err(error) => Err(format!("the MCP receiver could not start: {error}")),
    }
}

fn port_in_use(error: &localsend_rs::LocalSendError) -> bool {
    matches!(
        error,
        localsend_rs::LocalSendError::Io { source }
            if source.kind() == std::io::ErrorKind::AddrInUse
    )
}

async fn start_discovery(
    device: &DeviceInfo,
    certificate: Option<&TlsCertificate>,
    shared: &Arc<Mutex<Shared>>,
) -> Option<Arc<MulticastDiscovery>> {
    let mut discovery = MulticastDiscovery::new_with_device(device.clone());
    if let Some(certificate) = certificate.cloned() {
        discovery.set_client_certificate(certificate);
    }
    let own = device.fingerprint.clone();
    let collected = shared.clone();
    discovery.on_discovered(move |found| {
        if found.fingerprint == own {
            return;
        }
        if let Some(peer) = Peer::from_device(&found) {
            lock(&collected).peers.insert(peer.node_id.clone(), peer);
        }
    });
    match Discovery::start(&mut discovery).await {
        Ok(()) => {
            if let Err(error) = discovery.announce_presence().await {
                eprintln!("localsend-gpui mcp: announcing failed: {error}");
            }
            Some(Arc::new(discovery))
        }
        Err(error) => {
            // Discovery is how a caller finds devices; without it only an
            // `address:port` target works. That is a degraded session, not a dead
            // one, so say so on stderr and carry on.
            eprintln!(
                "localsend-gpui mcp: local discovery is unavailable ({error}); \
                 send_files still works with an address:port target"
            );
            None
        }
    }
}

/// Receive one event, or wait forever when this node has no receiver.
async fn next_event(events: &mut Option<mpsc::Receiver<ServerEvent>>) -> Option<ServerEvent> {
    match events {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

/// The one task allowed to change the shared state in response to the network.
async fn pump(
    shared: Arc<Mutex<Shared>>,
    journal: Journal,
    server: Option<LocalSendServer>,
    mut events: Option<mpsc::Receiver<ServerEvent>>,
    mut announce: mpsc::UnboundedReceiver<()>,
    discovery: Option<Arc<MulticastDiscovery>>,
    auto_accept: bool,
) {
    // Owns the listener for the life of the session.
    let _server = server;

    let mut ticker = tokio::time::interval(ANNOUNCE_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;

    loop {
        tokio::select! {
            biased;
            event = next_event(&mut events) => match event {
                Some(event) => on_server_event(&shared, &journal, event, auto_accept),
                // The receiver went away; nothing more will arrive.
                None => events = None,
            },
            announcement = announce.recv() => match announcement {
                Some(()) => {
                    if let Some(discovery) = &discovery {
                        let _ = discovery.announce_presence().await;
                    }
                }
                None => break,
            },
            _ = ticker.tick() => {
                if let Some(discovery) = &discovery {
                    let _ = discovery.announce_presence().await;
                }
            }
        }
    }
}

fn on_server_event(
    shared: &Arc<Mutex<Shared>>,
    journal: &Journal,
    event: ServerEvent,
    auto_accept: bool,
) {
    match event {
        ServerEvent::PeerRegistered(device) => {
            let own = lock(shared).fingerprint.clone();
            if device.fingerprint == own {
                return;
            }
            if let Some(peer) = Peer::from_device(&device) {
                lock(shared).peers.insert(peer.node_id.clone(), peer);
            }
        }
        ServerEvent::TransferRequest(request) => {
            let Some(from) = Peer::from_device_lenient(request.sender()) else {
                request.decline();
                return;
            };
            let mut files: Vec<(String, u64)> = request
                .files()
                .values()
                .map(|metadata| (metadata.file_name.clone(), metadata.size))
                .collect();
            files.sort_by(|left, right| left.0.cmp(&right.0));

            if auto_accept {
                request.accept();
                return;
            }

            let mut state = lock(shared);
            let id = state.next_offer;
            state.next_offer += 1;
            state.offers.insert(
                id,
                Held {
                    request,
                    summary: OfferSummary {
                        id,
                        from: from.alias.clone(),
                        sender_id: from.node_id.clone(),
                        files: files.iter().map(|(name, _)| name.clone()).collect(),
                        bytes: files.iter().map(|(_, size)| *size).sum(),
                        expires_in_ms: OFFER_TIMEOUT.as_millis() as u64,
                    },
                    expires: Instant::now() + OFFER_TIMEOUT,
                },
            );
            state.new_offer.notify_waiters();
        }
        ServerEvent::FileReceived {
            file_name,
            path,
            size,
            sender_alias,
            ..
        } => {
            let sender_id = lock(shared)
                .peers
                .values()
                .find(|peer| peer.alias == sender_alias)
                .map(|peer| peer.node_id.clone())
                .unwrap_or_default();
            record(
                shared,
                journal,
                ReceivedFile::file(file_name, path, size, sender_alias, sender_id),
            );
        }
        ServerEvent::TextReceived {
            text, sender_alias, ..
        } => {
            record(shared, journal, ReceivedFile::text(text, sender_alias));
        }
        ServerEvent::SessionDone { .. } => {}
        // Chrome-level progress is per-chunk noise for a model; the journal is
        // the signal. The browser share page is not exposed over MCP.
        ServerEvent::FileReceiveProgress { .. }
        | ServerEvent::WebShareRequest(_)
        | ServerEvent::WebShareDownloadProgress { .. }
        | ServerEvent::WebShareSessionDone { .. } => {}
    }
}

fn record(shared: &Arc<Mutex<Shared>>, journal: &Journal, entry: ReceivedFile) {
    let mut state = lock(shared);
    let existing = state.inbox.len();
    let mut entry = entry;
    // A journal write that fails still leaves the entry visible this session, so
    // a full disk degrades the history rather than losing the notification.
    if let Err(error) = journal.append(&mut entry, existing) {
        eprintln!("localsend-gpui mcp: could not journal the received file: {error}");
        entry.id = format!("{}-{}", now_millis(), existing + 1);
    }
    state.inbox.push(entry);
    if state.inbox.len() > MAX_IN_MEMORY {
        let excess = state.inbox.len() - MAX_IN_MEMORY;
        state.inbox.drain(..excess);
    }
    let signal = state.new_file.clone();
    drop(state);
    signal.notify_waiters();
}

/// The whole send half of one batch, mirroring the window's controller: one
/// `prepare-upload`, one upload per accepted file, the first failure named.
async fn send_batch(
    client: &LocalSendClient,
    peer: &Peer,
    paths: &[PathBuf],
    pin: Option<&str>,
) -> Result<(), String> {
    let target = peer.to_device();
    // A peer that is in our list from a multicast answer has never heard of us.
    // Registration is a courtesy; `prepare-upload` carries the same identity.
    let _ = client.register(&target).await;

    let mut metadata = HashMap::with_capacity(paths.len());
    let mut order = Vec::with_capacity(paths.len());
    for path in paths {
        let meta = build_file_metadata(path)
            .await
            .map_err(|error| format!("{} could not be read: {error}", file_name(path)))?;
        order.push((meta.id.clone(), path.clone()));
        metadata.insert(meta.id.clone(), meta);
    }

    let prepared = client
        .prepare_upload(&target, metadata, pin)
        .await
        .map_err(|error| describe_prepare_failure(peer, error))?;

    // 204: the receiver took everything from the offer itself (text messages).
    if prepared.session_id.as_str().is_empty() {
        return Ok(());
    }
    for (file_id, path) in order {
        // The receiver may accept a subset; a file it did not take has no token
        // and is not ours to upload.
        let Some(token) = prepared.files.get(&file_id) else {
            continue;
        };
        client
            .upload_file(&target, &prepared.session_id, &file_id, token, &path, None)
            .await
            .map_err(|error| format!("{} did not arrive: {error}", file_name(&path)))?;
    }
    Ok(())
}

fn describe_prepare_failure(peer: &Peer, error: localsend_rs::LocalSendError) -> String {
    use localsend_rs::LocalSendError;
    match error {
        LocalSendError::InvalidPin => "The receiving device rejected the PIN.".to_string(),
        LocalSendError::RateLimited => {
            "Too many PIN attempts — wait a few minutes before trying again.".to_string()
        }
        LocalSendError::Rejected { .. } => format!(
            "{} declined the files. If it is waiting for a person to answer, they said no.",
            peer.alias
        ),
        LocalSendError::SessionBlocked => {
            format!("{} is busy with another transfer.", peer.alias)
        }
        other => format!("{} could not start the transfer: {other}", peer.alias),
    }
}

fn no_device_message(wanted: &str, peers: &[Peer]) -> String {
    if peers.is_empty() {
        return format!(
            "No device named `{wanted}` is known and no address was given. Call list_devices first \
             — nothing has answered yet."
        );
    }
    let known: Vec<String> = peers
        .iter()
        .map(|peer| format!("{} ({})", peer.alias, peer.endpoint()))
        .collect();
    format!(
        "No device matches `{wanted}`. Known devices: {}. An `address:port` also works.",
        known.join(", ")
    )
}

/// `host:port`, or an address on the protocol's default port. IPv6 literals are
/// not supported — LocalSend's own URLs are dotted quads, and guessing at bracket
/// syntax would turn a typo into a connection to the wrong host.
pub fn split_host_port(target: &str) -> Option<(&str, u16)> {
    match target.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => {
            let port = port.parse::<u16>().ok()?;
            (port != 0).then_some((host, port))
        }
        Some(_) => None,
        None => {
            let looks_like_address = target.contains('.')
                && target
                    .split('.')
                    .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
            looks_like_address.then_some((target, localsend_rs::DEFAULT_HTTP_PORT))
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_alias_says_which_process_it_is_and_stays_inside_the_protocol_limit() {
        assert_eq!(alias_with_suffix("Huakun's Mac"), "Huakun's Mac (MCP)");
        let alias = alias_with_suffix(&"a".repeat(40));
        assert_eq!(alias.chars().count(), ALIAS_LIMIT);
        assert!(alias.ends_with(ALIAS_SUFFIX));
        // Multibyte names must not be cut mid-character.
        let cjk = "笔记本".repeat(12);
        assert!(alias_with_suffix(&cjk).chars().count() <= ALIAS_LIMIT);
    }

    #[test]
    fn a_target_can_be_an_address_with_or_without_a_port() {
        assert_eq!(split_host_port("10.0.0.7:53318"), Some(("10.0.0.7", 53318)));
        assert_eq!(
            split_host_port("10.0.0.7"),
            Some(("10.0.0.7", localsend_rs::DEFAULT_HTTP_PORT))
        );
        assert_eq!(split_host_port("Pixel"), None, "an alias is not an address");
        assert_eq!(split_host_port("10.0.0.7:0"), None);
        assert_eq!(split_host_port(":53317"), None);
        assert_eq!(split_host_port("10.0.0.7:notaport"), None);
    }

    #[test]
    fn an_unknown_target_is_told_what_is_known_instead() {
        assert!(no_device_message("Pixel", &[]).contains("list_devices"));
        let peer = Peer {
            node_id: "ab".into(),
            alias: "Pixel".into(),
            model: "Pixel 9".into(),
            device_kind: crate::service::PeerKind::Phone,
            ip: "10.0.0.7".into(),
            port: 53317,
            protocol: Protocol::Http,
            last_seen: std::time::SystemTime::now(),
        };
        let message = no_device_message("Laptop", &[peer]);
        assert!(message.contains("Pixel (10.0.0.7:53317 · HTTP)"));
    }
}
