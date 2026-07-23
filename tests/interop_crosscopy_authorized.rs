mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use crosscopy_file_service::{
    FileOffer, FileTransferSource, FileV3SenderBinding, PendingFileV3Send, TransferPeer,
};
use crosscopy_ipc::file::FileV3HandoffReady;
use localsend_rs::server::{
    CROSSCOPY_FILE_V3_HANDOFF_HEADER, CrossCopyAuthorizedPrepare, CrossCopyAuthorizedUpload,
    CrossCopyAuthorizedUploadError, CrossCopyAuthorizedUploadGate, CrossCopyAuthorizedUploadOwner,
    CrossCopyAuthorizedUploadReceipt, LocalSendServer,
};
use localsend_rs::{
    DeviceInfo, FileId, FileMetadata, LocalSendClient, PrepareUploadRequest, PrepareUploadResponse,
    Protocol, Token,
};
use prost::Message;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

const HANDOFF: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SECOND_HANDOFF: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct Shared {
    first_taken: AtomicBool,
    second_taken: AtomicBool,
    prepares: AtomicUsize,
    received: Mutex<Vec<Vec<u8>>>,
    cancellations: AtomicUsize,
    block_cancel: AtomicBool,
    cancel_entered: Notify,
    release_cancel: Notify,
    block_receive: AtomicBool,
    receive_entered: Notify,
    release_receive: Notify,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            first_taken: AtomicBool::new(false),
            second_taken: AtomicBool::new(false),
            prepares: AtomicUsize::new(0),
            received: Mutex::new(Vec::new()),
            cancellations: AtomicUsize::new(0),
            block_cancel: AtomicBool::new(false),
            cancel_entered: Notify::new(),
            release_cancel: Notify::new(),
            block_receive: AtomicBool::new(false),
            receive_entered: Notify::new(),
            release_receive: Notify::new(),
        }
    }
}

struct Gate {
    shared: Arc<Shared>,
}

struct Owner {
    shared: Arc<Shared>,
    cancellation: CancellationToken,
}

#[async_trait]
impl CrossCopyAuthorizedUploadGate for Gate {
    async fn take_authorized_upload(
        &self,
        prepare: CrossCopyAuthorizedPrepare,
    ) -> Result<Box<dyn CrossCopyAuthorizedUploadOwner>, CrossCopyAuthorizedUploadError> {
        let (handoff, metadata) = prepare.into_parts();
        let was_taken = handoff.with_value(|value| match value {
            HANDOFF => Some(self.shared.first_taken.swap(true, Ordering::SeqCst)),
            SECOND_HANDOFF => Some(self.shared.second_taken.swap(true, Ordering::SeqCst)),
            _ => None,
        });
        let Some(was_taken) = was_taken else {
            return Err(CrossCopyAuthorizedUploadError::Refused);
        };
        if was_taken || metadata.file().file_name != "protected.bin" {
            return Err(CrossCopyAuthorizedUploadError::Refused);
        }
        self.shared.prepares.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Owner {
            shared: self.shared.clone(),
            cancellation: CancellationToken::new(),
        }))
    }
}

#[async_trait]
impl CrossCopyAuthorizedUploadOwner for Owner {
    fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }
    async fn receive(
        self: Box<Self>,
        upload: CrossCopyAuthorizedUpload,
    ) -> Result<CrossCopyAuthorizedUploadReceipt, CrossCopyAuthorizedUploadError> {
        assert_eq!(upload.metadata().file().file_name, "protected.bin");
        if self.shared.block_receive.load(Ordering::SeqCst) {
            self.shared.receive_entered.notify_one();
            tokio::select! {
                _ = self.cancellation.cancelled() => return Err(CrossCopyAuthorizedUploadError::Failed),
                _ = self.shared.release_receive.notified() => {},
            }
        }
        let mut body = upload.into_body();
        let mut bytes = Vec::new();
        loop {
            let chunk = tokio::select! {
                _ = self.cancellation.cancelled() => return Err(CrossCopyAuthorizedUploadError::Failed),
                chunk = body.next_chunk() => chunk,
            };
            let Some(chunk) = chunk else {
                break;
            };
            bytes.extend_from_slice(&chunk.map_err(|_| CrossCopyAuthorizedUploadError::Failed)?);
        }
        self.shared.received.lock().await.push(bytes);
        Ok(CrossCopyAuthorizedUploadReceipt::new(
            std::path::PathBuf::from("/protected-test-output/protected.bin"),
            9,
        ))
    }

    async fn cancel(self: Box<Self>) {
        self.shared.cancellations.fetch_add(1, Ordering::SeqCst);
        if self.shared.block_cancel.load(Ordering::SeqCst) {
            self.shared.cancel_entered.notify_one();
            self.shared.release_cancel.notified().await;
        }
    }
}

fn prepare_request(file_name: &str, preview: Option<&str>) -> PrepareUploadRequest {
    let id = FileId::from_string("protected-file".to_string());
    PrepareUploadRequest {
        info: DeviceInfo::new("Fabric Sender".into(), 0, Protocol::Http),
        files: std::iter::once((
            id.clone(),
            FileMetadata {
                id,
                file_name: file_name.to_string(),
                size: 9,
                file_type: "application/octet-stream".to_string(),
                sha256: None,
                preview: preview.map(ToOwned::to_owned),
                metadata: None,
            },
        ))
        .collect(),
    }
}

async fn start_with_gate(shared: Arc<Shared>) -> (LocalSendServer, u16, tempfile::TempDir) {
    let output = tempfile::tempdir().expect("output");
    let (server, _events) = LocalSendServer::builder()
        .alias("receiver")
        .port(0)
        .save_dir(output.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .crosscopy_authorized_upload_gate(Arc::new(Gate { shared }))
        .build()
        .await
        .expect("server");
    let port = server.port();
    common::wait_for_http_info(port).await;
    (server, port, output)
}

async fn authorized_request(
    path: &std::path::Path,
    size: u64,
) -> crosscopy_file_service::AuthorizedLocalSendRequest {
    let pending = PendingFileV3Send::new(
        FileV3SenderBinding::new(
            "receiver".into(),
            "session".into(),
            1,
            "channel".into(),
            "offer".into(),
            "decision".into(),
            "instance".into(),
            [0x11; 32],
            "receiver".into(),
        )
        .expect("binding"),
        TransferPeer {
            endpoint_id: "receiver".into(),
            display_name: "receiver".into(),
            reachability_hint: None,
        },
        FileTransferSource::PathSnapshot {
            source_id: "source".into(),
            path: path.to_path_buf(),
            size,
            modified_at_ms: 1,
            digest: [0x22; 32],
        },
        FileOffer {
            offer_id: "offer".into(),
            item_count: 1,
            total_bytes: size,
            manifest_digest: [0x33; 32],
            expires_at_ms: 1_000,
            pairing_provenance: None,
        },
        CancellationToken::new(),
    )
    .expect("pending");
    let ready = FileV3HandoffReady {
        offer_id: "offer".into(),
        decision_id: "decision".into(),
        operation_instance_handle: "instance".into(),
        handoff_token: HANDOFF.into(),
        expires_at_ms: 1_000,
    };
    let payload = ready.encode_to_vec();
    let (mut writer, mut reader) = tokio::io::duplex(1024);
    writer.write_all(&[1]).await.expect("version");
    writer
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await
        .expect("length");
    writer.write_all(&payload).await.expect("payload");
    writer.shutdown().await.expect("EOF");
    pending.receive_ready(&mut reader).await.expect("ready")
}

#[tokio::test]
async fn one_listener_serves_protected_and_standard_modes_without_token_confusion() {
    let shared = Arc::new(Shared::default());
    let (mut server, port, _save) = start_with_gate(shared.clone()).await;
    let http = reqwest::Client::new();
    let prepare_url = format!("http://127.0.0.1:{port}/api/localsend/v2/prepare-upload");

    let prepared = http
        .post(&prepare_url)
        .header("X-CrossCopy-File-V3-Handoff", HANDOFF)
        .json(&prepare_request("protected.bin", None))
        .send()
        .await
        .expect("protected prepare");
    assert_eq!(prepared.status(), reqwest::StatusCode::OK);
    let prepared: PrepareUploadResponse = prepared.json().await.expect("protected response");
    let file_id = FileId::from_string("protected-file".into());
    let upload_token = prepared
        .files
        .get(&file_id)
        .expect("protected upload token");
    let upload_url = format!(
        "http://127.0.0.1:{port}/api/localsend/v2/upload?sessionId={}&fileId={}&token={}",
        prepared.session_id, file_id, upload_token
    );
    assert_eq!(
        http.post(upload_url)
            .body("protected")
            .send()
            .await
            .expect("protected upload")
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(shared.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(
        shared.received.lock().await.as_slice(),
        [b"protected".to_vec()]
    );

    // The same socket remains the normal LocalSend server. Its standard path
    // has no protected header and therefore follows normal auto-accept flow.
    let client = LocalSendClient::new(DeviceInfo::new("standard".into(), 0, Protocol::Http));
    let path_dir = tempfile::tempdir().expect("path dir");
    let path = path_dir.path().join("standard.bin");
    tokio::fs::write(&path, b"standard").await.expect("source");
    let metadata = localsend_rs::build_file_metadata(&path)
        .await
        .expect("metadata");
    let standard_id = metadata.id.clone();
    let prepared_standard = client
        .prepare_upload(
            &common::target_device(port),
            std::iter::once((standard_id.clone(), metadata)).collect(),
            None,
        )
        .await
        .expect("standard prepare on same listener");
    let standard_token = prepared_standard.files.get(&standard_id).expect("token");
    client
        .upload_file(
            &common::target_device(port),
            &prepared_standard.session_id,
            &standard_id,
            standard_token,
            &path,
            None,
        )
        .await
        .expect("standard upload");
    assert_eq!(shared.prepares.load(Ordering::SeqCst), 1);

    server.stop().await;
}

#[tokio::test]
async fn cancel_and_stop_reach_an_owner_while_its_protected_upload_is_active() {
    let shared = Arc::new(Shared::default());
    shared.block_receive.store(true, Ordering::SeqCst);
    let (mut server, port, _save) = start_with_gate(shared.clone()).await;
    let http = reqwest::Client::new();
    let prepare_url = format!("http://127.0.0.1:{port}/api/localsend/v2/prepare-upload");
    let prepared = http
        .post(&prepare_url)
        .header(CROSSCOPY_FILE_V3_HANDOFF_HEADER, HANDOFF)
        .json(&prepare_request("protected.bin", None))
        .send()
        .await
        .expect("prepare");
    let prepared: PrepareUploadResponse = prepared.json().await.expect("prepared");
    let file_id = FileId::from_string("protected-file".into());
    let token = prepared.files.get(&file_id).expect("token");
    let upload_url = format!(
        "http://127.0.0.1:{port}/api/localsend/v2/upload?sessionId={}&fileId={}&token={}",
        prepared.session_id, file_id, token
    );
    let entered = shared.receive_entered.notified();
    let upload = tokio::spawn({
        let http = http.clone();
        async move { http.post(upload_url).body("protected").send().await }
    });
    entered.await;
    assert_eq!(
        http.post(format!(
            "http://127.0.0.1:{port}/api/localsend/v2/cancel?sessionId={}",
            prepared.session_id
        ))
        .send()
        .await
        .expect("cancel")
        .status(),
        reqwest::StatusCode::OK
    );
    let response = tokio::time::timeout(std::time::Duration::from_secs(2), upload)
        .await
        .expect("active upload unblocks")
        .expect("upload join")
        .expect("response");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::INTERNAL_SERVER_ERROR
    );

    // Repeat the active path and use orderly stop rather than `/cancel`.
    let prepared = http
        .post(&prepare_url)
        .header(CROSSCOPY_FILE_V3_HANDOFF_HEADER, SECOND_HANDOFF)
        .json(&prepare_request("protected.bin", None))
        .send()
        .await
        .expect("second prepare");
    let prepared: PrepareUploadResponse = prepared.json().await.expect("second prepared");
    let token = prepared.files.get(&file_id).expect("second token");
    let upload_url = format!(
        "http://127.0.0.1:{port}/api/localsend/v2/upload?sessionId={}&fileId={}&token={}",
        prepared.session_id, file_id, token
    );
    let entered = shared.receive_entered.notified();
    let upload = tokio::spawn({
        let http = http.clone();
        async move { http.post(upload_url).body("protected").send().await }
    });
    entered.await;
    server.stop().await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), upload)
        .await
        .expect("stop unblocks active upload");
}

#[tokio::test]
async fn racing_protected_prepares_take_one_receiver_owned_slot() {
    let shared = Arc::new(Shared::default());
    let (mut server, port, _save) = start_with_gate(shared.clone()).await;
    let url = format!("http://127.0.0.1:{port}/api/localsend/v2/prepare-upload");
    let http = reqwest::Client::new();

    let first = http
        .post(&url)
        .header(CROSSCOPY_FILE_V3_HANDOFF_HEADER, HANDOFF)
        .json(&prepare_request("protected.bin", None))
        .send();
    let second = http
        .post(&url)
        .header(CROSSCOPY_FILE_V3_HANDOFF_HEADER, HANDOFF)
        .json(&prepare_request("protected.bin", None))
        .send();
    let (first, second) = tokio::join!(first, second);
    let first = first.expect("first response");
    let second = second.expect("second response");
    let successful = if first.status() == reqwest::StatusCode::OK {
        assert!(matches!(
            second.status(),
            reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::CONFLICT
        ));
        first
    } else {
        assert!(matches!(
            first.status(),
            reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::CONFLICT
        ));
        assert_eq!(second.status(), reqwest::StatusCode::OK);
        second
    };
    let response: PrepareUploadResponse = successful.json().await.expect("success response");
    assert_eq!(shared.prepares.load(Ordering::SeqCst), 1);

    assert_eq!(
        http.post(format!(
            "http://127.0.0.1:{port}/api/localsend/v2/cancel?sessionId={}",
            response.session_id
        ))
        .send()
        .await
        .expect("protected cancel")
        .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(shared.cancellations.load(Ordering::SeqCst), 1);
    server.stop().await;
}

#[tokio::test]
async fn occupied_protected_session_does_not_consume_a_second_handoff_slot() {
    let shared = Arc::new(Shared::default());
    let (mut server, port, _save) = start_with_gate(shared.clone()).await;
    let http = reqwest::Client::new();
    let prepare_url = format!("http://127.0.0.1:{port}/api/localsend/v2/prepare-upload");

    let first: PrepareUploadResponse = http
        .post(&prepare_url)
        .header(CROSSCOPY_FILE_V3_HANDOFF_HEADER, HANDOFF)
        .json(&prepare_request("protected.bin", None))
        .send()
        .await
        .expect("first prepare")
        .json()
        .await
        .expect("first response");
    assert_eq!(shared.prepares.load(Ordering::SeqCst), 1);
    assert!(shared.first_taken.load(Ordering::SeqCst));

    assert_eq!(
        http.post(&prepare_url)
            .header(CROSSCOPY_FILE_V3_HANDOFF_HEADER, SECOND_HANDOFF)
            .json(&prepare_request("protected.bin", None))
            .send()
            .await
            .expect("occupied response")
            .status(),
        reqwest::StatusCode::CONFLICT
    );
    assert_eq!(shared.prepares.load(Ordering::SeqCst), 1);
    assert!(!shared.second_taken.load(Ordering::SeqCst));

    assert_eq!(
        http.post(format!(
            "http://127.0.0.1:{port}/api/localsend/v2/cancel?sessionId={}",
            first.session_id
        ))
        .send()
        .await
        .expect("first cancel")
        .status(),
        reqwest::StatusCode::OK
    );

    let second: PrepareUploadResponse = http
        .post(&prepare_url)
        .header(CROSSCOPY_FILE_V3_HANDOFF_HEADER, SECOND_HANDOFF)
        .json(&prepare_request("protected.bin", None))
        .send()
        .await
        .expect("second retry")
        .json()
        .await
        .expect("second response");
    assert_eq!(shared.prepares.load(Ordering::SeqCst), 2);
    assert!(shared.second_taken.load(Ordering::SeqCst));
    assert_eq!(
        http.post(format!(
            "http://127.0.0.1:{port}/api/localsend/v2/cancel?sessionId={}",
            second.session_id
        ))
        .send()
        .await
        .expect("second cancel")
        .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(shared.cancellations.load(Ordering::SeqCst), 2);
    server.stop().await;
}

#[tokio::test]
async fn orderly_stop_terminalizes_an_active_protected_owner_exactly_once() {
    let shared = Arc::new(Shared::default());
    let (mut server, port, _save) = start_with_gate(shared.clone()).await;
    let http = reqwest::Client::new();
    let prepare_url = format!("http://127.0.0.1:{port}/api/localsend/v2/prepare-upload");

    assert_eq!(
        http.post(prepare_url)
            .header(CROSSCOPY_FILE_V3_HANDOFF_HEADER, HANDOFF)
            .json(&prepare_request("protected.bin", None))
            .send()
            .await
            .expect("protected prepare")
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(shared.cancellations.load(Ordering::SeqCst), 0);

    server.stop().await;
    assert_eq!(shared.cancellations.load(Ordering::SeqCst), 1);
    server.stop().await;
    assert_eq!(shared.cancellations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stop_closes_protected_admission_before_blocked_owner_cancellation() {
    let shared = Arc::new(Shared::default());
    let (mut server, port, _save) = start_with_gate(shared.clone()).await;
    let http = reqwest::Client::new();
    let prepare_url = format!("http://127.0.0.1:{port}/api/localsend/v2/prepare-upload");

    assert_eq!(
        http.post(&prepare_url)
            .header(CROSSCOPY_FILE_V3_HANDOFF_HEADER, HANDOFF)
            .json(&prepare_request("protected.bin", None))
            .send()
            .await
            .expect("first prepare")
            .status(),
        reqwest::StatusCode::OK
    );
    shared.block_cancel.store(true, Ordering::SeqCst);
    let cancel_entered = shared.cancel_entered.notified();
    let stop = tokio::spawn(async move {
        server.stop().await;
    });
    cancel_entered.await;

    assert_eq!(
        http.post(&prepare_url)
            .header(CROSSCOPY_FILE_V3_HANDOFF_HEADER, SECOND_HANDOFF)
            .json(&prepare_request("protected.bin", None))
            .send()
            .await
            .expect("stopping response")
            .status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(shared.prepares.load(Ordering::SeqCst), 1);
    assert!(!shared.second_taken.load(Ordering::SeqCst));

    shared.release_cancel.notify_one();
    stop.await.expect("stop task");
    assert_eq!(shared.cancellations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn standard_and_protected_upload_tokens_cannot_cross_modes() {
    let shared = Arc::new(Shared::default());
    let (mut server, port, _save) = start_with_gate(shared.clone()).await;
    let http = reqwest::Client::new();
    let prepare_url = format!("http://127.0.0.1:{port}/api/localsend/v2/prepare-upload");

    let protected: PrepareUploadResponse = http
        .post(&prepare_url)
        .header(CROSSCOPY_FILE_V3_HANDOFF_HEADER, HANDOFF)
        .json(&prepare_request("protected.bin", None))
        .send()
        .await
        .expect("protected prepare")
        .json()
        .await
        .expect("protected response");
    let protected_file = FileId::from_string("protected-file".into());
    assert_eq!(
        http.post(format!(
            "http://127.0.0.1:{port}/api/localsend/v2/upload?sessionId={}&fileId={}&token={}",
            protected.session_id,
            protected_file,
            Token::random()
        ))
        .body("wrong protected token")
        .send()
        .await
        .expect("protected mismatch")
        .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    assert_eq!(shared.cancellations.load(Ordering::SeqCst), 1);

    let client = LocalSendClient::new(DeviceInfo::new("standard".into(), 0, Protocol::Http));
    let source_dir = tempfile::tempdir().expect("source");
    let source = source_dir.path().join("standard.bin");
    tokio::fs::write(&source, b"standard")
        .await
        .expect("source bytes");
    let metadata = localsend_rs::build_file_metadata(&source)
        .await
        .expect("metadata");
    let file_id = metadata.id.clone();
    let standard = client
        .prepare_upload(
            &common::target_device(port),
            std::iter::once((file_id.clone(), metadata)).collect(),
            None,
        )
        .await
        .expect("standard prepare");
    assert_eq!(
        http.post(format!(
            "http://127.0.0.1:{port}/api/localsend/v2/upload?sessionId={}&fileId={}&token={}",
            standard.session_id,
            file_id,
            Token::random()
        ))
        .body("wrong standard token")
        .send()
        .await
        .expect("standard mismatch")
        .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    server.stop().await;
}

#[tokio::test]
async fn protected_header_is_fail_closed_without_hook_and_on_wrong_routes() {
    let save = tempfile::tempdir().expect("save");
    let (mut server, _events) = LocalSendServer::builder()
        .port(0)
        .save_dir(save.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .expect("server");
    let port = server.port();
    common::wait_for_http_info(port).await;
    let http = reqwest::Client::new();
    let prepare_url = format!("http://127.0.0.1:{port}/api/localsend/v2/prepare-upload");
    assert_eq!(
        http.post(prepare_url)
            .header(CROSSCOPY_FILE_V3_HANDOFF_HEADER, HANDOFF)
            .json(&prepare_request("protected.bin", None))
            .send()
            .await
            .expect("response")
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    assert_eq!(
        http.get(format!("http://127.0.0.1:{port}/api/localsend/v2/info"))
            .header(CROSSCOPY_FILE_V3_HANDOFF_HEADER, HANDOFF)
            .send()
            .await
            .expect("response")
            .status(),
        reqwest::StatusCode::BAD_REQUEST
    );
    server.stop().await;
}

#[tokio::test]
async fn protected_prepare_rejects_noncanonical_multi_value_and_text_shapes_before_gate() {
    let shared = Arc::new(Shared::default());
    let (mut server, port, _save) = start_with_gate(shared.clone()).await;
    let http = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/api/localsend/v2/prepare-upload");
    for request in [
        http.post(&url)
            .header(CROSSCOPY_FILE_V3_HANDOFF_HEADER, "A".repeat(64))
            .json(&prepare_request("protected.bin", None)),
        http.post(&url)
            .header(CROSSCOPY_FILE_V3_HANDOFF_HEADER, HANDOFF)
            .header(CROSSCOPY_FILE_V3_HANDOFF_HEADER, HANDOFF)
            .json(&prepare_request("protected.bin", None)),
        http.post(&url)
            .header(CROSSCOPY_FILE_V3_HANDOFF_HEADER, HANDOFF)
            .json(&prepare_request("protected.bin", Some("text"))),
    ] {
        assert_eq!(
            request.send().await.expect("response").status(),
            reqwest::StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(shared.prepares.load(Ordering::SeqCst), 0);
    server.stop().await;
}

#[tokio::test]
async fn typed_client_consumes_ready_token_only_for_protected_prepare() {
    let shared = Arc::new(Shared::default());
    let (mut server, port, _save) = start_with_gate(shared.clone()).await;
    let source_dir = tempfile::tempdir().expect("source");
    let source = source_dir.path().join("protected.bin");
    tokio::fs::write(&source, b"protected")
        .await
        .expect("source bytes");
    let client = LocalSendClient::new(DeviceInfo::new("sender".into(), 0, Protocol::Http));
    let request = authorized_request(&source, 9).await;
    let target = common::target_device(port);
    let bytes = request
        .send_with(|http| async move {
            client
                .send_crosscopy_authorized_file(&target, http)
                .await
                .map_err(|error| {
                    crosscopy_file_service::FileV3SenderError::Outbound(error.to_string())
                })
        })
        .await
        .expect("typed protected client send");
    assert_eq!(bytes, 9);
    assert_eq!(shared.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(
        shared.received.lock().await.as_slice(),
        [b"protected".to_vec()]
    );
    server.stop().await;
}
