//! The tokio↔GPUI bridge.
//!
//! The library is tokio-native: the receiver is axum, discovery is a udp socket
//! task, the client is reqwest. None of that can run on GPUI's executor, which is
//! a single-threaded futures executor with no I/O reactor. The app therefore owns
//! exactly one multi-thread tokio runtime and this struct is the only way UI code
//! touches it.
//!
//! Only `Send` data crosses the seam. GPUI entities, windows and contexts live on
//! the main thread and never enter a spawned future: the service reports back over
//! a `smol::channel`, whose receiver is runtime-agnostic and can be awaited from
//! `cx.spawn` on GPUI's executor.

use std::future::Future;

use tokio::runtime::Runtime;

/// The application's tokio runtime.
///
/// Held by the window for the life of the process. Dropping it tears down every
/// spawned task, which is what makes closing the window stop the listener.
pub struct HostRuntime {
    runtime: Runtime,
    handle: tokio::runtime::Handle,
}

impl HostRuntime {
    pub fn new() -> Result<Self, String> {
        let runtime = Runtime::new()
            .map_err(|error| format!("could not start the network runtime: {error}"))?;
        let handle = runtime.handle().clone();
        Ok(Self { runtime, handle })
    }

    /// A handle for spawning on this runtime from anywhere.
    pub fn handle(&self) -> tokio::runtime::Handle {
        self.handle.clone()
    }

    /// Spawn a future that reports its result over a channel the caller opened.
    pub fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.runtime.spawn(future);
    }
}
