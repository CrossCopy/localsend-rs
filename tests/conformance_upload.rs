mod common;

use localsend_rs::Protocol;
use localsend_rs::server::LocalSendServer;
use localsend_rs::sha256_from_bytes;
use serde_json::json;

/// Drive `prepare-upload` over raw reqwest and return `(session_id, token)`
/// for the single offered file id `f1`.
async fn prepare_single(port: u16, size: u64, sha256: Option<String>) -> (String, String) {
    let mut file: serde_json::Value = json!({
        "id": "f1",
        "fileName": "big.bin",
        "size": size,
        "fileType": "application/octet-stream",
    });
    if let Some(sha) = sha256 {
        file["sha256"] = json!(sha);
    }
    let body = json!({
        "info": { "alias": "raw", "version": "2.1", "deviceType": "headless",
                  "fingerprint": "fp", "port": 53317, "protocol": "http", "download": false },
        "files": { "f1": file }
    });
    let resp: serde_json::Value = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/api/localsend/v2/prepare-upload"
        ))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session_id = resp["sessionId"].as_str().unwrap().to_string();
    let token = resp["files"]["f1"].as_str().unwrap().to_string();
    (session_id, token)
}

/// A body shorter than the declared size (truncated transfer / misbehaving
/// client) must be rejected: 500, the partial file is discarded, and the
/// session is NOT completed (no FileReceived / SessionDone).
#[tokio::test]
async fn short_body_is_rejected_and_partial_discarded() {
    let save = tempfile::tempdir().unwrap();
    let (server, mut events) = LocalSendServer::builder()
        .alias("R")
        .port(0)
        .save_dir(save.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .unwrap();
    let port = server.port();
    common::wait_for_http_info(port).await;

    // Declare 200 bytes but send only 10.
    let (session_id, token) = prepare_single(port, 200, None).await;

    let r = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/api/localsend/v2/upload?sessionId={session_id}&fileId=f1&token={token}"
        ))
        .body(vec![0u8; 10])
        .send()
        .await
        .unwrap();

    // Wire: the receiver reports failure, not success.
    assert_eq!(r.status(), 500);

    // The partial file must not linger on disk.
    assert!(
        !save.path().join("big.bin").exists(),
        "partial upload must be deleted"
    );

    // The session must not have been recorded/completed.
    assert!(
        events.try_recv().is_err(),
        "no FileReceived/SessionDone must be emitted for a rejected upload"
    );
}

/// When the metadata carries a sha256, a full-length body whose contents
/// hash to a different digest must be rejected the same way.
#[tokio::test]
async fn sha256_mismatch_is_rejected_and_partial_discarded() {
    let save = tempfile::tempdir().unwrap();
    let (server, mut events) = LocalSendServer::builder()
        .alias("R")
        .port(0)
        .save_dir(save.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .unwrap();
    let port = server.port();
    common::wait_for_http_info(port).await;

    // Declare the sha256 of all-zero bytes, but send all-one bytes of the
    // same length: size matches, digest does not.
    let declared = sha256_from_bytes(&[0u8; 64]);
    let (session_id, token) = prepare_single(port, 64, Some(declared)).await;

    let r = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/api/localsend/v2/upload?sessionId={session_id}&fileId=f1&token={token}"
        ))
        .body(vec![1u8; 64])
        .send()
        .await
        .unwrap();

    assert_eq!(r.status(), 500);
    assert!(
        !save.path().join("big.bin").exists(),
        "partial upload must be deleted"
    );
    assert!(
        events.try_recv().is_err(),
        "no FileReceived/SessionDone must be emitted for a rejected upload"
    );
}
