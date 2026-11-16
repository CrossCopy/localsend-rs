//! The GPUI entity the views read.
//!
//! One entity holds the [`Session`] state machine and the service connection.
//! Views never touch the service directly: they read a snapshot out of this store
//! and call its command methods, which is what keeps the transfer path testable
//! without a window.
//!
//! # Two sinks
//!
//! A normal store owns a [`ServiceHandle`] and pushes [`Command`]s into the tokio
//! controller. A *detached* store (built by `TransferStore::detached`, used by the
//! UI tests and by any view that wants to render scripted data) records the
//! commands instead, so a test can click "Accept" and assert the app asked for
//! exactly that — with no sockets, no files and no runtime.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use gpui_kit::*;

use crate::service::{Command, Peer, ServiceEvent, ServiceHandle, Settings};
use crate::session::Session;

/// Where commands go.
enum Sink {
    Service(Arc<ServiceHandle>),
    /// Commands this store would have sent. Read by tests with `take_commands`.
    Recorded(Arc<Mutex<Vec<Command>>>),
}

/// How often the shell re-checks peer freshness with no other events arriving.
const PRUNE_INTERVAL: Duration = Duration::from_secs(10);

pub struct TransferStore {
    /// The window state. Public so a view can read it without a lock dance.
    pub session: Session,
    sink: Sink,
    _pump: Option<Task<()>>,
    _prune: Option<Task<()>>,
}

impl TransferStore {
    /// A store wired to the running service: events flow in, commands flow out.
    pub fn new(service: ServiceHandle, cx: &mut Context<Self>) -> Self {
        let service = Arc::new(service);
        let events = service.events().clone();
        let pump = cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                if this
                    .update(cx, |store, cx| store.apply_event(event, cx))
                    .is_err()
                {
                    // The window is gone; nothing left to update.
                    break;
                }
            }
        });
        let prune = cx.spawn(async move |this, cx| loop {
            smol::Timer::after(PRUNE_INTERVAL).await;
            let alive = this
                .update(cx, |store, cx| {
                    if store.session.prune_peers(SystemTime::now()) {
                        cx.notify();
                    }
                })
                .is_ok();
            if !alive {
                break;
            }
        });
        Self {
            session: Session::new(),
            sink: Sink::Service(service),
            _pump: Some(pump),
            _prune: Some(prune),
        }
    }

    /// A store with no service behind it. Commands are recorded, not sent; events
    /// are applied by whoever holds it.
    pub fn detached() -> Self {
        Self {
            session: Session::new(),
            sink: Sink::Recorded(Arc::default()),
            _pump: None,
            _prune: None,
        }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Whether commands reach a real service. A detached store records them for a
    /// test; it must not, say, write the settings file as a side effect of a click.
    pub fn is_live(&self) -> bool {
        matches!(self.sink, Sink::Service(_))
    }

    /// A cloneable handle for background operations such as the update barrier.
    /// Detached stores have no controller and return `None`.
    pub fn service_handle(&self) -> Option<Arc<ServiceHandle>> {
        match &self.sink {
            Sink::Service(service) => Some(service.clone()),
            Sink::Recorded(_) => None,
        }
    }

    /// Turn one service event into window state. The single write path: the pump
    /// and the tests both come through here.
    pub fn apply_event(&mut self, event: ServiceEvent, cx: &mut Context<Self>) {
        self.session.apply(event);
        cx.notify();
    }

    /// Commands this store recorded instead of sending. Empty for a live store.
    pub fn take_commands(&self) -> Vec<Command> {
        match &self.sink {
            Sink::Service(_) => Vec::new(),
            Sink::Recorded(commands) => std::mem::take(
                &mut *commands
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            ),
        }
    }

    fn command(&self, command: Command) -> bool {
        match &self.sink {
            Sink::Service(service) => service.command(command),
            Sink::Recorded(commands) => {
                commands
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(command);
                true
            }
        }
    }

    // ------------------------------------------------------------------ commands

    pub fn accept(&self, id: crate::service::TransferId) -> bool {
        self.command(Command::Accept { id, files: None })
    }

    pub fn accept_only(&self, id: crate::service::TransferId, files: Vec<String>) -> bool {
        self.command(Command::Accept {
            id,
            files: Some(files),
        })
    }

    pub fn decline(&self, id: crate::service::TransferId) -> bool {
        self.command(Command::Decline { id })
    }

    pub fn refresh_peers(&self) -> bool {
        self.command(Command::RefreshPeers)
    }

    /// Re-enumerate the local IPv4 interfaces without restarting the receiver.
    pub fn refresh_network_interfaces(&self) -> bool {
        self.command(Command::RefreshInterfaces)
    }

    /// Start a subnet scan sourced from a selected local interface.
    pub fn scan_selected_interface(&self, address: std::net::Ipv4Addr) -> bool {
        self.command(Command::ScanSubnet { address })
    }

    pub fn send(&self, peer: Peer, files: Vec<PathBuf>, pin: Option<String>) -> bool {
        self.command(Command::Send { peer, files, pin })
    }

    pub fn cancel_send(&self, id: crate::service::TransferId) -> bool {
        self.command(Command::CancelSend { id })
    }

    pub fn set_auto_accept(&self, yes: bool) -> bool {
        self.command(Command::SetAutoAccept(yes))
    }

    /// Replace the trusted-device list live. Starring a device must not drop the
    /// listener, so this is a command of its own rather than a restart.
    pub fn set_favorites(&self, favorites: Vec<String>) -> bool {
        self.command(Command::SetFavorites(favorites))
    }

    /// Rebuild the receiver from `settings`. The settings file is written by the
    /// caller; the service only learns the new values.
    pub fn apply_settings(&self, settings: Settings) -> bool {
        self.command(Command::Restart(Box::new(settings)))
    }
}

#[cfg(test)]
mod tests {
    use super::TransferStore;
    use crate::service::Command;
    use std::net::Ipv4Addr;

    #[test]
    fn selected_interface_refresh_and_scan_are_direct_service_commands() {
        let store = TransferStore::detached();
        let address = Ipv4Addr::new(192, 168, 1, 10);

        assert!(store.refresh_network_interfaces());
        assert!(store.scan_selected_interface(address));
        assert_eq!(
            store.take_commands(),
            vec![Command::RefreshInterfaces, Command::ScanSubnet { address },]
        );
    }
}
