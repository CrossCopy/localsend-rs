use crate::client::trust_policy::TlsTrustPolicy;
use crate::error::{LocalSendError, Result};
use crate::protocol::{
    DeviceInfo, FileId, FileMetadata, PrepareUploadRequest, PrepareUploadResponse, SessionId, Token,
};
use futures_util::StreamExt;
use reqwest::{Body, Client as HttpClient, StatusCode};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::fs::File;
use tokio_util::io::ReaderStream;

pub type ProgressCallback = Box<dyn Fn(u64, u64, f64) + Send + Sync>;

#[derive(Clone)]
pub struct LocalSendClient {
    client: HttpClient,
    device: DeviceInfo,
    pinned_fingerprint: Option<String>,
    pinned_client: Arc<tokio::sync::Mutex<Option<HttpClient>>>,
    client_identity: Option<reqwest::Identity>,
}

impl LocalSendClient {
    /// **No proxy, and it is not only about speed.** A LocalSend peer is a
    /// device on this network segment, addressed by the IP its own announcement
    /// carried; an HTTP proxy is a thing for reaching the internet through, and
    /// sending a person's file to one because their system configuration names
    /// one would be sending it somewhere they never chose.
    ///
    /// It is also the difference between a send starting now and a send
    /// starting in twenty seconds. `reqwest::Client::new()` asks the platform
    /// for its proxy configuration, measured at **20.4 s** on macOS on
    /// 2026-08-22 against 0.8 ms for this builder — a cost paid on every client,
    /// and this crate builds one per call.
    pub fn new(device: DeviceInfo) -> Self {
        crate::crypto::ensure_crypto_provider();
        let client = HttpClient::builder()
            .no_proxy()
            .tls_backend_rustls()
            .build()
            .expect("a reqwest client with no proxy and no TLS override cannot fail to build");
        Self {
            client,
            device,
            pinned_fingerprint: None,
            pinned_client: Arc::new(tokio::sync::Mutex::new(None)),
            client_identity: None,
        }
    }

    pub fn with_trust_policy(device: DeviceInfo, policy: TlsTrustPolicy) -> Result<Self> {
        Self::with_trust_policy_and_identity(device, policy, None)
    }

    /// A client that presents `certificate` to the peer during the TLS
    /// handshake, for peers that require one.
    ///
    /// # Why this is not optional for some peers
    ///
    /// LocalSend's transport is mutually authenticated in practice even though
    /// the spec describes only the server side: current iOS builds request a
    /// client certificate and **abort the handshake** when none is offered, so
    /// a client without one cannot reach any endpoint on those peers — not even
    /// `/info`. Android builds accept a client certificate and do not require
    /// it, so presenting one is the behaviour that works everywhere.
    ///
    /// # This overrides `device.fingerprint`
    ///
    /// The returned client announces `certificate`'s fingerprint, replacing
    /// whatever `device` carried. In LocalSend a device *is* its certificate
    /// fingerprint: it is what a peer stores from `/register`, what it
    /// de-duplicates its device list on, and what it pins us to on later
    /// connections. Announcing a fingerprint we will never present would make
    /// this device unrecognisable the next time it connects — a new entry in
    /// the peer's list on every run, and a pin that can never match. Passing
    /// the two separately is exactly how they come to disagree, so they are
    /// taken from one place.
    #[cfg(feature = "https")]
    pub fn with_trust_policy_and_client_certificate(
        mut device: DeviceInfo,
        policy: TlsTrustPolicy,
        certificate: &crate::crypto::TlsCertificate,
    ) -> Result<Self> {
        device.fingerprint = certificate.fingerprint.clone();
        let pem = format!("{}\n{}", certificate.cert_pem, certificate.key_pem);
        let identity = reqwest::Identity::from_pem(pem.as_bytes()).map_err(LocalSendError::from)?;
        Self::with_trust_policy_and_identity(device, policy, Some(identity))
    }

    fn with_trust_policy_and_identity(
        device: DeviceInfo,
        policy: TlsTrustPolicy,
        client_identity: Option<reqwest::Identity>,
    ) -> Result<Self> {
        crate::crypto::ensure_crypto_provider();
        let pinned_fingerprint = match &policy {
            TlsTrustPolicy::PinnedFingerprint(fingerprint) => Some(
                crate::client::trust_policy::normalize_fingerprint(fingerprint)
                    .ok_or_else(|| LocalSendError::network("Invalid LocalSend TLS fingerprint"))?,
            ),
            TlsTrustPolicy::InsecureForTests => None,
        };
        let client = match policy {
            TlsTrustPolicy::InsecureForTests => {
                let mut builder = HttpClient::builder()
                    .no_proxy()
                    .tls_backend_rustls()
                    .danger_accept_invalid_certs(true);
                if let Some(identity) = client_identity.clone() {
                    builder = builder.identity(identity);
                }
                builder.build().map_err(LocalSendError::from)?
            }
            TlsTrustPolicy::PinnedFingerprint(_fingerprint) => {
                #[cfg(feature = "https")]
                {
                    let mut builder = HttpClient::builder()
                        .no_proxy()
                        .tls_backend_rustls()
                        // This client is used only for the certificate bootstrap
                        // in `client_for_target`; all payload requests use a
                        // client pinned to the exact leaf certificate.
                        .danger_accept_invalid_certs(true)
                        .tls_info(true);
                    if let Some(identity) = client_identity.clone() {
                        builder = builder.identity(identity);
                    }
                    builder.build().map_err(LocalSendError::from)?
                }

                #[cfg(not(feature = "https"))]
                {
                    let _ = _fingerprint;
                    return Err(LocalSendError::network(
                        "Pinned LocalSend TLS requires the https feature",
                    ));
                }
            }
        };

        Ok(Self {
            client,
            device,
            pinned_fingerprint,
            pinned_client: Arc::new(tokio::sync::Mutex::new(None)),
            client_identity,
        })
    }

    async fn client_for_target(&self, target: &DeviceInfo) -> Result<HttpClient> {
        let Some(expected_fingerprint) = self.pinned_fingerprint.as_deref() else {
            return Ok(self.client.clone());
        };

        if target.protocol != crate::protocol::Protocol::Https {
            return Ok(self.client.clone());
        }

        let mut pinned_client = self.pinned_client.lock().await;
        if let Some(client) = pinned_client.as_ref() {
            return Ok(client.clone());
        }

        let ip = target
            .ip
            .as_ref()
            .ok_or_else(|| LocalSendError::network("Target IP not provided"))?;
        let url = format!("https://{}:{}/api/localsend/v2/info", ip, target.port);
        let response = self.client.get(&url).send().await?;
        let certificate = response
            .extensions()
            .get::<reqwest::tls::TlsInfo>()
            .and_then(reqwest::tls::TlsInfo::peer_certificate)
            .ok_or_else(|| LocalSendError::network("LocalSend TLS peer certificate unavailable"))?;

        let actual_fingerprint = crate::crypto::sha256_from_bytes(certificate);
        if crate::client::trust_policy::normalize_fingerprint(&actual_fingerprint)
            .is_none_or(|actual| actual != expected_fingerprint)
        {
            return Err(LocalSendError::network(
                "LocalSend TLS certificate fingerprint mismatch",
            ));
        }

        let certificate = reqwest::Certificate::from_der(certificate).map_err(|error| {
            LocalSendError::network(format!("Invalid LocalSend TLS certificate: {error}"))
        })?;
        let mut builder = HttpClient::builder()
            .no_proxy()
            .tls_backend_rustls()
            .tls_certs_only([certificate])
            // LocalSend certificates are self-signed for the peer, not for
            // the numeric IP address used by the discovery announcement.
            .danger_accept_invalid_hostnames(true);
        if let Some(identity) = self.client_identity.clone() {
            builder = builder.identity(identity);
        }
        let client = builder.build().map_err(LocalSendError::from)?;
        *pinned_client = Some(client.clone());
        Ok(client)
    }

    pub async fn register(&self, target: &DeviceInfo) -> Result<DeviceInfo> {
        let client = self.client_for_target(target).await?;
        let ip = target
            .ip
            .as_ref()
            .ok_or_else(|| LocalSendError::network("Target IP not provided"))?;
        let url = format!(
            "{}://{}:{}/api/localsend/v2/register",
            target.protocol, ip, target.port
        );

        let response = client.post(&url).json(&self.device).send().await?;
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
            Err(LocalSendError::Rejected {
                status: status.as_u16(),
            })
        } else {
            Err(LocalSendError::http_failed(
                status.as_u16(),
                "Registration failed",
            ))
        }
    }

    pub async fn prepare_upload(
        &self,
        target: &DeviceInfo,
        files: HashMap<FileId, FileMetadata>,
        pin: Option<&str>,
    ) -> Result<PrepareUploadResponse> {
        let client = self.client_for_target(target).await?;
        let ip = target
            .ip
            .as_ref()
            .ok_or_else(|| LocalSendError::network("Target IP not provided"))?;
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

        let response = client.post(&url).json(&request).send().await?;

        let status = response.status();
        match status {
            StatusCode::OK => {
                let upload_response: PrepareUploadResponse = response.json().await?;
                Ok(upload_response)
            }
            StatusCode::NO_CONTENT => {
                // This happens when sending text messages or if the receiver accepted the metadata but needs no file transfer
                Ok(PrepareUploadResponse {
                    session_id: SessionId::from_string(String::new()),
                    files: HashMap::new(),
                })
            }
            StatusCode::UNAUTHORIZED => Err(LocalSendError::InvalidPin),
            StatusCode::FORBIDDEN => Err(LocalSendError::Rejected {
                status: status.as_u16(),
            }),
            StatusCode::CONFLICT => Err(LocalSendError::SessionBlocked),
            StatusCode::TOO_MANY_REQUESTS => Err(LocalSendError::RateLimited),
            StatusCode::INTERNAL_SERVER_ERROR => Err(LocalSendError::network("Server error")),
            _ => Err(LocalSendError::http_failed(
                status.as_u16(),
                "Prepare upload failed",
            )),
        }
    }

    pub async fn upload_file(
        &self,
        target: &DeviceInfo,
        session_id: &SessionId,
        file_id: &FileId,
        token: &Token,
        file_path: &std::path::Path,
        progress: Option<ProgressCallback>,
    ) -> Result<()> {
        self.upload_file_with_rate_limit(
            target, session_id, file_id, token, file_path, progress, None,
        )
        .await
    }

    /// Uploads a file while optionally pacing the source stream. The rate
    /// limit is intended for deterministic integration tests; normal callers
    /// should use [`Self::upload_file`].
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_file_with_rate_limit(
        &self,
        target: &DeviceInfo,
        session_id: &SessionId,
        file_id: &FileId,
        token: &Token,
        file_path: &std::path::Path,
        progress: Option<ProgressCallback>,
        rate_limit_bytes_per_second: Option<u64>,
    ) -> Result<()> {
        let client = self.client_for_target(target).await?;
        let ip = target
            .ip
            .as_ref()
            .ok_or_else(|| LocalSendError::network("Target IP not provided"))?;
        let url = format!(
            "{}://{}:{}/api/localsend/v2/upload?sessionId={}&fileId={}&token={}",
            target.protocol, ip, target.port, session_id, file_id, token
        );

        // Stream the file instead of loading it all into memory
        let file = File::open(file_path).await?;
        let total_bytes = file.metadata().await?.len();
        let started = std::time::Instant::now();
        let progress = progress.map(std::sync::Arc::new);

        // Wrap the file stream so every chunk that goes out over the wire
        // also advances a running byte counter and reports it upstream.
        let throttle_started = tokio::time::Instant::now();
        let throttled_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let throttle_counter = throttled_bytes.clone();
        let rate_limit_bytes_per_second = rate_limit_bytes_per_second.filter(|rate| *rate > 0);
        let paced = ReaderStream::new(file).then(move |chunk| {
            let target_elapsed = chunk.as_ref().ok().and_then(|bytes| {
                let cumulative = throttle_counter
                    .fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed)
                    + bytes.len() as u64;
                rate_limit_bytes_per_second
                    .map(|rate| std::time::Duration::from_secs_f64(cumulative as f64 / rate as f64))
            });
            async move {
                if let Some(target_elapsed) = target_elapsed {
                    let delay = target_elapsed.saturating_sub(throttle_started.elapsed());
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                }
                chunk
            }
        });

        let counter_progress = progress.clone();
        let mut sent: u64 = 0;
        let counted = paced.inspect(move |chunk| {
            if let (Ok(c), Some(cb)) = (chunk, counter_progress.as_ref()) {
                sent += c.len() as u64;
                cb(sent, total_bytes, started.elapsed().as_secs_f64());
            }
        });
        let body = Body::wrap_stream(counted);

        let response = client
            .post(&url)
            .header(reqwest::header::CONTENT_LENGTH, total_bytes)
            .body(body)
            .send()
            .await?;

        let status = response.status();
        match status {
            StatusCode::OK | StatusCode::NO_CONTENT => Ok(()),
            _ => Err(LocalSendError::http_failed(
                status.as_u16(),
                "File upload failed",
            )),
        }
    }

    /// Uploads a body already in memory, for a payload that has no file.
    ///
    /// **A text message is the case this exists for.** Most receivers answer a
    /// message with 204 and never ask for bytes — the whole body travelled in
    /// the offer's `preview` — but one that *does* open a session leaves a
    /// sender with a string and an endpoint that wants a file. Writing the
    /// message to a temporary file to satisfy that is what the CLI does, and it
    /// puts a person's private text on disk, unasked, on the sending machine.
    pub async fn upload_bytes(
        &self,
        target: &DeviceInfo,
        session_id: &SessionId,
        file_id: &FileId,
        token: &Token,
        body: Vec<u8>,
    ) -> Result<()> {
        let client = self.client_for_target(target).await?;
        let ip = target
            .ip
            .as_ref()
            .ok_or_else(|| LocalSendError::network("Target IP not provided"))?;
        let url = format!(
            "{}://{}:{}/api/localsend/v2/upload?sessionId={}&fileId={}&token={}",
            target.protocol, ip, target.port, session_id, file_id, token
        );

        let length = body.len() as u64;
        let response = client
            .post(&url)
            .header(reqwest::header::CONTENT_LENGTH, length)
            .body(body)
            .send()
            .await?;

        let status = response.status();
        match status {
            StatusCode::OK | StatusCode::NO_CONTENT => Ok(()),
            _ => Err(LocalSendError::http_failed(
                status.as_u16(),
                "Upload failed",
            )),
        }
    }

    pub async fn cancel(&self, target: &DeviceInfo, session_id: &SessionId) -> Result<()> {
        let client = self.client_for_target(target).await?;
        let ip = target
            .ip
            .as_ref()
            .ok_or_else(|| LocalSendError::network("Target IP not provided"))?;
        let url = format!(
            "{}://{}:{}/api/localsend/v2/cancel?sessionId={}",
            target.protocol, ip, target.port, session_id
        );
        let response = client.post(&url).send().await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(LocalSendError::http_failed(
                response.status().as_u16(),
                "Cancel failed",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LocalSendClient;
    use crate::client::TlsTrustPolicy;
    use crate::protocol::{DeviceInfo, Protocol};

    #[cfg(feature = "https")]
    #[test]
    fn with_trust_policy_keeps_strict_policy_insecure_flag() {
        let device = DeviceInfo::new("alias".to_string(), 53317, Protocol::Https);
        let policy = TlsTrustPolicy::new(vec!["a".repeat(64)]);

        let client = LocalSendClient::with_trust_policy(device, policy.clone()).unwrap();

        assert!(!policy.allows_insecure());
        assert!(!policy.allows(""));
        // Client must construct without panicking and remain usable for the device payload.
        assert_eq!(client.device.alias, "alias");
    }

    #[cfg(not(feature = "https"))]
    #[test]
    fn pinned_policy_requires_the_https_feature() {
        let device = DeviceInfo::new("alias".to_string(), 53317, Protocol::Https);
        let policy = TlsTrustPolicy::new(vec!["a".repeat(64)]);

        assert!(matches!(
            LocalSendClient::with_trust_policy(device, policy),
            Err(error) if error.to_string().contains("https feature")
        ));
    }
}
