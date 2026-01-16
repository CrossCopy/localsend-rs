pub mod client;
pub mod crypto;
pub mod device;
pub mod discovery;
pub mod error;
pub mod file;
pub mod protocol;
pub mod server;

pub use client::LocalSendClient;
#[cfg(feature = "https")]
pub use crypto::{TlsCertificate, generate_tls_certificate};
pub use crypto::{generate_fingerprint, sha256_from_bytes, sha256_from_file};
pub use device::{get_device_model, get_device_type, get_local_ip};
pub use discovery::{Discovery, HttpDiscovery, MulticastDiscovery};
pub use error::{LocalSendError, Result};
pub use file::{
    build_file_metadata, build_file_metadata_from_bytes, generate_file_id, get_mime_type,
};
pub use protocol::{
    AnnouncementMessage, DeviceInfo, DeviceType, FileId, FileMetadata, Port, PrepareUploadRequest,
    PrepareUploadResponse, Protocol, RegisterMessage, SessionId, Token,
};
pub use protocol::{
    DEFAULT_HTTP_PORT, DEFAULT_MULTICAST_ADDRESS, DEFAULT_MULTICAST_PORT, PROTOCOL_VERSION,
    validate_device_info, validate_file_metadata, validate_protocol_version,
};
pub use server::LocalSendServer;

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(feature = "tui")]
pub mod tui;
