mod common;

use localsend_rs::Protocol;
use localsend_rs::server::{LocalSendServer, ServerEvent};
use reqwest::StatusCode;
use serde_json::json;
use std::path::{Path, PathBuf};

const PAYLOAD: &[u8] = b"safe receive payload";

struct UploadResult {
    status: StatusCode,
    file_received: Vec<(String, PathBuf)>,
    session_done: bool,
}

async fn upload_one(save_dir: &Path, file_name: &str) -> UploadResult {
    let (mut server, mut events) = LocalSendServer::builder()
        .alias("Safety Receiver")
        .port(0)
        .save_dir(save_dir)
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .expect("start real HTTP receiver");
    let port = server.port();
    common::wait_for_http_info(port).await;

    let prepare = json!({
        "info": {
            "alias": "Safety Sender",
            "version": "2.1",
            "deviceType": "headless",
            "fingerprint": "safety-sender",
            "port": 53317,
            "protocol": "http",
            "download": false
        },
        "files": {
            "f1": {
                "id": "f1",
                "fileName": file_name,
                "size": PAYLOAD.len(),
                "fileType": "application/octet-stream"
            }
        }
    });
    let prepared: serde_json::Value = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/api/localsend/v2/prepare-upload"
        ))
        .json(&prepare)
        .send()
        .await
        .expect("prepare request")
        .error_for_status()
        .expect("prepare accepted")
        .json()
        .await
        .expect("prepare response");
    let session_id = prepared["sessionId"]
        .as_str()
        .expect("session id in response");
    let token = prepared["files"]["f1"]
        .as_str()
        .expect("upload token in response");

    let response = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/api/localsend/v2/upload?sessionId={session_id}&fileId=f1&token={token}"
        ))
        .body(PAYLOAD)
        .send()
        .await
        .expect("upload request");

    let mut file_received = Vec::new();
    let mut session_done = false;
    while let Ok(event) = events.try_recv() {
        match event {
            ServerEvent::FileReceived {
                file_name, path, ..
            } => file_received.push((file_name, path)),
            ServerEvent::SessionDone { .. } => session_done = true,
            ServerEvent::FileReceiveProgress { .. } => {}
            other => panic!("unexpected receive-path event: {other:?}"),
        }
    }

    let result = UploadResult {
        status: response.status(),
        file_received,
        session_done,
    };
    server.stop().await;
    result
}

fn assert_one_success(result: UploadResult, expected_name: &str, expected_path: &Path) {
    assert_eq!(result.status, StatusCode::OK);
    assert_eq!(
        result.file_received,
        vec![(expected_name.to_owned(), expected_path.to_owned())]
    );
    assert!(result.session_done, "successful upload must finish session");
}

#[tokio::test]
async fn regular_collision_is_created_at_the_first_atomic_suffix() {
    let save = tempfile::tempdir().expect("save directory");
    let original = save.path().join("a.txt");
    std::fs::write(&original, b"existing").expect("plant collision");

    let result = upload_one(save.path(), "a.txt").await;

    assert_eq!(std::fs::read(&original).unwrap(), b"existing");
    let received = save.path().join("a (1).txt");
    assert_eq!(std::fs::read(&received).unwrap(), PAYLOAD);
    assert_one_success(result, "a (1).txt", &received);
}

#[cfg(unix)]
#[tokio::test]
async fn final_symlink_is_a_collision_and_never_changes_its_target() {
    use std::os::unix::fs::symlink;

    let save = tempfile::tempdir().expect("save directory");
    let outside = tempfile::tempdir().expect("outside directory");
    let target = outside.path().join("sentinel.bin");
    std::fs::write(&target, b"outside sentinel").expect("plant outside sentinel");
    symlink(&target, save.path().join("linked.bin")).expect("plant final symlink");

    let result = upload_one(save.path(), "linked.bin").await;

    assert_eq!(std::fs::read(&target).unwrap(), b"outside sentinel");
    let received = save.path().join("linked (1).bin");
    assert_eq!(std::fs::read(&received).unwrap(), PAYLOAD);
    assert_one_success(result, "linked (1).bin", &received);
}

#[cfg(unix)]
#[tokio::test]
async fn dangling_final_symlink_is_a_collision_and_never_creates_its_target() {
    use std::os::unix::fs::symlink;

    let save = tempfile::tempdir().expect("save directory");
    let outside = tempfile::tempdir().expect("outside directory");
    let target = outside.path().join("must-not-exist.bin");
    symlink(&target, save.path().join("dangling.bin")).expect("plant dangling symlink");

    let result = upload_one(save.path(), "dangling.bin").await;

    assert!(!target.exists(), "upload followed a dangling final symlink");
    let received = save.path().join("dangling (1).bin");
    assert_eq!(std::fs::read(&received).unwrap(), PAYLOAD);
    assert_one_success(result, "dangling (1).bin", &received);
}

#[cfg(unix)]
#[tokio::test]
async fn parent_symlink_is_rejected_without_writing_or_success_events() {
    use std::os::unix::fs::symlink;

    let save = tempfile::tempdir().expect("save directory");
    let outside = tempfile::tempdir().expect("outside directory");
    symlink(outside.path(), save.path().join("linked-parent")).expect("plant parent symlink");

    let result = upload_one(save.path(), "linked-parent/payload.bin").await;

    assert_ne!(result.status, StatusCode::OK);
    assert!(
        !outside.path().join("payload.bin").exists(),
        "upload escaped through a parent symlink"
    );
    assert!(
        result.file_received.is_empty(),
        "failed upload emitted FileReceived"
    );
    assert!(
        !result.session_done,
        "failed upload emitted SessionDone success"
    );
}
