//! Prelude module for convenient imports
//!
//! Use `use localsend_rs::prelude::*;` to import commonly used types

// Core types
pub use crate::core::{
    build_file_metadata, build_file_metadata_from_bytes, generate_file_id, get_device_model,
    get_device_type, get_local_ip, get_mime_type, DeviceInfoBuilder, Session, TransferState,
};

// Protocol types
pub use crate::protocol::{
    validate_device_info, validate_file_metadata, validate_protocol_version, DeviceInfo,
    DeviceType, FileId, FileMetadata, Port, PrepareUploadRequest, PrepareUploadResponse, Protocol,
    ReceivedFile, RegisterMessage, SessionId, Token, DEFAULT_HTTP_PORT, DEFAULT_MULTICAST_ADDRESS,
    DEFAULT_MULTICAST_PORT, PROTOCOL_VERSION,
};

// Crypto
pub use crate::crypto::{generate_fingerprint, sha256_from_bytes, sha256_from_file};

#[cfg(feature = "https")]
pub use crate::crypto::{generate_tls_certificate, TlsCertificate};

// Client & Server
pub use crate::client::LocalSendClient;
pub use crate::server::LocalSendServer;

// Discovery
pub use crate::discovery::{Discovery, HttpDiscovery, MulticastDiscovery};

// Storage
pub use crate::storage::{FileSystem, TokioFileSystem};

// Error handling
pub use crate::error::{LocalSendError, Result};
