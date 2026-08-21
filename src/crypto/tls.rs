#[cfg(feature = "https")]
use crate::error::Result;

/// A self-signed certificate and the fingerprint that names it.
///
/// `Clone` because a caller that both serves this certificate and announces its
/// fingerprint holds it in two places, and the alternative — passing the PEM
/// strings separately — is how the served certificate and the announced
/// fingerprint come to disagree.
#[cfg(feature = "https")]
#[derive(Clone)]
pub struct TlsCertificate {
    pub cert_pem: String,
    pub key_pem: String,
    pub cert_der: Vec<u8>,
    pub fingerprint: String,
}

#[cfg(feature = "https")]
pub fn generate_tls_certificate() -> Result<TlsCertificate> {
    use rcgen::generate_simple_self_signed;

    let cert = generate_simple_self_signed(vec!["localhost".to_string()]).map_err(|e| {
        crate::error::LocalSendError::network(format!("Failed to generate TLS certificate: {}", e))
    })?;

    let cert_der = cert.cert.der().to_vec();
    let fingerprint = super::hash::sha256_from_bytes(&cert_der);

    Ok(TlsCertificate {
        cert_pem: cert.cert.pem(),
        key_pem: cert.signing_key.serialize_pem(),
        cert_der,
        fingerprint,
    })
}

/// Rebuilds a certificate from PEM that was written out earlier.
///
/// # Why this exists rather than a caller parsing the PEM
///
/// The fingerprint is SHA-256 of the certificate DER, and a peer *pins* it. A
/// caller that persists a certificate across restarts and recomputes the
/// fingerprint itself has two derivations of one value in two crates, and the
/// day they disagree is the day every peer that remembered this device stops
/// trusting it. So the derivation stays in the one place that generated it:
/// this returns the same struct [`generate_tls_certificate`] returns, with the
/// fingerprint computed the same way from the same bytes.
#[cfg(feature = "https")]
pub fn tls_certificate_from_pem(cert_pem: String, key_pem: String) -> Result<TlsCertificate> {
    let mut reader = std::io::BufReader::new(cert_pem.as_bytes());
    let cert_der = rustls_pemfile::certs(&mut reader)
        .next()
        .transpose()
        .map_err(|e| {
            crate::error::LocalSendError::network(format!("Malformed certificate PEM: {}", e))
        })?
        .ok_or_else(|| {
            crate::error::LocalSendError::network("Certificate PEM contains no certificate")
        })?
        .to_vec();
    // The key is not parsed here: it is handed to the TLS stack as PEM, and a
    // key that does not match the certificate fails at bind, loudly, rather
    // than being announced and then refusing every connection.
    let fingerprint = super::hash::sha256_from_bytes(&cert_der);
    Ok(TlsCertificate {
        cert_pem,
        key_pem,
        cert_der,
        fingerprint,
    })
}

#[cfg(all(test, feature = "https"))]
mod tests {
    use super::*;

    #[test]
    fn a_certificate_read_back_from_pem_keeps_its_fingerprint() {
        // The property a peer's pin depends on: persisting and reloading is not
        // a new device.
        let generated = generate_tls_certificate().expect("generating");
        let reloaded =
            tls_certificate_from_pem(generated.cert_pem.clone(), generated.key_pem.clone())
                .expect("reloading");
        assert_eq!(reloaded.fingerprint, generated.fingerprint);
        assert_eq!(reloaded.cert_der, generated.cert_der);
    }

    #[test]
    fn pem_with_no_certificate_is_an_error_rather_than_a_fingerprint_of_nothing() {
        let error = tls_certificate_from_pem("not a certificate".to_string(), String::new());
        assert!(error.is_err());
    }
}
