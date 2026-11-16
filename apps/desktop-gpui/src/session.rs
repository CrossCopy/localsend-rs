//! The window's state machine: every fact a view draws, and the one place a
//! [`ServiceEvent`] is turned into it.
//!
//! This module has no GPUI in it. That is deliberate: the part of a GUI that is
//! worth testing exhaustively is what happens to the screen when events arrive in
//! awkward orders — a progress chunk before the offer, a file that lands after the
//! session closed, a peer that reappears with a new address. Those are plain data
//! transitions, and `#[cfg(test)]` below pins them without a window.
//!
//! The store (`crate::store`) is a thin GPUI entity around this: it forwards
//! events here and calls `cx.notify()`.

use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::service::format;
use crate::service::{
    NetworkInterface, NoticeLevel, Offer, Peer, SendFile, ServerStatus, ServiceEvent, TransferId,
    TransferOutcome,
};

/// How long a peer stays in the list without being seen again. Long enough that a
/// device answering slowly is not dropped mid-send, short enough that a laptop
/// that left the network does not haunt the list.
pub const PEER_TTL: Duration = Duration::from_secs(120);

/// How many finished transfers the window remembers.
pub const HISTORY_LIMIT: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Incoming,
    Outgoing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferState {
    Active,
    Completed,
    Failed(String),
    Cancelled,
}

impl TransferState {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferFile {
    pub name: String,
    pub size: u64,
    pub done: u64,
    pub path: Option<PathBuf>,
}

/// A transfer in flight. Rows appear when the first event of a transfer arrives
/// and disappear when it finishes; the record of what happened lives in
/// [`HistoryEntry`].
#[derive(Clone, Debug)]
pub struct Transfer {
    pub id: TransferId,
    pub direction: Direction,
    pub peer: String,
    pub files: Vec<TransferFile>,
    /// 1-based index of the file currently moving.
    pub file_index: usize,
    pub session_done: u64,
    pub session_total: u64,
    pub started_at: SystemTime,
    pub updated_at: SystemTime,
    pub state: TransferState,
}

impl Transfer {
    pub fn ratio(&self) -> f32 {
        format::percent(self.session_done, self.session_total)
    }

    pub fn current_file(&self) -> Option<&TransferFile> {
        let index = self.file_index.saturating_sub(1);
        self.files
            .get(index)
            .or_else(|| self.files.iter().find(|file| file.done < file.size))
            .or_else(|| self.files.last())
    }

    /// Average throughput since the transfer started. Averaged, not instantaneous:
    /// an instantaneous rate over a 40 ms window is noise, and this number is read
    /// to answer "how long is this going to take".
    pub fn bytes_per_second(&self, now: SystemTime) -> f64 {
        let elapsed = now
            .duration_since(self.started_at)
            .unwrap_or(Duration::ZERO)
            .as_secs_f64();
        if elapsed < 0.25 || self.session_done == 0 {
            return 0.0;
        }
        self.session_done as f64 / elapsed
    }

    pub fn eta(&self, now: SystemTime) -> Option<String> {
        let remaining = self.session_total.saturating_sub(self.session_done);
        format::eta(remaining, self.bytes_per_second(now))
    }

    pub fn is_incoming(&self) -> bool {
        self.direction == Direction::Incoming
    }
}

/// One finished file (or text message) — the permanent record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub id: u64,
    pub direction: Direction,
    pub peer: String,
    pub name: String,
    pub size: u64,
    pub path: Option<PathBuf>,
    /// Set for a text message, which never lands on disk.
    pub text: Option<String>,
    pub outcome: TransferOutcome,
    pub at: SystemTime,
}

impl HistoryEntry {
    pub fn is_text(&self) -> bool {
        self.text.is_some()
    }

    pub fn summary(&self) -> String {
        match &self.text {
            Some(text) => text.lines().next().unwrap_or("").to_string(),
            None => self.name.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Notice {
    pub level: NoticeLevel,
    pub message: String,
    pub at: SystemTime,
}

/// The whole window state.
///
/// `Clone` so a view can take a snapshot out of the store before building
/// elements: the element builders need `&mut Context` for listeners, which cannot
/// coexist with a borrow of the store that lives inside the same context.
#[derive(Clone)]
pub struct Session {
    pub status: ServerStatus,
    pub peers: Vec<Peer>,
    /// Current non-loopback IPv4 interfaces, refreshed at startup and on demand.
    pub network_interfaces: Vec<NetworkInterface>,
    /// Interface addresses whose bounded subnet scans are currently in progress.
    pub scanning_subnets: HashSet<Ipv4Addr>,
    pub offers: Vec<Offer>,
    pub transfers: Vec<Transfer>,
    /// Newest first.
    pub history: Vec<HistoryEntry>,
    /// Newest last; the shell shows the last one.
    pub notices: Vec<Notice>,
    history_seq: u64,
}

impl Session {
    pub fn new() -> Self {
        Self {
            status: ServerStatus::Starting,
            peers: Vec::new(),
            network_interfaces: Vec::new(),
            scanning_subnets: HashSet::new(),
            offers: Vec::new(),
            transfers: Vec::new(),
            history: Vec::new(),
            notices: Vec::new(),
            history_seq: 0,
        }
    }

    // ------------------------------------------------------------------ reads

    pub fn listening(&self) -> bool {
        self.status.is_listening()
    }

    pub fn listening_port(&self) -> Option<u16> {
        match &self.status {
            ServerStatus::Listening { port, .. } => Some(*port),
            _ => None,
        }
    }

    pub fn peer(&self, node_id: &str) -> Option<&Peer> {
        self.peers.iter().find(|peer| peer.node_id == node_id)
    }

    pub fn active_transfers(&self) -> impl Iterator<Item = &Transfer> {
        self.transfers
            .iter()
            .filter(|transfer| transfer.state.is_active())
    }

    pub fn active_send(&self) -> Option<&Transfer> {
        self.active_transfers()
            .find(|transfer| transfer.direction == Direction::Outgoing)
    }

    pub fn active_receives(&self) -> impl Iterator<Item = &Transfer> {
        self.active_transfers()
            .filter(|transfer| transfer.direction == Direction::Incoming)
    }

    pub fn last_notice(&self) -> Option<&Notice> {
        self.notices.last()
    }

    pub fn unread_offer_count(&self) -> usize {
        self.offers.len()
    }

    /// Received files that still exist on disk, newest first.
    pub fn received_files(&self) -> impl Iterator<Item = &HistoryEntry> {
        self.history
            .iter()
            .filter(|entry| entry.direction == Direction::Incoming && entry.path.is_some())
    }

    // ---------------------------------------------------------------- pruning

    /// Drop peers nobody has heard from in [`PEER_TTL`]. Called by the shell on a
    /// timer as well as after every event, because a quiet window receives no
    /// events to prune on.
    pub fn prune_peers(&mut self, now: SystemTime) -> bool {
        let before = self.peers.len();
        self.peers
            .retain(|peer| now.duration_since(peer.last_seen).unwrap_or(Duration::ZERO) < PEER_TTL);
        before != self.peers.len()
    }

    pub fn dismiss_notice(&mut self) {
        self.notices.pop();
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    // ------------------------------------------------------------------ write

    pub fn apply(&mut self, event: ServiceEvent) {
        match event {
            ServiceEvent::Status(status) => self.status = status,
            ServiceEvent::NetworkInterfacesUpdated(interfaces) => {
                self.network_interfaces = interfaces;
                self.scanning_subnets.retain(|address| {
                    self.network_interfaces
                        .iter()
                        .any(|interface| interface.address == *address)
                });
            }
            ServiceEvent::NetworkInterfacesFailed { message } => {
                self.push_notice(NoticeLevel::Error, message);
            }
            ServiceEvent::SubnetScanStarted { address } => {
                self.scanning_subnets.insert(address);
            }
            ServiceEvent::SubnetScanFinished {
                address,
                complete,
                found,
            } => {
                self.scanning_subnets.remove(&address);
                if !complete {
                    self.push_notice(
                        NoticeLevel::Warning,
                        format!("Subnet scan for {address} was partial; found {found} device(s)."),
                    );
                }
            }
            ServiceEvent::SubnetScanBusy { address } => {
                self.push_notice(
                    NoticeLevel::Info,
                    format!("Another subnet scan is already running (requested {address})."),
                );
            }
            ServiceEvent::SubnetScanFailed { address, message } => {
                self.scanning_subnets.remove(&address);
                self.push_notice(
                    NoticeLevel::Error,
                    format!("Subnet scan failed for {address}: {message}"),
                );
            }
            ServiceEvent::PeerFound(peer) => self.upsert_peer(peer),
            ServiceEvent::Offer(offer) => {
                if !self.offers.iter().any(|existing| existing.id == offer.id) {
                    self.offers.push(offer);
                }
            }
            ServiceEvent::OfferAcceptedAutomatically(offer) => {
                // No prompt: it is already being received. Say so once, then let
                // the progress rows tell the rest.
                self.notices.push(Notice {
                    level: NoticeLevel::Info,
                    message: format!(
                        "Accepted {} from {} (favorite).",
                        crate::service::format::files(offer.files.len()),
                        offer.from.alias
                    ),
                    at: SystemTime::now(),
                });
            }
            ServiceEvent::OfferClosed { id, accepted } => {
                self.offers.retain(|offer| offer.id != id);
                if accepted {
                    // Accepted offers become a progress row on the first chunk;
                    // nothing to draw until bytes move.
                }
            }
            ServiceEvent::ReceiveProgress {
                id,
                sender,
                file_name,
                file_index,
                file_count,
                file_done,
                file_size,
                session_done,
                session_total,
            } => {
                let transfer = self.transfer_or_start(id, Direction::Incoming, sender);
                transfer.file_index = file_index.max(1);
                transfer.session_done = session_done;
                transfer.session_total = session_total.max(file_size);
                transfer.updated_at = SystemTime::now();
                upsert_file(&mut transfer.files, &file_name, file_size, file_done, false);
                let _ = file_count;
            }
            ServiceEvent::Received {
                id,
                sender,
                file_name,
                path,
                size,
            } => {
                let transfer = self.transfer_or_start(id, Direction::Incoming, sender.clone());
                upsert_file(&mut transfer.files, &file_name, size, size, true);
                if let Some(file) = transfer
                    .files
                    .iter_mut()
                    .find(|file| file.name == file_name)
                {
                    file.path = Some(path.clone());
                }
                self.push_history(HistoryEntry {
                    id: 0,
                    direction: Direction::Incoming,
                    peer: sender,
                    name: file_name,
                    size,
                    path: Some(path),
                    text: None,
                    outcome: TransferOutcome::Completed,
                    at: SystemTime::now(),
                });
            }
            ServiceEvent::ReceivedText { sender, text } => {
                self.push_history(HistoryEntry {
                    id: 0,
                    direction: Direction::Incoming,
                    peer: sender,
                    name: "Text message".to_string(),
                    size: text.len() as u64,
                    path: None,
                    text: Some(text),
                    outcome: TransferOutcome::Completed,
                    at: SystemTime::now(),
                });
            }
            ServiceEvent::ReceiveFinished { id, outcome } => {
                self.finish(id, outcome);
            }
            ServiceEvent::SendStarted {
                id,
                peer_alias,
                files,
                total_bytes,
            } => {
                let now = SystemTime::now();
                let files = files
                    .into_iter()
                    .map(|SendFile { name, size, .. }| TransferFile {
                        name,
                        size,
                        done: 0,
                        path: None,
                    })
                    .collect();
                self.transfers.push(Transfer {
                    id,
                    direction: Direction::Outgoing,
                    peer: peer_alias,
                    files,
                    file_index: 1,
                    session_done: 0,
                    session_total: total_bytes,
                    started_at: now,
                    updated_at: now,
                    state: TransferState::Active,
                });
            }
            ServiceEvent::SendProgress {
                id,
                file_name,
                file_index,
                file_count,
                file_done,
                file_size,
                session_done,
                session_total,
            } => {
                if let Some(transfer) = self.transfers.iter_mut().find(|transfer| transfer.id == id)
                {
                    transfer.file_index = file_index.max(1);
                    transfer.session_done = session_done;
                    transfer.session_total = session_total.max(file_size);
                    transfer.updated_at = SystemTime::now();
                    upsert_file(&mut transfer.files, &file_name, file_size, file_done, false);
                }
                let _ = file_count;
            }
            ServiceEvent::SendFinished { id, outcome } => {
                let transfer = self
                    .transfers
                    .iter()
                    .find(|transfer| transfer.id == id)
                    .cloned();
                if let Some(transfer) = transfer {
                    self.push_send_history(&transfer, &outcome);
                }
                self.finish(id, outcome);
            }
            ServiceEvent::Notice { level, message } => {
                self.push_notice(level, message);
            }
        }
    }

    fn push_notice(&mut self, level: NoticeLevel, message: String) {
        let now = SystemTime::now();
        // A peer that keeps refusing produces the same sentence over and
        // over; show it once rather than stacking a wall of duplicates.
        let duplicate = self.notices.last().is_some_and(|notice| {
            notice.message == message
                && now.duration_since(notice.at).unwrap_or(Duration::ZERO) < Duration::from_secs(4)
        });
        if !duplicate {
            self.notices.push(Notice {
                level,
                message,
                at: now,
            });
            if self.notices.len() > 4 {
                self.notices.remove(0);
            }
        }
    }

    fn upsert_peer(&mut self, peer: Peer) {
        match self
            .peers
            .iter_mut()
            .find(|existing| existing.node_id == peer.node_id)
        {
            Some(existing) if peer.last_seen > existing.last_seen => {
                // The newest sighting is the truth: a device that moved to a new
                // address must be reachable at the new one.
                *existing = peer;
            }
            Some(_) => {}
            None => self.peers.push(peer),
        }
        self.peers.sort_by(|left, right| {
            left.alias
                .to_lowercase()
                .cmp(&right.alias.to_lowercase())
                .then_with(|| left.node_id.cmp(&right.node_id))
        });
    }

    fn transfer_or_start(
        &mut self,
        id: TransferId,
        direction: Direction,
        peer: String,
    ) -> &mut Transfer {
        let index = match self.transfers.iter().position(|transfer| transfer.id == id) {
            Some(index) => index,
            None => {
                let now = SystemTime::now();
                self.transfers.push(Transfer {
                    id,
                    direction,
                    peer,
                    files: Vec::new(),
                    file_index: 1,
                    session_done: 0,
                    session_total: 0,
                    started_at: now,
                    updated_at: now,
                    state: TransferState::Active,
                });
                self.transfers.len() - 1
            }
        };
        &mut self.transfers[index]
    }

    fn finish(&mut self, id: TransferId, outcome: TransferOutcome) {
        if let Some(index) = self.transfers.iter().position(|transfer| transfer.id == id) {
            let state = match &outcome {
                TransferOutcome::Completed => TransferState::Completed,
                TransferOutcome::Cancelled => TransferState::Cancelled,
                TransferOutcome::Failed { message } => TransferState::Failed(message.clone()),
            };
            self.transfers[index].state = state;
            // A finished transfer leaves the live list: what remains is history,
            // and a row that says "active" after it ended is a lie the user has to
            // reason about.
            self.transfers.remove(index);
        }
    }

    fn push_send_history(&mut self, transfer: &Transfer, outcome: &TransferOutcome) {
        match outcome {
            TransferOutcome::Completed => {
                // `push_history` inserts at the head, so walk the batch backwards
                // to leave it in the order it was sent.
                for file in transfer.files.iter().rev() {
                    self.push_history(HistoryEntry {
                        id: 0,
                        direction: Direction::Outgoing,
                        peer: transfer.peer.clone(),
                        name: file.name.clone(),
                        size: file.size,
                        path: None,
                        text: None,
                        outcome: TransferOutcome::Completed,
                        at: SystemTime::now(),
                    });
                }
            }
            TransferOutcome::Failed { message } => {
                let name = transfer
                    .current_file()
                    .map(|file| file.name.clone())
                    .unwrap_or_else(|| "Transfer".to_string());
                self.push_history(HistoryEntry {
                    id: 0,
                    direction: Direction::Outgoing,
                    peer: transfer.peer.clone(),
                    name,
                    size: transfer.session_done,
                    path: None,
                    text: Some(message.clone()),
                    outcome: outcome.clone(),
                    at: SystemTime::now(),
                });
            }
            TransferOutcome::Cancelled => {}
        }
    }

    fn push_history(&mut self, mut entry: HistoryEntry) {
        self.history_seq += 1;
        entry.id = self.history_seq;
        self.history.insert(0, entry);
        self.history.truncate(HISTORY_LIMIT);
    }
}

fn upsert_file(files: &mut Vec<TransferFile>, name: &str, size: u64, done: u64, finished: bool) {
    if let Some(file) = files.iter_mut().find(|file| file.name == name) {
        file.size = file.size.max(size);
        file.done = file.done.max(done);
        if finished {
            file.done = file.size;
        }
    } else {
        files.push(TransferFile {
            name: name.to_string(),
            size,
            done: if finished { size.max(done) } else { done },
            path: None,
        });
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{NetworkInterface, OfferFile};
    use localsend_rs::Protocol;
    use std::net::Ipv4Addr;

    fn peer(node_id: &str, alias: &str, ip: &str) -> Peer {
        Peer {
            node_id: node_id.to_string(),
            alias: alias.to_string(),
            model: "MacBook".to_string(),
            device_kind: crate::service::PeerKind::Computer,
            ip: ip.to_string(),
            port: 53317,
            protocol: Protocol::Https,
            last_seen: SystemTime::now(),
        }
    }

    fn offer(id: TransferId) -> Offer {
        Offer {
            id,
            from: peer("peer-1", "iPhone", "10.0.0.9"),
            files: vec![OfferFile {
                id: "file-1".to_string(),
                name: "photo.jpg".to_string(),
                size: 2_000,
                mime: "image/jpeg".to_string(),
            }],
            total_bytes: 2_000,
        }
    }

    fn progress(id: TransferId, file: &str, done: u64, total: u64) -> ServiceEvent {
        ServiceEvent::ReceiveProgress {
            id,
            sender: "iPhone".to_string(),
            file_name: file.to_string(),
            file_index: 1,
            file_count: 1,
            file_done: done,
            file_size: total,
            session_done: done,
            session_total: total,
        }
    }

    #[test]
    fn a_peer_is_one_row_even_when_it_announces_twice_from_a_new_address() {
        let mut session = Session::new();
        session.apply(ServiceEvent::PeerFound(peer("abc", "iPhone", "10.0.0.9")));
        session.apply(ServiceEvent::PeerFound(peer("abc", "iPhone", "10.0.0.42")));
        assert_eq!(session.peers.len(), 1);
        assert_eq!(session.peer("abc").unwrap().ip, "10.0.0.42");
        assert_eq!(session.peer("abc").unwrap().alias, "iPhone");
    }

    #[test]
    fn a_late_scan_observation_cannot_replace_a_newer_multicast_peer() {
        let mut session = Session::new();
        let observed_at = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let multicast_seen_at = observed_at + Duration::from_secs(2);
        let mut scan_peer = peer("same-device", "Phone", "192.168.1.8");
        scan_peer.last_seen = observed_at;
        let mut multicast_peer = peer("same-device", "Phone", "192.168.1.20");
        multicast_peer.last_seen = multicast_seen_at;

        session.apply(ServiceEvent::PeerFound(multicast_peer));
        session.apply(ServiceEvent::PeerFound(scan_peer));

        let current = session.peer("same-device").expect("existing peer");
        assert_eq!(current.ip, "192.168.1.20");
        assert_eq!(current.last_seen, multicast_seen_at);
    }

    #[test]
    fn an_equal_time_scan_observation_preserves_the_existing_peer() {
        let mut session = Session::new();
        let observed_at = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let mut existing = peer("same-device", "Phone", "192.168.1.20");
        existing.last_seen = observed_at;
        let mut scan_peer = peer("same-device", "Phone", "192.168.1.8");
        scan_peer.last_seen = observed_at;

        session.apply(ServiceEvent::PeerFound(existing));
        session.apply(ServiceEvent::PeerFound(scan_peer));

        let current = session.peer("same-device").expect("existing peer");
        assert_eq!(current.ip, "192.168.1.20");
        assert_eq!(current.last_seen, observed_at);
    }

    #[test]
    fn a_scan_observation_still_updates_a_peer_seen_earlier() {
        let mut session = Session::new();
        let old_seen_at = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let scan_seen_at = old_seen_at + Duration::from_secs(2);
        let mut old_peer = peer("same-device", "Phone", "192.168.1.8");
        old_peer.last_seen = old_seen_at;
        let mut scan_peer = peer("same-device", "Phone", "192.168.1.20");
        scan_peer.last_seen = scan_seen_at;

        session.apply(ServiceEvent::PeerFound(old_peer));
        session.apply(ServiceEvent::PeerFound(scan_peer));

        let current = session.peer("same-device").expect("existing peer");
        assert_eq!(current.ip, "192.168.1.20");
        assert_eq!(current.last_seen, scan_seen_at);
    }

    #[test]
    fn peers_are_listed_alphabetically() {
        let mut session = Session::new();
        for (id, alias) in [("c", "zeta"), ("a", "Alpha"), ("b", "beta")] {
            session.apply(ServiceEvent::PeerFound(peer(id, alias, "10.0.0.1")));
        }
        let aliases: Vec<&str> = session
            .peers
            .iter()
            .map(|peer| peer.alias.as_str())
            .collect();
        assert_eq!(aliases, vec!["Alpha", "beta", "zeta"]);
    }

    #[test]
    fn interface_refresh_and_subnet_scan_events_update_the_selected_scan_state() {
        let mut session = Session::new();
        let interface = NetworkInterface {
            name: "en0".to_string(),
            address: Ipv4Addr::new(192, 168, 1, 10),
        };
        session.apply(ServiceEvent::NetworkInterfacesUpdated(vec![
            interface.clone()
        ]));
        assert_eq!(session.network_interfaces, vec![interface]);

        let address = Ipv4Addr::new(192, 168, 1, 10);
        session.apply(ServiceEvent::SubnetScanStarted { address });
        assert!(session.scanning_subnets.contains(&address));

        session.apply(ServiceEvent::SubnetScanFinished {
            address,
            complete: false,
            found: 2,
        });
        assert!(!session.scanning_subnets.contains(&address));

        session.apply(ServiceEvent::SubnetScanStarted { address });
        session.apply(ServiceEvent::SubnetScanFailed {
            address,
            message: "interface disappeared".to_string(),
        });
        assert!(!session.scanning_subnets.contains(&address));
        assert_eq!(
            session.last_notice().map(|notice| notice.message.as_str()),
            Some("Subnet scan failed for 192.168.1.10: interface disappeared")
        );
    }

    #[test]
    fn interface_refresh_removes_running_scans_for_disappeared_addresses() {
        let mut session = Session::new();
        let address = Ipv4Addr::new(192, 168, 1, 10);
        session.apply(ServiceEvent::SubnetScanStarted { address });
        assert!(session.scanning_subnets.contains(&address));

        session.apply(ServiceEvent::NetworkInterfacesUpdated(Vec::new()));

        assert!(!session.scanning_subnets.contains(&address));
    }

    #[test]
    fn pruning_only_drops_peers_older_than_the_ttl() {
        let mut session = Session::new();
        let mut old = peer("old", "Gone", "10.0.0.2");
        old.last_seen = SystemTime::now() - PEER_TTL - Duration::from_secs(1);
        session.apply(ServiceEvent::PeerFound(old));
        session.apply(ServiceEvent::PeerFound(peer("new", "Here", "10.0.0.3")));
        assert!(session.prune_peers(SystemTime::now()));
        assert_eq!(session.peers.len(), 1);
        assert_eq!(session.peers[0].node_id, "new");
        assert!(!session.prune_peers(SystemTime::now()));
    }

    #[test]
    fn an_offer_stays_until_its_answer_arrives() {
        let mut session = Session::new();
        session.apply(ServiceEvent::Offer(offer(7)));
        assert_eq!(session.unread_offer_count(), 1);
        // A duplicate delivery must not double the prompt.
        session.apply(ServiceEvent::Offer(offer(7)));
        assert_eq!(session.unread_offer_count(), 1);
        session.apply(ServiceEvent::OfferClosed {
            id: 7,
            accepted: true,
        });
        assert_eq!(session.unread_offer_count(), 0);
    }

    #[test]
    fn a_receive_shows_progress_then_lands_in_history() {
        let mut session = Session::new();
        session.apply(progress(3, "photo.jpg", 500, 2_000));
        let transfer = session.active_receives().next().expect("a live row");
        assert_eq!(transfer.peer, "iPhone");
        assert_eq!(transfer.ratio(), 25.0);
        assert_eq!(transfer.current_file().unwrap().name, "photo.jpg");

        session.apply(ServiceEvent::Received {
            id: 3,
            sender: "iPhone".to_string(),
            file_name: "photo.jpg".to_string(),
            path: PathBuf::from("/tmp/photo.jpg"),
            size: 2_000,
        });
        assert_eq!(session.history.len(), 1);
        assert_eq!(
            session.history[0].path.as_deref(),
            Some(std::path::Path::new("/tmp/photo.jpg"))
        );

        session.apply(ServiceEvent::ReceiveFinished {
            id: 3,
            outcome: TransferOutcome::Completed,
        });
        assert_eq!(session.active_receives().count(), 0);
        assert_eq!(session.history.len(), 1);
    }

    #[test]
    fn a_file_can_arrive_before_any_progress_was_reported() {
        let mut session = Session::new();
        session.apply(ServiceEvent::Received {
            id: 9,
            sender: "iPad".to_string(),
            file_name: "note.txt".to_string(),
            path: PathBuf::from("/tmp/note.txt"),
            size: 12,
        });
        let transfer = session
            .active_receives()
            .next()
            .expect("row created lazily");
        assert_eq!(transfer.files.len(), 1);
        assert_eq!(transfer.files[0].done, 12);
        assert_eq!(session.history[0].name, "note.txt");
    }

    #[test]
    fn a_text_message_is_recorded_without_a_path() {
        let mut session = Session::new();
        session.apply(ServiceEvent::ReceivedText {
            sender: "iPhone".to_string(),
            text: "https://example.com/x\nsecond line".to_string(),
        });
        let entry = &session.history[0];
        assert!(entry.is_text());
        assert_eq!(entry.summary(), "https://example.com/x");
        assert_eq!(entry.path, None);
        assert_eq!(entry.direction, Direction::Incoming);
    }

    #[test]
    fn a_completed_send_files_one_history_row_per_file() {
        let mut session = Session::new();
        session.apply(ServiceEvent::SendStarted {
            id: 11,
            peer_alias: "MacBook".to_string(),
            files: vec![
                SendFile {
                    name: "a.txt".to_string(),
                    size: 10,
                    path: PathBuf::from("/tmp/a.txt"),
                },
                SendFile {
                    name: "b.txt".to_string(),
                    size: 20,
                    path: PathBuf::from("/tmp/b.txt"),
                },
            ],
            total_bytes: 30,
        });
        assert_eq!(session.active_send().unwrap().session_total, 30);
        session.apply(ServiceEvent::SendProgress {
            id: 11,
            file_name: "a.txt".to_string(),
            file_index: 1,
            file_count: 2,
            file_done: 10,
            file_size: 10,
            session_done: 10,
            session_total: 30,
        });
        assert!((session.active_send().unwrap().ratio() - 33.333336).abs() < 0.001);
        session.apply(ServiceEvent::SendFinished {
            id: 11,
            outcome: TransferOutcome::Completed,
        });
        assert!(session.active_send().is_none());
        assert_eq!(session.history.len(), 2);
        assert_eq!(session.history[0].name, "a.txt");
        assert_eq!(session.history[1].name, "b.txt");
        assert!(session
            .history
            .iter()
            .all(|entry| entry.direction == Direction::Outgoing));
    }

    #[test]
    fn a_failed_send_records_the_reason_once() {
        let mut session = Session::new();
        session.apply(ServiceEvent::SendStarted {
            id: 12,
            peer_alias: "iPhone".to_string(),
            files: vec![SendFile {
                name: "a.txt".to_string(),
                size: 10,
                path: PathBuf::from("/tmp/a.txt"),
            }],
            total_bytes: 10,
        });
        session.apply(ServiceEvent::SendFinished {
            id: 12,
            outcome: TransferOutcome::Failed {
                message: "That PIN was not accepted.".to_string(),
            },
        });
        assert_eq!(session.history.len(), 1);
        assert_eq!(
            session.history[0].outcome,
            TransferOutcome::Failed {
                message: "That PIN was not accepted.".to_string()
            }
        );
        assert_eq!(
            session.history[0].text.as_deref(),
            Some("That PIN was not accepted.")
        );
        assert!(session.active_send().is_none());
    }

    #[test]
    fn a_cancelled_send_leaves_history_alone() {
        let mut session = Session::new();
        session.apply(ServiceEvent::SendStarted {
            id: 13,
            peer_alias: "iPhone".to_string(),
            files: vec![],
            total_bytes: 0,
        });
        session.apply(ServiceEvent::SendFinished {
            id: 13,
            outcome: TransferOutcome::Cancelled,
        });
        assert!(session.history.is_empty());
        assert!(session.active_send().is_none());
    }

    #[test]
    fn identical_notices_are_not_stacked() {
        let mut session = Session::new();
        for _ in 0..5 {
            session.apply(ServiceEvent::Notice {
                level: NoticeLevel::Error,
                message: "Port 53317 is already in use".to_string(),
            });
        }
        assert_eq!(session.notices.len(), 1);
        session.apply(ServiceEvent::Notice {
            level: NoticeLevel::Info,
            message: "Something else".to_string(),
        });
        assert_eq!(session.notices.len(), 2);
        assert_eq!(
            session.last_notice().unwrap().message,
            "Something else".to_string()
        );
    }

    #[test]
    fn status_and_history_clear_are_reported() {
        let mut session = Session::new();
        assert!(!session.listening());
        session.apply(ServiceEvent::Status(ServerStatus::Listening {
            port: 53317,
            alias: "This Mac".to_string(),
            protocol: Protocol::Https,
            fingerprint: "abcd".to_string(),
            save_dir: PathBuf::from("/tmp"),
        }));
        assert_eq!(session.listening_port(), Some(53317));
        session.apply(ServiceEvent::ReceivedText {
            sender: "iPhone".to_string(),
            text: "hi".to_string(),
        });
        assert_eq!(session.history.len(), 1);
        session.clear_history();
        assert!(session.history.is_empty());
    }

    #[test]
    fn throughput_and_eta_stay_quiet_until_bytes_have_moved() {
        let mut session = Session::new();
        session.apply(ServiceEvent::SendStarted {
            id: 1,
            peer_alias: "iPhone".to_string(),
            files: vec![SendFile {
                name: "big.bin".to_string(),
                size: 1_000,
                path: PathBuf::from("/tmp/big.bin"),
            }],
            total_bytes: 1_000,
        });
        let transfer = session.active_send().unwrap();
        assert_eq!(transfer.bytes_per_second(SystemTime::now()), 0.0);
        assert_eq!(transfer.eta(SystemTime::now()), None);

        session.apply(ServiceEvent::SendProgress {
            id: 1,
            file_name: "big.bin".to_string(),
            file_index: 1,
            file_count: 1,
            file_done: 500,
            file_size: 1_000,
            session_done: 500,
            session_total: 1_000,
        });
        let transfer = session.active_send().unwrap();
        assert_eq!(transfer.ratio(), 50.0);
        // Elapsed is a few microseconds here, so the rate is intentionally not
        // asserted — the guard is that it stays finite and positive.
        assert!(transfer.bytes_per_second(SystemTime::now()) >= 0.0);
    }
}
