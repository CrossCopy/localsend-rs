use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceType {
    Mobile,
    Desktop,
    Web,
    Headless,
    Server,
}

impl Default for DeviceType {
    fn default() -> Self {
        Self::Desktop
    }
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
    pub protocol: String,
    pub download: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileMetadata {
    pub id: String,
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
    pub files: HashMap<String, FileMetadata>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrepareUploadResponse {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub files: HashMap<String, String>,
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
    pub protocol: String,
    pub download: bool,
    #[serde(default)]
    pub announce: bool,
    #[serde(default)]
    pub announcement: Option<bool>,
}

pub type RegisterMessage = DeviceInfo;

impl DeviceInfo {
    pub fn new(alias: String, port: u16, protocol: String) -> Self {
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
