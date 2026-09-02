#![cfg(feature = "https")]

use localsend_rs::{
    DeviceInfo, FileId, FileMetadata, LocalSendClient, LocalSendServer, Protocol, SessionId,
    TlsTrustPolicy, Token, generate_tls_certificate,
};
use std::collections::HashMap;
use std::time::Duration;

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

/// The full HTTPS send path must present the client certificate on every
/// connection, not only while discovering the peer. The server below requires
/// mTLS and exercises the bootstrap `/info`, `/register`, `/prepare-upload`,
/// and `/upload` requests in sequence.
#[tokio::test]
async fn m_tls_client_survives_bootstrap_and_full_upload() {
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
            "GET /api/localsend/v2/info",
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
                "GET /api/localsend/v2/info" | "POST /api/localsend/v2/register" => (
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
