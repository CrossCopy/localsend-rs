pub mod client;
pub mod core;
pub mod crypto;
pub mod discovery;
pub mod error;
pub mod prelude;
pub mod protocol;
pub mod server;
pub mod storage;

// Re-export commonly used types for backwards compatibility
pub use client::LocalSendClient;
pub use core::{
    DeviceInfoBuilder, Session, TransferState, build_file_metadata, build_file_metadata_from_bytes,
    generate_file_id, get_device_model, get_device_type, get_local_ip, get_mime_type,
};
#[cfg(feature = "https")]
pub use crypto::{TlsCertificate, generate_tls_certificate};
pub use crypto::{generate_fingerprint, sha256_from_bytes, sha256_from_file};
pub use discovery::{Discovery, HttpDiscovery, MulticastDiscovery};
pub use error::{LocalSendError, Result};
pub use protocol::{
    AnnouncementMessage, DEFAULT_HTTP_PORT, DEFAULT_MULTICAST_ADDRESS, DEFAULT_MULTICAST_PORT,
    DeviceInfo, DeviceType, FileId, FileMetadata, PROTOCOL_VERSION, Port, PrepareUploadRequest,
    PrepareUploadResponse, Protocol, ReceivedFile, RegisterMessage, SessionId, Token,
    validate_device_info, validate_file_metadata, validate_protocol_version,
};
pub use server::LocalSendServer;
pub use storage::{FileSystem, TokioFileSystem};

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(feature = "tui")]
pub mod tui;
