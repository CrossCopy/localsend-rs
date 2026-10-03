#![cfg(feature = "https")]

use localsend_rs::{
    DeviceInfo, FileId, FileMetadata, LocalSendClient, LocalSendServer, PrepareUploadRequest,
    Protocol, SessionId, TlsCertificate, TlsTrustPolicy, Token, generate_tls_certificate,
    server::ServerEvent, tls_certificate_from_pem,
};
use std::collections::HashMap;
use std::time::Duration;

fn expired_client_certificate() -> localsend_rs::TlsCertificate {
    let key = rcgen::KeyPair::generate().expect("expired certificate key");
    let mut params = rcgen::CertificateParams::new(vec!["localhost".into()])
        .expect("expired certificate parameters");
    params.not_before = rcgen::date_time_ymd(2000, 1, 1);
    params.not_after = rcgen::date_time_ymd(2001, 1, 1);
    let certificate = params
        .self_signed(&key)
        .expect("expired self-signed certificate");
    tls_certificate_from_pem(certificate.pem(), key.serialize_pem())
        .expect("expired LocalSend certificate")
}

fn offered_files(name: &str) -> HashMap<FileId, FileMetadata> {
    let id = FileId::new();
    HashMap::from([(
        id.clone(),
        FileMetadata {
            id,
            file_name: name.into(),
            size: 1,
            file_type: "text/plain".into(),
            sha256: None,
            preview: None,
            metadata: None,
        },
    )])
}

fn raw_https_client(certificate: Option<&TlsCertificate>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .danger_accept_invalid_certs(true);
    if let Some(certificate) = certificate {
        let pem = format!("{}\n{}", certificate.cert_pem, certificate.key_pem);
        builder = builder.identity(
            reqwest::Identity::from_pem(pem.as_bytes()).expect("raw test client identity"),
        );
    }
    builder.build().expect("raw HTTPS test client")
}

#[tokio::test]
async fn builder_uses_the_supplied_certificate_fingerprint_for_https() {
    let output = tempfile::tempdir().expect("output directory");
    let certificate = generate_tls_certificate().expect("certificate");
    let expected_fingerprint = certificate.fingerprint.clone();
    let (mut server, _events) = LocalSendServer::builder()
        .alias("Pinned receiver")
        .port(0)
        .save_dir(output.path())
        .protocol(Protocol::Https)
        .tls_certificate(certificate)
        .build()
        .await
        .expect("start HTTPS receiver");

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("test client");
    let info_url = format!("https://127.0.0.1:{}/api/localsend/v2/info", server.port());
    let mut response = None;
    for _ in 0..50 {
        if let Ok(candidate) = client.get(&info_url).send().await {
            response = Some(candidate);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let info: DeviceInfo = response
        .expect("HTTPS /info should become available")
        .json()
        .await
        .expect("LocalSend device info");
    assert_eq!(server.device().fingerprint, expected_fingerprint);
    assert_eq!(info.fingerprint, expected_fingerprint);

    server.stop().await;
}

#[tokio::test]
async fn builder_generates_a_nonempty_https_fingerprint_when_none_is_supplied() {
    let output = tempfile::tempdir().expect("output directory");
    let (mut server, _events) = LocalSendServer::builder()
        .alias("Generated certificate receiver")
        .port(0)
        .save_dir(output.path())
        .protocol(Protocol::Https)
        .build()
        .await
        .expect("start HTTPS receiver");

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("test client");
    let info_url = format!("https://127.0.0.1:{}/api/localsend/v2/info", server.port());
    let mut response = None;
    for _ in 0..50 {
        if let Ok(candidate) = client.get(&info_url).send().await {
            response = Some(candidate);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let info: DeviceInfo = response
        .expect("HTTPS /info should become available")
        .json()
        .await
        .expect("LocalSend device info");
    assert!(!server.device().fingerprint.is_empty());
    assert_eq!(info.fingerprint, server.device().fingerprint);

    server.stop().await;
}

#[tokio::test]
async fn pinned_client_accepts_the_matching_self_signed_leaf() {
    let output = tempfile::tempdir().expect("output directory");
    let (mut server, _events) = LocalSendServer::builder()
        .alias("Pinned receiver")
        .port(0)
        .save_dir(output.path())
        .protocol(Protocol::Https)
        .build()
        .await
        .expect("start HTTPS receiver");

    let mut target = server.device().clone();
    target.ip = Some("127.0.0.1".into());
    let sender = DeviceInfo::new("Pinned sender".into(), 0, Protocol::Https);
    let client = LocalSendClient::with_trust_policy(
        sender,
        TlsTrustPolicy::new([target.fingerprint.clone()]),
    )
    .expect("pinned client");

    client
        .register(&target)
        .await
        .expect("matching fingerprint should be accepted");

    server.stop().await;
}

#[tokio::test]
async fn pinned_client_rejects_a_non_matching_self_signed_leaf() {
    let output = tempfile::tempdir().expect("output directory");
    let (mut server, _events) = LocalSendServer::builder()
        .alias("Pinned receiver")
        .port(0)
        .save_dir(output.path())
        .protocol(Protocol::Https)
        .build()
        .await
        .expect("start HTTPS receiver");

    let mut target = server.device().clone();
    target.ip = Some("127.0.0.1".into());
    let sender = DeviceInfo::new("Pinned sender".into(), 0, Protocol::Https);
    let client = LocalSendClient::with_trust_policy(sender, TlsTrustPolicy::new(["f".repeat(64)]))
        .expect("pinned client");

    assert!(client.register(&target).await.is_err());
    server.stop().await;
}

#[tokio::test]
async fn receiver_exposes_the_authenticated_client_certificate_fingerprint() {
    let output = tempfile::tempdir().expect("output directory");
    let client_certificate = generate_tls_certificate().expect("client certificate");
    let expected_fingerprint = client_certificate.fingerprint.clone();
    let (mut server, mut events) = LocalSendServer::builder()
        .alias("Authenticated receiver")
        .port(0)
        .save_dir(output.path())
        .protocol(Protocol::Https)
        .build()
        .await
        .expect("start HTTPS receiver");

    let mut target = server.device().clone();
    target.ip = Some("127.0.0.1".into());
    let sender = DeviceInfo::new("Authenticated sender".into(), 0, Protocol::Https);
    let client = LocalSendClient::with_trust_policy_and_client_certificate(
        sender,
        TlsTrustPolicy::new([target.fingerprint.clone()]),
        &client_certificate,
    )
    .expect("mTLS client");
    let file_id = FileId::new();
    let files = HashMap::from([(
        file_id.clone(),
        FileMetadata {
            id: file_id,
            file_name: "identity.txt".into(),
            size: 1,
            file_type: "text/plain".into(),
            sha256: None,
            preview: None,
            metadata: None,
        },
    )]);

    let request = tokio::spawn(async move { client.prepare_upload(&target, files, None).await });
    let received_fingerprint = match tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("receive event before timeout")
        .expect("server event stream")
    {
        ServerEvent::TransferRequest(request) => {
            let fingerprint = request.authenticated_fingerprint().map(str::to_owned);
            request.decline();
            fingerprint
        }
        event => panic!("expected transfer request, got {event:?}"),
    };
    assert_eq!(
        received_fingerprint.as_deref(),
        Some(expected_fingerprint.as_str())
    );
    assert!(request.await.expect("request task").is_err());

    server.stop().await;
}

#[tokio::test]
async fn receiver_rejects_an_expired_client_certificate_during_the_tls_handshake() {
    assert_receiver_rejects_certificate(expired_client_certificate()).await;
}

#[tokio::test]
async fn receiver_rejects_a_client_certificate_with_an_invalid_self_signature() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let mut certificate = generate_tls_certificate().expect("client certificate");
    *certificate
        .cert_der
        .last_mut()
        .expect("certificate signature") ^= 1;
    certificate.cert_pem = format!(
        "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
        STANDARD.encode(&certificate.cert_der)
    );
    // Keep the claim consistent with the tampered leaf: only the certificate
    // validation, not a later metadata mismatch, should reject this client.
    certificate.fingerprint = localsend_rs::sha256_from_bytes(&certificate.cert_der);
    assert_receiver_rejects_certificate(certificate).await;
}

async fn assert_receiver_rejects_certificate(client_certificate: TlsCertificate) {
    let output = tempfile::tempdir().expect("output directory");
    let (mut server, _events) = LocalSendServer::builder()
        .alias("Certificate-validating receiver")
        .port(0)
        .save_dir(output.path())
        .protocol(Protocol::Https)
        .build()
        .await
        .expect("start HTTPS receiver");

    let mut target = server.device().clone();
    target.ip = Some("127.0.0.1".into());
    let sender = DeviceInfo::new("Invalid certificate sender".into(), 0, Protocol::Https);
    let client = LocalSendClient::with_trust_policy_and_client_certificate(
        sender,
        TlsTrustPolicy::new([target.fingerprint.clone()]),
        &client_certificate,
    )
    .expect("mTLS client with invalid certificate");

    assert!(
        client.register(&target).await.is_err(),
        "an invalid peer certificate must fail before any HTTP request is served"
    );

    server.stop().await;
}

#[tokio::test]
async fn receiver_rejects_claimed_fingerprints_that_do_not_match_the_client_certificate() {
    let output = tempfile::tempdir().expect("output directory");
    let client_certificate = generate_tls_certificate().expect("client certificate");
    let (mut server, mut events) = LocalSendServer::builder()
        .alias("Fingerprint-checking receiver")
        .port(0)
        .save_dir(output.path())
        .protocol(Protocol::Https)
        .build()
        .await
        .expect("start HTTPS receiver");
    let base = format!("https://127.0.0.1:{}/api/localsend/v2", server.port());
    let client = raw_https_client(Some(&client_certificate));
    let mut impostor = DeviceInfo::new("Impostor".into(), 0, Protocol::Https);
    impostor.fingerprint = "f".repeat(64);

    let register = client
        .post(format!("{base}/register"))
        .json(&impostor)
        .send()
        .await
        .expect("register response");
    assert_eq!(register.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), events.recv())
            .await
            .is_err(),
        "a mismatched registration must not emit PeerRegistered"
    );

    let prepare = client
        .post(format!("{base}/prepare-upload"))
        .json(&PrepareUploadRequest {
            info: impostor,
            files: offered_files("mismatch.txt"),
        })
        .send()
        .await
        .expect("prepare-upload response");
    assert_eq!(prepare.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), events.recv())
            .await
            .is_err(),
        "a mismatched upload offer must not reach the consent event stream"
    );

    server.stop().await;
}

#[tokio::test]
async fn https_without_a_client_certificate_stays_compatible_but_is_not_authenticated() {
    let output = tempfile::tempdir().expect("output directory");
    let (mut server, mut events) = LocalSendServer::builder()
        .alias("Browser-compatible receiver")
        .port(0)
        .save_dir(output.path())
        .protocol(Protocol::Https)
        .build()
        .await
        .expect("start HTTPS receiver");
    let url = format!(
        "https://127.0.0.1:{}/api/localsend/v2/prepare-upload",
        server.port()
    );
    let client = raw_https_client(None);
    let sender = DeviceInfo::new("Certificate-less sender".into(), 0, Protocol::Https);
    let request = tokio::spawn(async move {
        client
            .post(url)
            .json(&PrepareUploadRequest {
                info: sender,
                files: offered_files("no-certificate.txt"),
            })
            .send()
            .await
    });

    match tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("receive event before timeout")
        .expect("server event stream")
    {
        ServerEvent::TransferRequest(request) => {
            assert_eq!(request.authenticated_fingerprint(), None);
            request.decline();
        }
        event => panic!("expected transfer request, got {event:?}"),
    }
    assert_eq!(
        request
            .await
            .expect("request task")
            .expect("prepare-upload response")
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );

    server.stop().await;
}

#[tokio::test]
async fn plain_http_requests_never_gain_an_authenticated_certificate_identity() {
    let output = tempfile::tempdir().expect("output directory");
    let (mut server, mut events) = LocalSendServer::builder()
        .alias("Plain HTTP receiver")
        .port(0)
        .save_dir(output.path())
        .protocol(Protocol::Http)
        .build()
        .await
        .expect("start HTTP receiver");
    let url = format!(
        "http://127.0.0.1:{}/api/localsend/v2/prepare-upload",
        server.port()
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("HTTP test client");
    let sender = DeviceInfo::new("HTTP sender".into(), 0, Protocol::Http);
    let request = tokio::spawn(async move {
        client
            .post(url)
            .json(&PrepareUploadRequest {
                info: sender,
                files: offered_files("plain-http.txt"),
            })
            .send()
            .await
    });

    match tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("receive event before timeout")
        .expect("server event stream")
    {
        ServerEvent::TransferRequest(request) => {
            assert_eq!(request.authenticated_fingerprint(), None);
            request.decline();
        }
        event => panic!("expected transfer request, got {event:?}"),
    }
    assert_eq!(
        request
            .await
            .expect("request task")
            .expect("prepare-upload response")
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );

    server.stop().await;
}

/// A peer whose leaf does not match the advertised fingerprint must be
/// rejected before mutual TLS reveals this device's stable certificate.
#[tokio::test]
async fn wrong_fingerprint_peer_never_receives_the_client_certificate() {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    use rustls::server::WebPkiClientVerifier;
    use rustls::{RootCertStore, ServerConfig};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    let server_certificate = generate_tls_certificate().expect("generate server certificate");
    let client_certificate = generate_tls_certificate().expect("generate client certificate");
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(client_certificate.cert_der.clone()))
        .expect("trust the client certificate");
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .expect("build client verifier");
    let config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![CertificateDer::from(server_certificate.cert_der.clone())],
            PrivateKeyDer::from_pem_slice(server_certificate.key_pem.as_bytes())
                .expect("read server key"),
        )
        .expect("build malicious peer config");
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind malicious peer");
    let port = listener.local_addr().expect("peer address").port();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let server_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept client");
        let Ok(Ok(mut stream)) =
            tokio::time::timeout(Duration::from_secs(2), acceptor.accept(stream)).await
        else {
            return false;
        };
        let received_client_certificate = stream
            .get_ref()
            .1
            .peer_certificates()
            .is_some_and(|certificates| !certificates.is_empty());

        // The old post-handshake check reaches /info, so let it finish and
        // report the mismatch instead of hanging the test.
        let mut request = [0u8; 4096];
        let _ = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut request)).await;
        let body = serde_json::to_vec(&DeviceInfo::new("wrong peer".into(), port, Protocol::Https))
            .expect("encode peer");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.write_all(&body).await;
        received_client_certificate
    });

    let mut target = DeviceInfo::new("wrong peer".into(), port, Protocol::Https);
    target.ip = Some("127.0.0.1".into());
    target.fingerprint = "f".repeat(64);
    let sender = DeviceInfo::new("private sender".into(), 0, Protocol::Https);
    let client = LocalSendClient::with_trust_policy_and_client_certificate(
        sender,
        TlsTrustPolicy::new([target.fingerprint.clone()]),
        &client_certificate,
    )
    .expect("pinned client");

    assert!(client.register(&target).await.is_err());
    assert!(
        !server_task.await.expect("malicious peer task"),
        "a wrong-fingerprint peer received the stable client certificate"
    );
}

/// The full HTTPS send path must present the client certificate on every
/// connection. The server below requires mTLS and exercises `/register`,
/// `/prepare-upload`, and `/upload` in sequence while the leaf fingerprint is
/// verified inside each TLS handshake.
#[tokio::test]
async fn m_tls_client_survives_handshake_pinning_and_full_upload() {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    use rustls::server::WebPkiClientVerifier;
    use rustls::{RootCertStore, ServerConfig};
    use std::sync::Arc;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    fn private_key(certificate: &localsend_rs::TlsCertificate) -> PrivateKeyDer<'static> {
        PrivateKeyDer::from_pem_slice(certificate.key_pem.as_bytes()).expect("read private key")
    }

    async fn read_request<S: AsyncRead + Unpin>(stream: &mut S) -> String {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 2048];
        let mut header_end;
        let mut content_length;
        loop {
            let count = stream.read(&mut chunk).await.expect("read request");
            assert!(count > 0, "client closed before sending a request");
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                header_end = end + 4;
                let headers = String::from_utf8_lossy(&bytes[..end]);
                content_length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if bytes.len() >= header_end + content_length {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    let server_certificate = generate_tls_certificate().expect("generate server certificate");
    let client_certificate = generate_tls_certificate().expect("generate client certificate");
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(client_certificate.cert_der.clone()))
        .expect("trust the client certificate");
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .expect("build client verifier");
    let config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![CertificateDer::from(server_certificate.cert_der.clone())],
            private_key(&server_certificate),
        )
        .expect("build mTLS server config");
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind test server");
    let port = listener.local_addr().expect("server address").port();
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let mut peer = DeviceInfo::new("mTLS upload peer".into(), port, Protocol::Https);
    peer.fingerprint = server_certificate.fingerprint.clone();
    let server_peer = peer.clone();
    let expected_client_fingerprint = client_certificate.fingerprint.clone();
    let server_task = tokio::spawn(async move {
        for expected_path in [
            "POST /api/localsend/v2/register",
            "POST /api/localsend/v2/prepare-upload",
            "POST /api/localsend/v2/upload",
        ] {
            let (stream, _) = listener.accept().await.expect("accept request");
            let mut stream = acceptor.accept(stream).await.expect("client certificate");
            let request = read_request(&mut stream).await;
            assert!(
                request.starts_with(expected_path),
                "expected {expected_path}, got {}",
                request.lines().next().unwrap_or_default()
            );
            if expected_path == "POST /api/localsend/v2/register" {
                let body = request
                    .split_once("\r\n\r\n")
                    .map(|(_, body)| body)
                    .expect("register request body");
                let sender: DeviceInfo = serde_json::from_str(body).expect("decode sender");
                assert_eq!(sender.fingerprint, expected_client_fingerprint);
            }

            let (status, body) = match expected_path {
                "POST /api/localsend/v2/register" => (
                    "200 OK",
                    serde_json::to_vec(&server_peer).expect("encode peer"),
                ),
                "POST /api/localsend/v2/prepare-upload" => (
                    "200 OK",
                    serde_json::json!({
                        "sessionId": "m-tls-session",
                        "files": { "m-tls-file": "m-tls-token" }
                    })
                    .to_string()
                    .into_bytes(),
                ),
                "POST /api/localsend/v2/upload" => ("204 No Content", Vec::new()),
                _ => unreachable!(),
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response");
            stream.write_all(&body).await.expect("write response body");
        }
    });

    let mut target = peer.clone();
    target.ip = Some("127.0.0.1".into());
    let sender = DeviceInfo::new("mTLS sender".into(), 0, Protocol::Https);
    let client = LocalSendClient::with_trust_policy_and_client_certificate(
        sender,
        TlsTrustPolicy::new([target.fingerprint.clone()]),
        &client_certificate,
    )
    .expect("mTLS client");
    client.register(&target).await.expect("register over mTLS");

    let file_id = FileId::from_string("m-tls-file".into());
    let mut files = HashMap::new();
    files.insert(
        file_id.clone(),
        FileMetadata {
            id: file_id.clone(),
            file_name: "payload.txt".into(),
            size: 7,
            file_type: "text/plain".into(),
            sha256: None,
            preview: None,
            metadata: None,
        },
    );
    let prepared = client
        .prepare_upload(&target, files, None)
        .await
        .expect("prepare upload over mTLS");
    assert_eq!(
        prepared.session_id,
        SessionId::from_string("m-tls-session".into())
    );
    let token = prepared
        .files
        .get(&file_id)
        .cloned()
        .unwrap_or_else(|| Token::from_string("m-tls-token".into()));
    client
        .upload_bytes(
            &target,
            &prepared.session_id,
            &file_id,
            &token,
            b"payload".to_vec(),
        )
        .await
        .expect("upload over mTLS");

    server_task.await.expect("mTLS server completed");
}

#[test]
fn pinned_client_rejects_an_empty_or_malformed_discovered_fingerprint() {
    let device = DeviceInfo::new("Pinned sender".into(), 0, Protocol::Https);
    assert!(LocalSendClient::with_trust_policy(device.clone(), TlsTrustPolicy::new([""])).is_err());
    assert!(
        LocalSendClient::with_trust_policy(device, TlsTrustPolicy::new(["not-a-sha256"])).is_err()
    );
}

#[tokio::test]
async fn http_requests_bypass_the_tls_verifier() {
    let output = tempfile::tempdir().expect("output directory");
    let (mut server, _events) = LocalSendServer::builder()
        .alias("HTTP receiver")
        .port(0)
        .save_dir(output.path())
        .protocol(Protocol::Http)
        .build()
        .await
        .expect("start HTTP receiver");

    let mut target = server.device().clone();
    target.ip = Some("127.0.0.1".into());
    let sender = DeviceInfo::new("Pinned sender".into(), 0, Protocol::Http);
    let client = LocalSendClient::with_trust_policy(sender, TlsTrustPolicy::new(["f".repeat(64)]))
        .expect("pinned client configuration");

    client
        .register(&target)
        .await
        .expect("HTTP should not invoke TLS verification");
    server.stop().await;
}
