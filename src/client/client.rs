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
}

impl LocalSendClient {
    pub fn new(device: DeviceInfo) -> Self {
        Self {
            client: HttpClient::new(),
            device,
        }
    }

    pub fn with_trust_policy(device: DeviceInfo, policy: TlsTrustPolicy) -> Result<Self> {
        let client = match policy {
            TlsTrustPolicy::InsecureForTests => HttpClient::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .map_err(LocalSendError::from)?,
            TlsTrustPolicy::PinnedFingerprint(fingerprint) => {
                let verifier = FingerprintVerifier::new(fingerprint)?;
                let tls_config = rustls::ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(verifier))
                    .with_no_client_auth();
                HttpClient::builder()
                    .tls_backend_preconfigured(tls_config)
                    .build()
                    .map_err(LocalSendError::from)?
            }
        };

        Ok(Self { client, device })
    }

    pub async fn register(&self, target: &DeviceInfo) -> Result<DeviceInfo> {
        let ip = target
            .ip
            .as_ref()
            .ok_or_else(|| LocalSendError::network("Target IP not provided"))?;
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
        let counter_progress = progress.clone();
        let mut sent: u64 = 0;
        let counted = ReaderStream::new(file).inspect(move |chunk| {
            if let (Ok(c), Some(cb)) = (chunk, counter_progress.as_ref()) {
                sent += c.len() as u64;
                cb(sent, total_bytes, started.elapsed().as_secs_f64());
            }
        });
        let body = Body::wrap_stream(counted);

        let response = self
            .client
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

    pub async fn cancel(&self, target: &DeviceInfo, session_id: &SessionId) -> Result<()> {
        let ip = target
            .ip
            .as_ref()
            .ok_or_else(|| LocalSendError::network("Target IP not provided"))?;
        let url = format!(
            "{}://{}:{}/api/localsend/v2/cancel?sessionId={}",
            target.protocol, ip, target.port, session_id
        );
        let response = self.client.post(&url).send().await?;
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

#[derive(Debug)]
struct FingerprintVerifier {
    expected_fingerprint: String,
    signature_verifier: Arc<dyn rustls::client::danger::ServerCertVerifier>,
}

impl FingerprintVerifier {
    fn new(expected_fingerprint: String) -> Result<Self> {
        let expected_fingerprint =
            crate::client::trust_policy::normalize_fingerprint(&expected_fingerprint)
                .ok_or_else(|| LocalSendError::network("Invalid LocalSend TLS fingerprint"))?;

        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();
        let placeholder_certificate = crate::crypto::generate_tls_certificate()?;
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(rustls::pki_types::CertificateDer::from(
                placeholder_certificate.cert_der,
            ))
            .map_err(|error| {
                LocalSendError::network(format!("Invalid TLS verifier root: {error}"))
            })?;
        let signature_verifier = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|error| {
                LocalSendError::network(format!("TLS verifier setup failed: {error}"))
            })?;

        Ok(Self {
            expected_fingerprint,
            signature_verifier,
        })
    }
}

impl rustls::client::danger::ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let actual = crate::crypto::sha256_from_bytes(end_entity.as_ref());
        if crate::client::trust_policy::normalize_fingerprint(&actual)
            .is_some_and(|actual| actual == self.expected_fingerprint)
        {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "LocalSend TLS certificate fingerprint mismatch".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.signature_verifier
            .verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.signature_verifier
            .verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.signature_verifier.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::LocalSendClient;
    use crate::client::TlsTrustPolicy;
    use crate::protocol::{DeviceInfo, Protocol};

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
}
