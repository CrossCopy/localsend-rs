use sha2::Digest;

use crate::error::Result;

pub fn generate_fingerprint() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn sha256_from_bytes(data: &[u8]) -> String {
    let hash = sha2::Sha256::digest(data);
    format!("{:x}", hash)
}

pub async fn sha256_from_file(path: &std::path::Path) -> Result<String> {
    let contents = tokio::fs::read(path).await?;
    Ok(sha256_from_bytes(&contents))
}

#[cfg(feature = "https")]
pub struct TlsCertificate {
    pub cert_pem: String,
    pub key_pem: String,
    pub fingerprint: String,
}

#[cfg(feature = "https")]
pub fn generate_tls_certificate() -> Result<TlsCertificate> {
    use rcgen::generate_simple_self_signed;

    let cert = generate_simple_self_signed(vec!["localhost".to_string()]).map_err(|e| {
        crate::error::LocalSendError::network(format!("Failed to generate TLS certificate: {}", e))
    })?;

    let cert_der = cert.cert.der();
    let fingerprint = sha256_from_bytes(cert_der);

    Ok(TlsCertificate {
        cert_pem: cert.cert.pem(),
        key_pem: cert.signing_key.serialize_pem(),
        fingerprint,
    })
}
