use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

// ============================================================================
// Newtype Patterns for Type Safety
// ============================================================================

/// Protocol type for LocalSend communication
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Http,
    Https,
}

impl Protocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Protocol::Http => "http",
            Protocol::Https => "https",
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl From<&str> for Protocol {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "https" => Protocol::Https,
            _ => Protocol::Http,
        }
    }
}

impl From<String> for Protocol {
    fn from(s: String) -> Self {
        Protocol::from(s.as_str())
    }
}

/// Session identifier for file transfer sessions
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn from_string(s: String) -> Self {
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// File identifier for individual files in a transfer
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileId(String);

impl FileId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn from_string(s: String) -> Self {
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for FileId {
    fn default() -> Self {
        Self::new()
    }
}

/// Authorization token for file uploads
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Token(String);

impl Token {
    pub fn new(session_id: &SessionId, file_id: &FileId) -> Self {
        Self(format!("{}_{}", session_id.as_str(), file_id.as_str()))
    }

    pub fn from_string(s: String) -> Self {
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Network port with validation
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Port(u16);

impl Port {
    pub fn new(port: u16) -> Result<Self, crate::error::LocalSendError> {
        if port == 0 {
            return Err(crate::error::LocalSendError::InvalidPort(
                "Port cannot be 0".to_string(),
            ));
        }
        Ok(Port(port))
    }

    pub fn new_unchecked(port: u16) -> Self {
        Port(port)
    }

    pub fn get(&self) -> u16 {
        self.0
    }
}

impl fmt::Display for Port {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for Port {
    fn default() -> Self {
        Port(crate::protocol::constants::DEFAULT_HTTP_PORT)
    }
}

// ============================================================================
// Device Types
// ============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum DeviceType {
    Mobile,
    #[default]
    Desktop,
    Web,
    Headless,
    Server,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub alias: String,
    pub version: String,
    #[serde(rename = "deviceModel")]
    pub device_model: Option<String>,
    #[serde(rename = "deviceType")]
    pub device_type: Option<DeviceType>,
    pub fingerprint: String,
    pub port: u16,
    pub protocol: Protocol,
    pub download: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileMetadata {
    pub id: FileId,
    #[serde(rename = "fileName")]
    pub file_name: String,
    pub size: u64,
    #[serde(rename = "fileType")]
    pub file_type: String,
    pub sha256: Option<String>,
    pub preview: Option<String>,
    pub metadata: Option<FileMetadataDetails>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileMetadataDetails {
    pub modified: Option<String>,
    pub accessed: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrepareUploadRequest {
    pub info: DeviceInfo,
    pub files: HashMap<FileId, FileMetadata>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrepareUploadResponse {
    #[serde(rename = "sessionId")]
    pub session_id: SessionId,
    pub files: HashMap<FileId, Token>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReceivedFile {
    pub file_name: String,
    pub size: u64,
    pub sender: String,
    pub time: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnnouncementMessage {
    pub alias: String,
    pub version: String,
    #[serde(rename = "deviceModel")]
    pub device_model: Option<String>,
    #[serde(rename = "deviceType")]
    pub device_type: Option<DeviceType>,
    pub fingerprint: String,
    pub port: u16,
    pub protocol: Protocol,
    pub download: bool,
    #[serde(default)]
    pub announce: bool,
    #[serde(default)]
    pub announcement: Option<bool>,
}

pub type RegisterMessage = DeviceInfo;

impl DeviceInfo {
    pub fn new(alias: String, port: u16, protocol: Protocol) -> Self {
        Self {
            alias,
            version: crate::protocol::constants::PROTOCOL_VERSION.to_string(),
            device_model: None,
            device_type: None,
            fingerprint: String::new(),
            port,
            protocol,
            download: false,
            ip: None,
        }
    }

    // This function is added based on the provided "Code Edit" and instruction.
    // It assumes `_src` is a type that has an `ip()` method returning an IP address.
    // For example, `std::net::SocketAddr` or similar.
    pub fn from_announcement<T: std::net::ToSocketAddrs>(
        announcement: AnnouncementMessage,
        _src: T,
    ) -> Self {
        let socket_addr = _src.to_socket_addrs().ok().and_then(|mut i| i.next());
        Self {
            alias: announcement.alias,
            version: announcement.version,
            device_model: announcement.device_model,
            device_type: announcement.device_type,
            fingerprint: announcement.fingerprint,
            port: announcement.port,
            protocol: announcement.protocol,
            download: announcement.download,
            ip: socket_addr.map(|s| s.ip().to_string()),
        }
    }
}
