pub mod fingerprint;
pub mod hash;
pub mod tls;

pub use fingerprint::generate_fingerprint;
pub use hash::{sha256_from_bytes, sha256_from_file};

/// Install rustls' **ring** provider as the process-wide default, once.
///
/// Call this before constructing any `reqwest::Client` or rustls config.
///
/// Every rustls-facing dependency of this crate is pinned to its
/// "-no-provider" spelling so that `aws-lc-rs` — ~1.2 MiB of assembly that no
/// code path here executes — stays out of the binary. The trade-off is that
/// reqwest then has no built-in provider to fall back on: its
/// `ClientBuilder::build()` **panics** if the process default is unset. This
/// function is the guard against that, so it must run ahead of every client or
/// server construction.
///
/// Idempotent and thread-safe. `install_default()` returns `Err` once a
/// provider is already installed, which is the expected steady state.
pub fn ensure_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(feature = "https")]
pub use tls::{TlsCertificate, generate_tls_certificate};
