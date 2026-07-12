use localsend_rs::{DeviceInfo, LocalSendServer, Protocol, generate_tls_certificate};
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

    server.stop();
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

    server.stop();
}
