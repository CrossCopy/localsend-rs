use crate::error::{LocalSendError, Result};
use crate::protocol::{DeviceInfo, PrepareUploadRequest, PrepareUploadResponse};
use reqwest::{Client as HttpClient, StatusCode};
use std::collections::HashMap;

pub type ProgressCallback = Box<dyn Fn(u64, u64, f64) + Send + Sync>;

#[derive(Clone)]
pub struct LocalSendClient {
    client: HttpClient,
    device: DeviceInfo,
}

impl LocalSendClient {
    pub fn new(device: DeviceInfo) -> Self {
        Self {
            client: HttpClient::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .unwrap_or_else(|_| HttpClient::new()),
            device,
        }
    }

    pub async fn register(&self, target: &DeviceInfo) -> Result<DeviceInfo> {
        let ip = target
            .ip
            .as_ref()
            .ok_or_else(|| LocalSendError::Network("Target IP not provided".to_string()))?;
        let url = format!(
            "{}://{}:{}/api/localsend/v2/register",
            target.protocol, ip, target.port
        );

        let response = self.client.post(&url).json(&self.device).send().await?;
        let status = response.status();

        if status.is_success() {
            let bytes = response.bytes().await?;
            if bytes.is_empty() {
                return Ok(target.clone());
            }

            match serde_json::from_slice::<DeviceInfo>(&bytes) {
                Ok(info) => Ok(info),
                Err(_e) => {
                    // If we successfully posted our info (200 OK) but can't parse the response,
                    // we still consider registration successful because the other device received our info.
                    // This often happens if the other device sends a slightly different JSON format.
                    Ok(target.clone())
                }
            }
        } else if status == 401 || status == 403 {
            Err(LocalSendError::Rejected(status.as_u16()))
        } else {
            Err(LocalSendError::HttpFailed(status.as_u16()))
        }
    }

    pub async fn prepare_upload(
        &self,
        target: &DeviceInfo,
        files: HashMap<String, crate::protocol::FileMetadata>,
        pin: Option<&str>,
    ) -> Result<PrepareUploadResponse> {
        let ip = target
            .ip
            .as_ref()
            .ok_or_else(|| LocalSendError::Network("Target IP not provided".to_string()))?;
        let mut url = format!(
            "{}://{}:{}/api/localsend/v2/prepare-upload",
            target.protocol, ip, target.port
        );

        if let Some(pin_value) = pin {
            url = format!("{}?pin={}", url, pin_value);
        }

        let request = PrepareUploadRequest {
            info: self.device.clone(),
            files,
        };

        let response = self.client.post(&url).json(&request).send().await?;

        let status = response.status();
        match status {
            StatusCode::OK => {
                let upload_response: PrepareUploadResponse = response.json().await?;
                Ok(upload_response)
            }
            StatusCode::NO_CONTENT => {
                // This happens when sending text messages or if the receiver accepted the metadata but needs no file transfer
                Ok(PrepareUploadResponse {
                    session_id: String::new(),
                    files: HashMap::new(),
                })
            }
            StatusCode::UNAUTHORIZED => Err(LocalSendError::InvalidPin),
            StatusCode::FORBIDDEN => Err(LocalSendError::Rejected(status.as_u16())),
            StatusCode::CONFLICT => Err(LocalSendError::SessionBlocked),
            StatusCode::TOO_MANY_REQUESTS => Err(LocalSendError::RateLimited),
            StatusCode::INTERNAL_SERVER_ERROR => {
                Err(LocalSendError::Network("Server error".to_string()))
            }
            _ => Err(LocalSendError::HttpFailed(status.as_u16())),
        }
    }

    pub async fn upload_file(
        &self,
        target: &DeviceInfo,
        session_id: &str,
        file_id: &str,
        token: &str,
        file_path: &std::path::Path,
        _progress: Option<ProgressCallback>,
    ) -> Result<()> {
        let ip = target
            .ip
            .as_ref()
            .ok_or_else(|| LocalSendError::Network("Target IP not provided".to_string()))?;
        let url = format!(
            "{}://{}:{}/api/localsend/v2/upload?sessionId={}&fileId={}&token={}",
            target.protocol, ip, target.port, session_id, file_id, token
        );

        let file_bytes = tokio::fs::read(file_path).await?;
        let _total_bytes = file_bytes.len();

        let response = self.client.post(&url).body(file_bytes).send().await?;

        let status = response.status();
        match status {
            StatusCode::OK | StatusCode::NO_CONTENT => Ok(()),
            _ => Err(LocalSendError::HttpFailed(status.as_u16())),
        }
    }
}
