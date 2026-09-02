#[cfg(feature = "https")]
use crate::error::Result;

#[cfg(feature = "https")]
use std::path::{Path, PathBuf};

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
    use rustls::pki_types::{CertificateDer, pem::PemObject};

    let cert_der = CertificateDer::from_pem_slice(cert_pem.as_bytes())
        .map_err(|e| {
            crate::error::LocalSendError::network(format!("Malformed certificate PEM: {}", e))
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

/// Whether `certificate`'s private key is actually the key for its certificate.
///
/// [`tls_certificate_from_pem`] deliberately does not parse the key, which is
/// the right trade for a pair that was just generated: it fails at bind. A pair
/// read back from disk is different — nothing regenerates it, so an
/// inconsistent pair fails at *every* bind from then on. This is the same check
/// the TLS stack makes when the server binds, run early enough to do something
/// about it.
#[cfg(feature = "https")]
fn is_self_consistent(certificate: &TlsCertificate) -> bool {
    use rustls::ServerConfig;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

    crate::crypto::ensure_crypto_provider();
    let Ok(key) = PrivateKeyDer::from_pem_slice(certificate.key_pem.as_bytes()) else {
        return false;
    };
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(certificate.cert_der.clone())],
            key,
        )
        .is_ok()
}

/// Loads a persisted certificate/key pair, or creates it when neither file
/// exists. The pair is deliberately caller-owned: applications can choose the
/// appropriate config directory without making the library guess where user
/// data belongs.
///
/// # Recovering from a damaged identity
///
/// A stored pair whose key is not the certificate's key can never complete a
/// handshake, so it is replaced rather than returned. Preserving it would brick
/// every subsequent start — the files exist, so nothing regenerates them — and
/// the only cure would be deleting them by hand. Rotating costs this device its
/// fingerprint, which peers must re-accept; that is a far smaller loss than a
/// receiver that can never start again, and an unusable pair has no identity
/// left to preserve anyway.
///
/// A pair that is merely *incomplete* — one file present, the other missing —
/// is an error instead. A missing file can be a half-finished copy or an
/// unmounted directory, where the surviving half is still the real identity and
/// destroying it would be the actual damage.
#[cfg(feature = "https")]
pub fn load_or_generate_tls_certificate(
    cert_path: impl AsRef<Path>,
    key_path: impl AsRef<Path>,
) -> Result<TlsCertificate> {
    let cert_path = cert_path.as_ref();
    let key_path = key_path.as_ref();
    let cert = std::fs::read_to_string(cert_path);
    let key = std::fs::read_to_string(key_path);

    match (cert, key) {
        (Ok(cert_pem), Ok(key_pem)) => match tls_certificate_from_pem(cert_pem, key_pem) {
            Ok(certificate) if is_self_consistent(&certificate) => Ok(certificate),
            loaded => {
                match &loaded {
                    Ok(_) => tracing::warn!(
                        "TLS identity {} does not match its key {}; generating a new one. \
                             This device's fingerprint changes and peers must accept it again.",
                        cert_path.display(),
                        key_path.display()
                    ),
                    Err(error) => tracing::warn!(
                        "TLS identity {} could not be read ({error}); generating a new one. \
                             This device's fingerprint changes and peers must accept it again.",
                        cert_path.display()
                    ),
                }
                write_new_identity(cert_path, key_path)
            }
        },
        (Err(cert_error), Err(key_error))
            if cert_error.kind() == std::io::ErrorKind::NotFound
                && key_error.kind() == std::io::ErrorKind::NotFound =>
        {
            write_new_identity(cert_path, key_path)
        }
        (Err(cert_error), Err(key_error)) => Err(crate::error::LocalSendError::network(format!(
            "Could not read TLS identity files ({}; {}): {cert_error}; {key_error}",
            cert_path.display(),
            key_path.display()
        ))),
        (Err(cert_error), Ok(_)) => Err(crate::error::LocalSendError::network(format!(
            "Could not read TLS certificate {}: {}",
            cert_path.display(),
            cert_error
        ))),
        (Ok(_), Err(key_error)) => Err(crate::error::LocalSendError::network(format!(
            "Could not read TLS private key {}: {}",
            key_path.display(),
            key_error
        ))),
    }
}

/// Generates an identity and publishes it to `cert_path`/`key_path`.
///
/// The two files are one value, and there is no rename that carries both at
/// once. Both temporary files are therefore written and flushed *before* either
/// is published, so the gap in which another process could observe a half-new
/// pair is two adjacent renames rather than a key generation.
///
/// That gap is still not zero, so the pair is read back afterwards. A process
/// that lost a first-start race sees the winner's pair, keeps that one, and
/// both processes converge on a single identity instead of each announcing a
/// fingerprint the files no longer hold.
#[cfg(feature = "https")]
fn write_new_identity(cert_path: &Path, key_path: &Path) -> Result<TlsCertificate> {
    let certificate = generate_tls_certificate()?;
    for parent in [cert_path.parent(), key_path.parent()]
        .into_iter()
        .flatten()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            crate::error::LocalSendError::network(format!(
                "Failed to create TLS identity directory {}: {}",
                parent.display(),
                error
            ))
        })?;
    }

    let staged_cert = stage_write(cert_path, &certificate.cert_pem, false)?;
    let staged_key = match stage_write(key_path, &certificate.key_pem, true) {
        Ok(staged) => staged,
        Err(error) => {
            let _ = std::fs::remove_file(&staged_cert);
            return Err(error);
        }
    };
    publish(&staged_cert, cert_path)?;
    if let Err(error) = publish(&staged_key, key_path) {
        // The certificate is already published; leaving it beside no key would
        // be the incomplete pair this function refuses to create.
        let _ = std::fs::remove_file(cert_path);
        return Err(error);
    }

    match (
        std::fs::read_to_string(cert_path),
        std::fs::read_to_string(key_path),
    ) {
        (Ok(cert_pem), Ok(key_pem)) => match tls_certificate_from_pem(cert_pem, key_pem) {
            Ok(published) if is_self_consistent(&published) => Ok(published),
            // Two processes interleaved their renames. Neither pair on disk is
            // usable, and re-generating here would just re-enter the race, so
            // report it rather than loop.
            _ => Err(crate::error::LocalSendError::network(format!(
                "TLS identity {} and {} were written concurrently and do not match; \
                 retry, or remove both files",
                cert_path.display(),
                key_path.display()
            ))),
        },
        _ => Ok(certificate),
    }
}

/// Writes `contents` to a uniquely named temporary file beside `path` and
/// returns that path, ready to be renamed over `path` by [`publish`].
#[cfg(feature = "https")]
fn stage_write(path: &Path, contents: &str, private: bool) -> Result<PathBuf> {
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            crate::error::LocalSendError::network(format!(
                "Invalid TLS identity path: {}",
                path.display()
            ))
        })?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let temporary_name = format!(".{file_name}.tmp-{}-{nonce}", std::process::id());
    let temporary_path = parent
        .map(|parent| parent.join(&temporary_name))
        .unwrap_or_else(|| PathBuf::from(&temporary_name));

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options.open(&temporary_path).map_err(|error| {
        crate::error::LocalSendError::network(format!(
            "Failed to create temporary TLS identity file {}: {}",
            temporary_path.display(),
            error
        ))
    })?;
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| {
                let _ = std::fs::remove_file(&temporary_path);
                crate::error::LocalSendError::network(format!(
                    "Failed to protect TLS private key {}: {}",
                    temporary_path.display(),
                    error
                ))
            })?;
    }
    if let Err(error) = file
        .write_all(contents.as_bytes())
        .and_then(|_| file.sync_all())
    {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(crate::error::LocalSendError::network(format!(
            "Failed to write TLS identity file {}: {}",
            path.display(),
            error
        )));
    }
    drop(file);
    Ok(temporary_path)
}

/// Renames a file staged by [`stage_write`] into its final place.
#[cfg(feature = "https")]
fn publish(staged: &Path, path: &Path) -> Result<()> {
    std::fs::rename(staged, path).map_err(|error| {
        let _ = std::fs::remove_file(staged);
        crate::error::LocalSendError::network(format!(
            "Failed to publish TLS identity file {}: {}",
            path.display(),
            error
        ))
    })
}

/// Returns the platform config directory used by the command-line and TUI
/// frontends for their stable HTTPS identity.
#[cfg(feature = "https")]
pub fn default_tls_identity_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library").join("Application Support"));
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("LOCALAPPDATA"))
        .map(PathBuf::from);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));

    base.map(|base| base.join("LocalSend-Rust")).ok_or_else(|| {
        crate::error::LocalSendError::network(
            "Cannot determine a config directory for the TLS identity",
        )
    })
}

/// Loads the stable HTTPS identity used by the bundled CLI/TUI frontends.
#[cfg(feature = "https")]
pub fn load_or_generate_default_tls_certificate() -> Result<TlsCertificate> {
    let directory = default_tls_identity_dir()?;
    load_or_generate_tls_certificate(
        directory.join("certificate.pem"),
        directory.join("private-key.pem"),
    )
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

    #[test]
    fn persisted_identity_round_trips_without_changing_device_fingerprint() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let certificate_path = directory.path().join("certificate.pem");
        let key_path = directory.path().join("private-key.pem");

        let first = load_or_generate_tls_certificate(&certificate_path, &key_path)
            .expect("create identity");
        let second = load_or_generate_tls_certificate(&certificate_path, &key_path)
            .expect("reload identity");

        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.cert_der, second.cert_der);
        assert_eq!(first.cert_pem, second.cert_pem);
        assert_eq!(first.key_pem, second.key_pem);
    }

    #[test]
    fn a_partial_persisted_identity_is_rejected_instead_of_rotated() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let certificate_path = directory.path().join("certificate.pem");
        let key_path = directory.path().join("private-key.pem");
        std::fs::write(&certificate_path, "partial").expect("write partial certificate");

        let error = load_or_generate_tls_certificate(&certificate_path, &key_path)
            .err()
            .expect("partial identity must not be silently replaced");
        assert!(error.to_string().contains("private key"));
    }

    #[cfg(unix)]
    #[test]
    fn persisted_private_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary directory");
        let key_path = directory.path().join("private-key.pem");
        load_or_generate_tls_certificate(directory.path().join("certificate.pem"), &key_path)
            .expect("create identity");

        assert_eq!(
            std::fs::metadata(key_path)
                .expect("private key metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    /// A pair whose halves belong to different certificates can never complete
    /// a handshake. Because both files exist, nothing would ever regenerate
    /// them, so returning it would fail every start from then on.
    #[test]
    fn a_stored_pair_whose_key_is_not_its_key_is_replaced_with_a_usable_one() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let certificate_path = directory.path().join("certificate.pem");
        let key_path = directory.path().join("private-key.pem");
        let original = load_or_generate_tls_certificate(&certificate_path, &key_path)
            .expect("create identity");
        let unrelated = generate_tls_certificate().expect("an unrelated identity");
        std::fs::write(&key_path, &unrelated.key_pem).expect("store a mismatched key");

        let recovered = load_or_generate_tls_certificate(&certificate_path, &key_path)
            .expect("a mismatched pair must not brick every later start");

        assert!(
            is_self_consistent(&recovered),
            "the recovered identity must actually be usable for a handshake"
        );
        assert_ne!(
            recovered.fingerprint, original.fingerprint,
            "recovery rotates the identity; it cannot revive the lost key"
        );
        // And the repair is durable: the next start reuses it rather than
        // rotating again.
        let reloaded = load_or_generate_tls_certificate(&certificate_path, &key_path)
            .expect("reload the repaired identity");
        assert_eq!(reloaded.fingerprint, recovered.fingerprint);
    }

    #[test]
    fn a_stored_key_that_is_not_a_key_is_replaced_with_a_usable_one() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let certificate_path = directory.path().join("certificate.pem");
        let key_path = directory.path().join("private-key.pem");
        load_or_generate_tls_certificate(&certificate_path, &key_path).expect("create identity");
        std::fs::write(&key_path, "-----BEGIN PRIVATE KEY-----\nnope\n").expect("corrupt the key");

        let recovered =
            load_or_generate_tls_certificate(&certificate_path, &key_path).expect("recover");

        assert!(is_self_consistent(&recovered));
    }

    #[test]
    fn a_stored_certificate_that_is_not_a_certificate_is_replaced_with_a_usable_one() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let certificate_path = directory.path().join("certificate.pem");
        let key_path = directory.path().join("private-key.pem");
        load_or_generate_tls_certificate(&certificate_path, &key_path).expect("create identity");
        std::fs::write(&certificate_path, "not a certificate").expect("corrupt the certificate");

        let recovered = load_or_generate_tls_certificate(&certificate_path, &key_path)
            .expect("recover from an unreadable certificate");

        assert!(is_self_consistent(&recovered));
    }

    #[test]
    fn a_freshly_generated_identity_is_self_consistent() {
        let generated = generate_tls_certificate().expect("generating");
        assert!(is_self_consistent(&generated));

        let mut swapped = generated.clone();
        swapped.key_pem = generate_tls_certificate().expect("another").key_pem;
        assert!(
            !is_self_consistent(&swapped),
            "the check must actually reject a foreign key"
        );
    }

    /// Concurrent first starts must not leave a pair that no later start can
    /// use. Whichever process wins the two renames, every process must end up
    /// holding an identity that matches what is on disk.
    #[test]
    fn concurrent_first_starts_converge_on_one_usable_identity() {
        for _ in 0..24 {
            let directory = tempfile::tempdir().expect("temporary directory");
            let certificate_path = directory.path().join("certificate.pem");
            let key_path = directory.path().join("private-key.pem");
            let threads: Vec<_> = (0..4)
                .map(|_| {
                    let certificate_path = certificate_path.clone();
                    let key_path = key_path.clone();
                    std::thread::spawn(move || {
                        load_or_generate_tls_certificate(&certificate_path, &key_path)
                    })
                })
                .collect();

            for thread in threads {
                if let Ok(certificate) = thread.join().expect("thread panicked") {
                    assert!(
                        is_self_consistent(&certificate),
                        "a concurrent start returned an identity that cannot handshake"
                    );
                }
            }

            let reloaded = load_or_generate_tls_certificate(&certificate_path, &key_path)
                .expect("the surviving pair must be loadable");
            assert!(is_self_consistent(&reloaded));
        }
    }

    #[test]
    fn the_default_identity_directory_is_under_the_platform_config_home() {
        let directory = default_tls_identity_dir().expect("a config directory");
        assert!(
            directory.is_absolute(),
            "a relative identity directory would depend on the working directory"
        );
        assert_eq!(
            directory.file_name().and_then(|name| name.to_str()),
            Some("LocalSend-Rust")
        );

        #[cfg(target_os = "macos")]
        assert!(directory.starts_with(std::env::var_os("HOME").map(PathBuf::from).unwrap()));
        #[cfg(all(unix, not(target_os = "macos")))]
        assert!(
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .map(|base| directory.starts_with(base))
                .unwrap_or(true)
        );
    }
}
