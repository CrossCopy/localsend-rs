use thiserror::Error;

#[derive(Error, Debug)]
pub enum LocalSendError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serde JSON error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("HTTP client error: {0}")]
    Reqwest(#[from] reqwest::Error),

    #[error("Address parse error: {0}")]
    AddrParse(#[from] std::net::AddrParseError),

    #[error("Network error: {0}")]
    Network(String),

    #[error("Invalid device info: {0}")]
    InvalidDevice(String),

    #[error("Invalid file metadata: {0}")]
    InvalidFile(String),

    #[error("Invalid token or session ID")]
    InvalidToken,

    #[error("Request rejected by receiver (HTTP {0})")]
    Rejected(u16),

    #[error("Request failed with HTTP {0}")]
    HttpFailed(u16),

    #[error("Session blocked by another transfer")]
    SessionBlocked,

    #[error("Invalid PIN")]
    InvalidPin,

    #[error("Too many requests")]
    RateLimited,
}

pub type Result<T> = std::result::Result<T, LocalSendError>;
