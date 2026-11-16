//! The update barrier must close admission at the server's session boundary,
//! not infer idleness from events that may not have reached an embedder yet.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream;
use localsend_rs::server::{
    CrossCopyAuthorizedPrepare, CrossCopyAuthorizedUpload, CrossCopyAuthorizedUploadError,
    CrossCopyAuthorizedUploadGate, CrossCopyAuthorizedUploadOwner,
    CrossCopyAuthorizedUploadReceipt, LocalSendServer, ServerEvent, ServerQuiescenceError,
};
use localsend_rs::{DeviceInfo, DeviceType, LocalSendClient, Protocol, build_file_metadata};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

fn sender() -> DeviceInfo {
    DeviceInfo {
        alias: "quiescence-test".to_string(),
        version: "2.1".to_string(),
        device_model: Some("test".to_string()),
        device_type: Some(DeviceType::Desktop),
        fingerprint: "quiescence-test-fingerprint".to_string(),
        port: 53317,
        protocol: Protocol::Http,
        download: false,
        ip: None,
    }
}

fn target(server: &LocalSendServer) -> DeviceInfo {
    DeviceInfo {
        ip: Some("127.0.0.1".to_string()),
        port: server.port(),
        ..server.device().clone()
    }
}

fn offer(name: &str) -> Value {
    json!({
        "info": {
            "alias": "raw sender",
            "version": "2.1",
            "deviceType": "desktop",
            "fingerprint": "raw-sender-fingerprint",
            "port": 53317,
            "protocol": "http",
            "download": false
        },
        "files": {
            "file-id": {
                "id": "file-id",
                "fileName": name,
                "size": 1,
                "fileType": "application/octet-stream"
            }
        }
    })
}

async fn raw_prepare(port: u16, name: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/api/localsend/v2/prepare-upload"
        ))
        .json(&offer(name))
        .send()
        .await
        .expect("prepare request should reach the server")
}

async fn fill_server_event_queue(port: u16) {
    localsend_rs::crypto::ensure_crypto_provider();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for _ in 0..64 {
        assert_eq!(
            client
                .post(format!("http://127.0.0.1:{port}/api/localsend/v2/register"))
                .json(&sender())
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_cancel_releases_a_prepare_blocked_on_a_full_event_queue() {
    let save_dir = tempfile::tempdir().unwrap();
    let (mut server, _events) = LocalSendServer::builder()
        .port(0)
        .save_dir(save_dir.path())
        .protocol(Protocol::Http)
        .accept_timeout(Duration::from_secs(5))
        .build()
        .await
        .unwrap();
    let port = server.port();
    fill_server_event_queue(port).await;

    let (first_tx, mut first_rx) = oneshot::channel();
    let first = tokio::spawn(async move {
        let _ = first_tx.send(raw_prepare(port, "cancel-me-a.bin").await);
    });
    let (second_tx, mut second_rx) = oneshot::channel();
    let second = tokio::spawn(async move {
        let _ = second_tx.send(raw_prepare(port, "cancel-me-b.bin").await);
    });
    let (first_done, first_response) = tokio::select! {
        response = &mut first_rx => (1, response.unwrap()),
        response = &mut second_rx => (2, response.unwrap()),
    };
    assert_eq!(
        first_response.status(),
        reqwest::StatusCode::CONFLICT,
        "one prepare owns the reservation while the other observes it"
    );

    assert_eq!(
        server.prepare_update_quiescence().await,
        Err(ServerQuiescenceError::Busy),
        "the handler blocked on event delivery still owns its prepare lifecycle"
    );
    assert_eq!(
        server.stop_for_update().await,
        Err(ServerQuiescenceError::Busy),
        "finalization must not wait behind an untracked prepare handler"
    );
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    assert_eq!(
        client
            .post(format!("http://127.0.0.1:{port}/api/localsend/v2/cancel"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    let remaining_response = match first_done {
        1 => second_rx.await.unwrap(),
        2 => first_rx.await.unwrap(),
        _ => unreachable!(),
    };
    assert_eq!(
        remaining_response.status(),
        reqwest::StatusCode::FORBIDDEN,
        "sender cancellation must release the prepare blocked by event queue backpressure"
    );
    first.await.unwrap();
    second.await.unwrap();
    assert_eq!(server.prepare_update_quiescence().await, Ok(()));
    server.stop_for_update().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn event_queue_expiry_releases_a_prepare_handler_and_allows_finalization() {
    let save_dir = tempfile::tempdir().unwrap();
    let (mut server, _events) = LocalSendServer::builder()
        .port(0)
        .save_dir(save_dir.path())
        .protocol(Protocol::Http)
        .accept_timeout(Duration::from_millis(100))
        .build()
        .await
        .unwrap();
    let port = server.port();
    fill_server_event_queue(port).await;

    let response = tokio::time::timeout(Duration::from_secs(2), raw_prepare(port, "expires.bin"))
        .await
        .expect("prepare event delivery should share the consent deadline");
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    assert_eq!(server.prepare_update_quiescence().await, Ok(()));
    server.stop_for_update().await.unwrap();
}

#[tokio::test]
async fn a_pending_offer_is_busy_and_can_finish_while_admission_is_closed() {
    let save_dir = tempfile::tempdir().unwrap();
    let (mut server, mut events) = LocalSendServer::builder()
        .port(0)
        .save_dir(save_dir.path())
        .protocol(Protocol::Http)
        .accept_timeout(Duration::from_secs(10))
        .build()
        .await
        .unwrap();
    let port = server.port();
    let input = tempfile::tempdir().unwrap();
    let path = input.path().join("pending.bin");
    tokio::fs::write(&path, b"pending transfer").await.unwrap();
    let metadata = build_file_metadata(&path).await.unwrap();
    let file_id = metadata.id.clone();
    let client = LocalSendClient::new(sender());
    let receiver = target(&server);

    let prepare = tokio::spawn(async move {
        client
            .prepare_upload(&receiver, HashMap::from([(file_id, metadata)]), None)
            .await
    });
    let request = loop {
        if let Some(ServerEvent::TransferRequest(request)) = events.recv().await {
            break request;
        }
    };

    assert_eq!(
        server.prepare_update_quiescence().await,
        Err(ServerQuiescenceError::Busy),
        "a consent reservation is a pending transfer even before progress exists"
    );
    assert_eq!(
        server.stop_for_update().await,
        Err(ServerQuiescenceError::Busy),
        "a busy drain check must leave the listener running for existing work"
    );
    assert_eq!(
        raw_prepare(port, "new-after-barrier.bin").await.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "busy must leave new prepare admission closed while the existing offer drains"
    );

    request.accept();
    let prepared = prepare.await.unwrap().unwrap();
    let id = prepared.files.keys().next().unwrap();
    let token = &prepared.files[id];
    let upload_client = LocalSendClient::new(sender());
    upload_client
        .upload_file(
            &target(&server),
            &prepared.session_id,
            id,
            token,
            &path,
            None,
        )
        .await
        .unwrap();

    assert_eq!(server.prepare_update_quiescence().await, Ok(()));
    server.cancel_update_quiescence().await.unwrap();
    server.stop().await;
}

struct BlockingCrossCopyGate {
    take_started: tokio::sync::Mutex<Option<oneshot::Sender<()>>>,
    take_release: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
    cancel_started: tokio::sync::Mutex<Option<oneshot::Sender<()>>>,
    cancel_release: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait::async_trait]
impl CrossCopyAuthorizedUploadGate for BlockingCrossCopyGate {
    async fn take_authorized_upload(
        &self,
        _prepare: CrossCopyAuthorizedPrepare,
    ) -> Result<Box<dyn CrossCopyAuthorizedUploadOwner>, CrossCopyAuthorizedUploadError> {
        if let Some(started) = self.take_started.lock().await.take() {
            let _ = started.send(());
        }
        self.take_release
            .lock()
            .await
            .take()
            .expect("one test take")
            .await
            .expect("test releases the take");
        Ok(Box::new(BlockingCrossCopyOwner {
            cancel_started: self.cancel_started.lock().await.take(),
            cancel_release: self.cancel_release.lock().await.take(),
        }))
    }
}

struct BlockingCrossCopyOwner {
    cancel_started: Option<oneshot::Sender<()>>,
    cancel_release: Option<oneshot::Receiver<()>>,
}

#[async_trait::async_trait]
impl CrossCopyAuthorizedUploadOwner for BlockingCrossCopyOwner {
    fn cancellation(&self) -> CancellationToken {
        CancellationToken::new()
    }

    async fn receive(
        self: Box<Self>,
        _upload: CrossCopyAuthorizedUpload,
    ) -> Result<CrossCopyAuthorizedUploadReceipt, CrossCopyAuthorizedUploadError> {
        Err(CrossCopyAuthorizedUploadError::Failed)
    }

    async fn cancel(mut self: Box<Self>) {
        if let Some(started) = self.cancel_started.take() {
            let _ = started.send(());
        }
        if let Some(release) = self.cancel_release.take() {
            let _ = release.await;
        }
    }
}

#[tokio::test]
async fn a_gate_prepare_and_its_terminal_cleanup_remain_visible_to_the_barrier() {
    let save_dir = tempfile::tempdir().unwrap();
    let (take_started_tx, take_started_rx) = oneshot::channel();
    let (take_release_tx, take_release_rx) = oneshot::channel();
    let (cancel_started_tx, cancel_started_rx) = oneshot::channel();
    let (cancel_release_tx, cancel_release_rx) = oneshot::channel();
    let gate = std::sync::Arc::new(BlockingCrossCopyGate {
        take_started: tokio::sync::Mutex::new(Some(take_started_tx)),
        take_release: tokio::sync::Mutex::new(Some(take_release_rx)),
        cancel_started: tokio::sync::Mutex::new(Some(cancel_started_tx)),
        cancel_release: tokio::sync::Mutex::new(Some(cancel_release_rx)),
    });
    let (mut server, _events) = LocalSendServer::builder()
        .port(0)
        .save_dir(save_dir.path())
        .protocol(Protocol::Http)
        .crosscopy_authorized_upload_gate(gate)
        .build()
        .await
        .unwrap();

    localsend_rs::crypto::ensure_crypto_provider();
    let request = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{}/api/localsend/v2/prepare-upload",
            server.port()
        ))
        .header(
            localsend_rs::server::CROSSCOPY_FILE_V3_HANDOFF_HEADER,
            "a".repeat(64),
        )
        .json(&json!({
            "info": {
                "alias": "protected sender",
                "version": "2.1",
                "deviceType": "desktop",
                "fingerprint": "protected-fingerprint",
                "port": 53317,
                "protocol": "http",
                "download": false
            },
            "files": {
                "protected-file": {
                    "id": "protected-file",
                    "fileName": "protected.bin",
                    "size": 1,
                    "fileType": "application/octet-stream"
                }
            }
        }))
        .send();
    let request = tokio::spawn(request);
    take_started_rx.await.unwrap();

    assert_eq!(
        server.prepare_update_quiescence().await,
        Err(ServerQuiescenceError::Busy),
        "a gate call outside ServerState's lock is still a pending prepare"
    );
    take_release_tx.send(()).unwrap();
    cancel_started_rx.await.unwrap();
    assert_eq!(
        server.prepare_update_quiescence().await,
        Err(ServerQuiescenceError::Busy),
        "the barrier waits until a gate owner rejected by quiescence is terminalized"
    );
    cancel_release_tx.send(()).unwrap();
    assert_eq!(
        request.await.unwrap().unwrap().status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(server.prepare_update_quiescence().await, Ok(()));
    server.cancel_update_quiescence().await.unwrap();
    server.stop().await;
}

struct ImmediateCrossCopyGate(std::sync::Arc<tokio::sync::Notify>);

#[async_trait::async_trait]
impl CrossCopyAuthorizedUploadGate for ImmediateCrossCopyGate {
    async fn take_authorized_upload(
        &self,
        _prepare: CrossCopyAuthorizedPrepare,
    ) -> Result<Box<dyn CrossCopyAuthorizedUploadOwner>, CrossCopyAuthorizedUploadError> {
        Ok(Box::new(ReceiptCrossCopyOwner(self.0.clone())))
    }
}

struct ReceiptCrossCopyOwner(std::sync::Arc<tokio::sync::Notify>);

#[async_trait::async_trait]
impl CrossCopyAuthorizedUploadOwner for ReceiptCrossCopyOwner {
    fn cancellation(&self) -> CancellationToken {
        CancellationToken::new()
    }

    async fn receive(
        self: Box<Self>,
        upload: CrossCopyAuthorizedUpload,
    ) -> Result<CrossCopyAuthorizedUploadReceipt, CrossCopyAuthorizedUploadError> {
        let mut body = upload.into_body();
        let mut size = 0_u64;
        while let Some(chunk) = body.next_chunk().await {
            size += chunk
                .map_err(|_| CrossCopyAuthorizedUploadError::Failed)?
                .len() as u64;
        }
        self.0.notify_one();
        Ok(CrossCopyAuthorizedUploadReceipt::new(
            PathBuf::from("protected.bin"),
            size,
        ))
    }

    async fn cancel(self: Box<Self>) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn protected_upload_stays_busy_until_terminal_events_unblock_its_response() {
    let save_dir = tempfile::tempdir().unwrap();
    let receive_done = std::sync::Arc::new(tokio::sync::Notify::new());
    let (mut server, mut events) = LocalSendServer::builder()
        .port(0)
        .save_dir(save_dir.path())
        .protocol(Protocol::Http)
        .crosscopy_authorized_upload_gate(std::sync::Arc::new(ImmediateCrossCopyGate(
            receive_done.clone(),
        )))
        .build()
        .await
        .unwrap();
    localsend_rs::crypto::ensure_crypto_provider();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = format!("http://127.0.0.1:{}", server.port());

    // Fill the event queue. CrossCopy's terminal FileReceived and SessionDone
    // events use awaited sends, so the upload response must remain in flight.
    for _ in 0..64 {
        let registered = client
            .post(format!("{base}/api/localsend/v2/register"))
            .json(&sender())
            .send()
            .await
            .unwrap();
        assert_eq!(registered.status(), reqwest::StatusCode::OK);
    }
    assert_eq!(
        server.stop_for_update().await,
        Err(ServerQuiescenceError::NotConfirmed),
        "the server must not release its listener before the barrier is confirmed"
    );

    let prepared = client
        .post(format!("{base}/api/localsend/v2/prepare-upload"))
        .header(
            localsend_rs::server::CROSSCOPY_FILE_V3_HANDOFF_HEADER,
            "b".repeat(64),
        )
        .json(&json!({
            "info": {
                "alias": "protected sender",
                "version": "2.1",
                "deviceType": "desktop",
                "fingerprint": "protected-fingerprint",
                "port": 53317,
                "protocol": "http",
                "download": false
            },
            "files": {
                "protected-file": {
                    "id": "protected-file",
                    "fileName": "protected.bin",
                    "size": 1,
                    "fileType": "application/octet-stream"
                }
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(prepared.status(), reqwest::StatusCode::OK);
    let prepared: Value = prepared.json().await.unwrap();
    let session = prepared["sessionId"].as_str().unwrap();
    let file_id = prepared["files"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap();
    let token = prepared["files"][file_id].as_str().unwrap();
    let upload_url = format!(
        "{base}/api/localsend/v2/upload?sessionId={session}&fileId={file_id}&token={token}"
    );
    let upload =
        tokio::spawn(async move { client.post(upload_url).body("x").send().await.unwrap() });
    receive_done.notified().await;

    assert!(
        !upload.is_finished(),
        "terminal events have not been consumed"
    );
    for _ in 0..3 {
        assert_eq!(
            server.prepare_update_quiescence().await,
            Err(ServerQuiescenceError::Busy),
            "a committed upload whose terminal response is still blocked must remain active"
        );
        tokio::task::yield_now().await;
    }

    // One slot releases FileReceived; the next releases SessionDone.
    events.recv().await.unwrap();
    events.recv().await.unwrap();
    assert_eq!(upload.await.unwrap().status(), reqwest::StatusCode::OK);
    assert_eq!(server.prepare_update_quiescence().await, Ok(()));
    server.stop_for_update().await.unwrap();
}

#[tokio::test]
async fn an_admitted_upload_remains_busy_after_remote_cancel_clears_its_session() {
    let save_dir = tempfile::tempdir().unwrap();
    let (mut server, _events) = LocalSendServer::builder()
        .port(0)
        .save_dir(save_dir.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .unwrap();
    let input = tempfile::tempdir().unwrap();
    let path = input.path().join("slow.bin");
    tokio::fs::write(&path, b"still receiving").await.unwrap();
    let metadata = build_file_metadata(&path).await.unwrap();
    let file_id = metadata.id.clone();
    let client = LocalSendClient::new(sender());
    let receiver = target(&server);
    let prepared = client
        .prepare_upload(
            &receiver,
            HashMap::from([(file_id.clone(), metadata)]),
            None,
        )
        .await
        .unwrap();
    let token = prepared.files.get(&file_id).unwrap();

    let upload_url = format!(
        "http://127.0.0.1:{}/api/localsend/v2/upload?sessionId={}&fileId={}&token={}",
        server.port(),
        prepared.session_id,
        file_id,
        token
    );
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel::<Bytes>();
    let body = reqwest::Body::wrap_stream(stream::once(async move {
        let _ = started_tx.send(());
        Ok::<_, std::io::Error>(release_rx.await.expect("test releases upload body"))
    }));
    let upload = tokio::spawn(async move {
        reqwest::Client::new()
            .post(upload_url)
            .header(reqwest::header::CONTENT_LENGTH, b"still receiving".len())
            .body(body)
            .send()
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(3), started_rx)
        .await
        .expect("the server should begin polling the active upload")
        .unwrap();

    let cancelled = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{}/api/localsend/v2/cancel?sessionId={}",
            server.port(),
            prepared.session_id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(cancelled.status(), reqwest::StatusCode::OK);
    assert_eq!(
        server.prepare_update_quiescence().await,
        Err(ServerQuiescenceError::Busy),
        "an admitted upload still owns cleanup even after its session slot was cleared"
    );

    release_tx
        .send(Bytes::from_static(b"still receiving"))
        .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(3), upload)
        .await
        .expect("cancelled upload should settle")
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(server.prepare_update_quiescence().await, Ok(()));
    server.cancel_update_quiescence().await.unwrap();
    server.stop().await;
}

#[tokio::test]
async fn admission_racing_the_barrier_is_either_counted_busy_or_rejected() {
    let save_dir = tempfile::tempdir().unwrap();
    let (mut server, _events) = LocalSendServer::builder()
        .port(0)
        .save_dir(save_dir.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .unwrap();

    for index in 0..24 {
        let name = format!("race-{index}.bin");
        let (barrier, response) = tokio::join!(
            server.prepare_update_quiescence(),
            raw_prepare(server.port(), &name)
        );
        match (barrier, response.status()) {
            (Ok(()), reqwest::StatusCode::SERVICE_UNAVAILABLE) => {}
            (Err(ServerQuiescenceError::Busy), reqwest::StatusCode::OK) => {
                server.cancel_incoming().await.unwrap();
                assert_eq!(server.prepare_update_quiescence().await, Ok(()));
            }
            (barrier, status) => panic!(
                "prepare/barrier race must have one linearized winner; got {barrier:?} and {status}"
            ),
        }
        server.cancel_update_quiescence().await.unwrap();
    }
    server.stop().await;
}

#[tokio::test]
async fn idle_barrier_blocks_then_resumes_admission_and_reports_stopped_server() {
    let save_dir = tempfile::tempdir().unwrap();
    let (mut server, _events) = LocalSendServer::builder()
        .port(0)
        .save_dir(save_dir.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .unwrap();
    let port = server.port();

    assert_eq!(server.prepare_update_quiescence().await, Ok(()));
    assert_eq!(
        raw_prepare(port, "paused.bin").await.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    server.cancel_update_quiescence().await.unwrap();
    assert_eq!(
        raw_prepare(port, "resumed.bin").await.status(),
        reqwest::StatusCode::OK
    );
    server.cancel_incoming().await.unwrap();

    server.stop().await;
    assert_eq!(
        server.prepare_update_quiescence().await,
        Err(ServerQuiescenceError::NotRunning)
    );
}

#[cfg(feature = "https")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn https_update_stop_releases_its_listener_before_acknowledging() {
    let save_dir = tempfile::tempdir().unwrap();
    localsend_rs::crypto::ensure_crypto_provider();
    let (mut server, _events) = LocalSendServer::builder()
        .port(0)
        .save_dir(save_dir.path())
        .protocol(Protocol::Https)
        .auto_accept(true)
        .build()
        .await
        .unwrap();
    let port = server.port();

    assert_eq!(server.prepare_update_quiescence().await, Ok(()));
    tokio::time::timeout(Duration::from_secs(5), server.stop_for_update())
        .await
        .expect("HTTPS graceful stop should finish")
        .unwrap();
    std::net::TcpListener::bind(("0.0.0.0", port))
        .expect("the HTTPS port must be released before stop acknowledgement");
}

#[tokio::test]
async fn web_share_session_drains_before_the_barrier_can_succeed() {
    let save_dir = tempfile::tempdir().unwrap();
    let (mut server, mut events) = LocalSendServer::builder()
        .port(0)
        .save_dir(save_dir.path())
        .protocol(Protocol::Http)
        .build()
        .await
        .unwrap();
    server
        .start_web_share(
            vec![localsend_rs::server::WebShareFile::inline(
                "shared.bin",
                b"web share bytes".to_vec(),
            )],
            None,
            false,
        )
        .await
        .unwrap();

    localsend_rs::crypto::ensure_crypto_provider();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let prepare_url = format!(
        "http://127.0.0.1:{}/api/localsend/v2/prepare-download",
        server.port()
    );
    let pending_prepare_url = prepare_url.clone();
    let pending_prepare = tokio::spawn(async move {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(pending_prepare_url)
            .send()
            .await
            .unwrap()
    });
    let request = loop {
        if let Some(ServerEvent::WebShareRequest(request)) = events.recv().await {
            break request;
        }
    };

    assert_eq!(
        server.prepare_update_quiescence().await,
        Err(ServerQuiescenceError::Busy),
        "an unanswered Web Share consent request is a pending transfer"
    );
    assert_eq!(
        client.post(&prepare_url).send().await.unwrap().status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    server
        .respond_web_share(request.session_id(), true)
        .await
        .unwrap();

    let prepared = pending_prepare.await.unwrap();
    assert_eq!(prepared.status(), reqwest::StatusCode::OK);
    let prepared: Value = prepared.json().await.unwrap();
    let session = prepared["sessionId"].as_str().unwrap();
    let file_id = prepared["files"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap();

    let download = client
        .get(format!(
            "http://127.0.0.1:{}/api/localsend/v2/download?sessionId={session}&fileId={file_id}",
            server.port()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(download.bytes().await.unwrap().as_ref(), b"web share bytes");
    loop {
        if matches!(
            tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .expect("share completion event")
                .expect("event stream"),
            ServerEvent::WebShareSessionDone { .. }
        ) {
            break;
        }
    }

    assert_eq!(server.prepare_update_quiescence().await, Ok(()));
    assert_eq!(
        client
            .get(format!(
                "http://127.0.0.1:{}/api/localsend/v2/download?sessionId={session}&fileId={file_id}",
                server.port()
            ))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "a completed Web Share session cannot replay a download after quiescence"
    );
    server.cancel_update_quiescence().await.unwrap();
    server.stop().await;
}
