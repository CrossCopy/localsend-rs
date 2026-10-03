//! Transport-authenticated LocalSend peer identity for HTTPS requests.
//!
//! LocalSend devices use a self-signed certificate, so there is no shared CA
//! to validate. The TLS `CertificateVerify` message still proves possession of
//! the private key for the presented leaf. We request that certificate, let
//! rustls verify the handshake signature, and attach its SHA-256 fingerprint to
//! every request on the connection. Presenting a certificate stays optional so
//! the Web Share routes on this server remain usable from ordinary browsers.

#[cfg(feature = "https")]
use axum::http::Request;
#[cfg(feature = "https")]
use axum_server::{
    accept::Accept,
    tls_rustls::{RustlsAcceptor, RustlsConfig},
};
#[cfg(feature = "https")]
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, Error, RootCertStore, ServerConfig,
    SignatureScheme,
    pki_types::{CertificateDer, PrivateKeyDer, UnixTime, pem::PemObject},
    server::{
        WebPkiClientVerifier,
        danger::{ClientCertVerified, ClientCertVerifier},
    },
};
#[cfg(feature = "https")]
use std::{
    fmt,
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
#[cfg(feature = "https")]
use tokio::io::{AsyncRead, AsyncWrite};
#[cfg(feature = "https")]
use tokio_rustls::server::TlsStream;
#[cfg(feature = "https")]
use tower::Service;
#[cfg(feature = "https")]
use x509_parser::{asn1_rs::FromDer, certificate::X509Certificate, time::ASN1Time};

#[derive(Clone, Debug)]
pub(crate) struct AuthenticatedPeer {
    fingerprint: String,
}

impl AuthenticatedPeer {
    pub(crate) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

#[derive(Clone)]
#[cfg(feature = "https")]
pub(crate) struct IdentityAcceptor {
    inner: RustlsAcceptor,
}

#[cfg(feature = "https")]
impl IdentityAcceptor {
    pub(crate) fn new(certificate: &crate::crypto::TlsCertificate) -> crate::Result<Self> {
        let certificate_der = CertificateDer::from(certificate.cert_der.clone());
        let mut roots = RootCertStore::empty();
        roots.add(certificate_der.clone()).map_err(|error| {
            crate::error::LocalSendError::network(format!(
                "Invalid LocalSend TLS certificate: {error}"
            ))
        })?;
        let signatures = WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|error| {
                crate::error::LocalSendError::network(format!(
                    "Could not build LocalSend client certificate verifier: {error}"
                ))
            })?;
        let verifier = AnyClientCertificateVerifier { signatures };
        let key =
            PrivateKeyDer::from_pem_slice(certificate.key_pem.as_bytes()).map_err(|error| {
                crate::error::LocalSendError::network(format!(
                    "Invalid LocalSend TLS private key: {error}"
                ))
            })?;
        let mut config = ServerConfig::builder()
            .with_client_cert_verifier(Arc::new(verifier))
            .with_single_cert(vec![certificate_der], key)
            .map_err(|error| {
                crate::error::LocalSendError::network(format!(
                    "Invalid LocalSend TLS identity: {error}"
                ))
            })?;
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Self {
            inner: RustlsAcceptor::new(RustlsConfig::from_config(Arc::new(config))),
        })
    }
}

#[cfg(feature = "https")]
impl<I, S> Accept<I, S> for IdentityAcceptor
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    S: Send + 'static,
{
    type Stream = TlsStream<I>;
    type Service = PeerIdentityService<S>;
    type Future = Pin<Box<dyn Future<Output = io::Result<(Self::Stream, Self::Service)>> + Send>>;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let handshake = self.inner.accept(stream, service);
        Box::pin(async move {
            let (stream, service) = handshake.await?;
            let identity = stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|certificates| certificates.first())
                .map(|certificate| AuthenticatedPeer {
                    fingerprint: crate::crypto::sha256_from_bytes(certificate.as_ref()),
                });
            Ok((
                stream,
                PeerIdentityService {
                    inner: service,
                    identity,
                },
            ))
        })
    }
}

#[derive(Clone)]
#[cfg(feature = "https")]
pub(crate) struct PeerIdentityService<S> {
    inner: S,
    identity: Option<AuthenticatedPeer>,
}

#[cfg(feature = "https")]
impl<S, B> Service<Request<B>> for PeerIdentityService<S>
where
    S: Service<Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, mut request: Request<B>) -> Self::Future {
        if let Some(identity) = self.identity.clone() {
            request.extensions_mut().insert(identity);
        }
        self.inner.call(request)
    }
}

#[cfg(feature = "https")]
struct AnyClientCertificateVerifier {
    signatures: Arc<dyn ClientCertVerifier>,
}

#[cfg(feature = "https")]
impl fmt::Debug for AnyClientCertificateVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnyClientCertificateVerifier")
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "https")]
impl ClientCertVerifier for AnyClientCertificateVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        // Web Share is mounted on this listener and can be enabled without a
        // server restart, so browsers and older clients must be able to connect
        // without a certificate. Such connections deliberately receive no
        // `AuthenticatedPeer`; any certificate that is presented is fully
        // validated below and by the subsequent TLS CertificateVerify step.
        false
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.signatures.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        let (remaining, certificate) = X509Certificate::from_der(end_entity.as_ref())
            .map_err(|_| Error::InvalidCertificate(CertificateError::BadEncoding))?;
        if !remaining.is_empty() {
            return Err(Error::InvalidCertificate(CertificateError::BadEncoding));
        }

        let timestamp = i64::try_from(now.as_secs())
            .ok()
            .and_then(|seconds| ASN1Time::from_timestamp(seconds).ok())
            .ok_or(Error::InvalidCertificate(CertificateError::BadEncoding))?;
        if timestamp < certificate.validity().not_before {
            return Err(Error::InvalidCertificate(CertificateError::NotValidYet));
        }
        if timestamp > certificate.validity().not_after {
            return Err(Error::InvalidCertificate(CertificateError::Expired));
        }
        certificate
            .verify_signature(None)
            .map_err(|_| Error::InvalidCertificate(CertificateError::BadSignature))?;

        // LocalSend peers are named by a self-signed leaf rather than a shared
        // CA. The checks above validate that leaf; rustls next verifies the TLS
        // CertificateVerify signature with it, proving possession of its key.
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, Error> {
        self.signatures
            .verify_tls12_signature(message, certificate, signature)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, Error> {
        self.signatures
            .verify_tls13_signature(message, certificate, signature)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.signatures.supported_verify_schemes()
    }
}
