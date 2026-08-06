mod common;

use localsend_rs::Protocol;
use localsend_rs::server::{LocalSendServer, ServerEvent};
use reqwest::StatusCode;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::oneshot;

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

async fn prepare_one(port: u16, file_name: &str, size: usize) -> (String, String) {
    let request = json!({
        "info": {
            "alias": "Paused Sender",
            "version": "2.1",
            "deviceType": "headless",
            "fingerprint": "paused-sender",
            "port": 53317,
            "protocol": "http",
            "download": false
        },
        "files": {
            "f1": {
                "id": "f1",
                "fileName": file_name,
                "size": size,
                "fileType": "application/octet-stream"
            }
        }
    });
    let prepared: serde_json::Value = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/api/localsend/v2/prepare-upload"
        ))
        .json(&request)
        .send()
        .await
        .expect("prepare paused upload")
        .error_for_status()
        .expect("paused upload accepted")
        .json()
        .await
        .expect("paused prepare response");
    (
        prepared["sessionId"].as_str().unwrap().to_owned(),
        prepared["files"]["f1"].as_str().unwrap().to_owned(),
    )
}

fn paused_body(first: &'static [u8], rest: &'static [u8]) -> (reqwest::Body, oneshot::Sender<()>) {
    let (release_tx, release_rx) = oneshot::channel();
    let stream = async_stream::stream! {
        yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(first));
        let _ = release_rx.await;
        yield Ok(bytes::Bytes::from_static(rest));
    };
    (reqwest::Body::wrap_stream(stream), release_tx)
}

#[derive(Default)]
struct ObservedEvents {
    progress: Vec<u64>,
    file_received: usize,
    session_done: usize,
}

fn observe(event: ServerEvent, observed: &mut ObservedEvents) {
    match event {
        ServerEvent::FileReceiveProgress { bytes_received, .. } => {
            observed.progress.push(bytes_received)
        }
        ServerEvent::FileReceived { .. } => observed.file_received += 1,
        ServerEvent::SessionDone { .. } => observed.session_done += 1,
        other => panic!("unexpected paused-upload event: {other:?}"),
    }
}

async fn wait_for_positive_progress(
    events: &mut tokio::sync::mpsc::Receiver<ServerEvent>,
    observed: &mut ObservedEvents,
) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.expect("event stream remains open");
            observe(event, observed);
            if observed.progress.last().is_some_and(|value| *value > 0) {
                break;
            }
        }
    })
    .await
    .expect("receiver should write the first body chunk");
}

fn drain_events(
    events: &mut tokio::sync::mpsc::Receiver<ServerEvent>,
    observed: &mut ObservedEvents,
) {
    while let Ok(event) = events.try_recv() {
        observe(event, observed);
    }
}

#[tokio::test]
async fn cancel_during_a_paused_upload_aborts_before_publication() {
    const FIRST: &[u8] = b"first body chunk";
    const REST: &[u8] = b"rest of body";

    let save = tempfile::tempdir().expect("save directory");
    let (mut server, mut events) = LocalSendServer::builder()
        .alias("Cancel Receiver")
        .port(0)
        .save_dir(save.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .expect("start cancel receiver");
    let port = server.port();
    common::wait_for_http_info(port).await;
    let (session_id, token) = prepare_one(port, "cancelled.bin", FIRST.len() + REST.len()).await;
    let (body, release) = paused_body(FIRST, REST);
    let upload_session_id = session_id.clone();
    let upload = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!(
                "http://127.0.0.1:{port}/api/localsend/v2/upload?sessionId={upload_session_id}&fileId=f1&token={token}"
            ))
            .body(body)
            .send()
            .await
            .expect("paused upload response")
    });

    let mut observed = ObservedEvents::default();
    wait_for_positive_progress(&mut events, &mut observed).await;
    let cancel = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/api/localsend/v2/cancel?sessionId={session_id}"
        ))
        .send()
        .await
        .expect("cancel request");
    assert_eq!(cancel.status(), StatusCode::OK);
    release.send(()).expect("release paused body");
    let upload = tokio::time::timeout(Duration::from_secs(2), upload)
        .await
        .expect("cancelled upload should finish")
        .expect("upload task should not panic");
    drain_events(&mut events, &mut observed);

    assert!(!upload.status().is_success());
    assert!(!save.path().join("cancelled.bin").exists());
    assert_eq!(observed.file_received, 0);
    assert_eq!(
        observed.session_done, 1,
        "only /cancel may finish the session"
    );
    assert_eq!(observed.progress.last(), Some(&0));
    server.stop().await;
}

#[tokio::test]
async fn timeout_replacement_during_a_paused_upload_aborts_before_publication() {
    const FIRST: &[u8] = b"first replacement chunk";
    const REST: &[u8] = b"rest after replacement";

    let save = tempfile::tempdir().expect("save directory");
    let (mut server, mut events) = LocalSendServer::builder()
        .alias("Replacement Receiver")
        .port(0)
        .save_dir(save.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .session_timeout(Duration::from_millis(50))
        .build()
        .await
        .expect("start replacement receiver");
    let port = server.port();
    common::wait_for_http_info(port).await;
    let (session_id, token) = prepare_one(port, "timed-out.bin", FIRST.len() + REST.len()).await;
    let (body, release) = paused_body(FIRST, REST);
    let upload = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!(
                "http://127.0.0.1:{port}/api/localsend/v2/upload?sessionId={session_id}&fileId=f1&token={token}"
            ))
            .body(body)
            .send()
            .await
            .expect("timed-out upload response")
    });

    let mut observed = ObservedEvents::default();
    wait_for_positive_progress(&mut events, &mut observed).await;
    tokio::time::sleep(Duration::from_millis(75)).await;
    let _replacement = prepare_one(port, "replacement.bin", 1).await;
    release.send(()).expect("release replaced body");
    let upload = tokio::time::timeout(Duration::from_secs(2), upload)
        .await
        .expect("replaced upload should finish")
        .expect("upload task should not panic");
    drain_events(&mut events, &mut observed);

    assert!(!upload.status().is_success());
    assert!(!save.path().join("timed-out.bin").exists());
    assert_eq!(observed.file_received, 0);
    assert_eq!(observed.session_done, 0);
    assert_eq!(observed.progress.last(), Some(&0));
    server.stop().await;
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
