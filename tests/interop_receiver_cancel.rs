use localsend_rs::core::{AtomicFileSink, PendingReceive, ReceiveSink, SinkError};
use localsend_rs::protocol::{PrepareUploadRequest, PrepareUploadResponse};
use localsend_rs::server::{LocalSendServer, PendingRequest, ServerEvent};
use localsend_rs::{DeviceInfo, FileId, FileMetadata, Protocol};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Notify, mpsc};

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

fn offer() -> PrepareUploadRequest {
    let id = FileId::from_string("test".into());
    PrepareUploadRequest {
        info: DeviceInfo::new("Sender".into(), 0, Protocol::Http),
        files: HashMap::from([(
            id.clone(),
            FileMetadata {
                id,
                file_name: "received.txt".into(),
                size: 3,
                file_type: "application/octet-stream".into(),
                sha256: None,
                preview: None,
                metadata: None,
            },
        )]),
    }
}

fn base(server: &LocalSendServer) -> String {
    format!("http://127.0.0.1:{}/api/localsend/v2", server.port())
}

async fn prepare(client: &reqwest::Client, base: &str) -> PrepareUploadResponse {
    client
        .post(format!("{base}/prepare-upload"))
        .json(&offer())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn upload_request(
    client: &reqwest::Client,
    base: &str,
    prep: &PrepareUploadResponse,
) -> reqwest::RequestBuilder {
    let id = FileId::from_string("test".into());
    let mut url = reqwest::Url::parse(&format!("{base}/upload")).unwrap();
    url.query_pairs_mut().extend_pairs([
        ("sessionId", prep.session_id.as_str()),
        ("fileId", id.as_str()),
        ("token", prep.files[&id].as_str()),
    ]);
    client.post(url)
}

async fn next(events: &mut mpsc::Receiver<ServerEvent>) -> ServerEvent {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap()
}

async fn pending(events: &mut mpsc::Receiver<ServerEvent>) -> PendingRequest {
    loop {
        if let ServerEvent::TransferRequest(request) = next(events).await {
            return request;
        }
    }
}

#[tokio::test]
async fn cancel_withdraws_consent_without_affecting_the_next_offer() {
    let root = tempfile::tempdir().unwrap();
    let (mut server, mut events) = LocalSendServer::builder()
        .port(0)
        .save_dir(root.path())
        .protocol(Protocol::Http)
        .auto_accept(false)
        .build()
        .await
        .unwrap();
    assert!(server.cancel_incoming().await.unwrap().is_none());
    let client = client();
    let base = base(&server);
    let first = tokio::spawn({
        let client = client.clone();
        let base = base.clone();
        async move {
            client
                .post(format!("{base}/prepare-upload"))
                .json(&offer())
                .send()
                .await
                .unwrap()
        }
    });
    let stale_request = pending(&mut events).await;
    let cancelled_id = server.cancel_incoming().await.unwrap().unwrap();
    assert_eq!(
        first.await.unwrap().status(),
        reqwest::StatusCode::FORBIDDEN
    );

    let second = tokio::spawn({
        let client = client.clone();
        let base = base.clone();
        async move { prepare(&client, &base).await }
    });
    let request = pending(&mut events).await;
    stale_request.accept();
    request.accept();
    let prep = second.await.unwrap();
    assert_ne!(prep.session_id, cancelled_id);
    assert!(
        upload_request(&client, &base, &prep)
            .body("new")
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert_eq!(
        std::fs::read(root.path().join("received.txt")).unwrap(),
        b"new"
    );
    server.stop().await;
}

#[tokio::test]
async fn cancel_accepted_offer_preserves_listener_and_pooled_client() {
    let root = tempfile::tempdir().unwrap();
    let (mut server, mut events) = LocalSendServer::builder()
        .port(0)
        .save_dir(root.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .unwrap();
    let client = client();
    let base = base(&server);
    let identity: DeviceInfo = client
        .get(format!("{base}/info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let first = prepare(&client, &base).await;
    assert_eq!(
        server.cancel_incoming().await.unwrap(),
        Some(first.session_id.clone())
    );
    assert!(
        matches!(next(&mut events).await, ServerEvent::SessionDone { session_id } if session_id == first.session_id)
    );
    assert!(server.cancel_incoming().await.unwrap().is_none());
    assert!(
        !upload_request(&client, &base, &first)
            .body("old")
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let second = prepare(&client, &base).await;
    assert!(
        upload_request(&client, &base, &second)
            .body("new")
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let current: DeviceInfo = client
        .get(format!("{base}/info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(current.port, identity.port);
    assert_eq!(current.fingerprint, identity.fingerprint);
    assert_eq!(
        std::fs::read(root.path().join("received.txt")).unwrap(),
        b"new"
    );
    server.stop().await;
}

#[tokio::test]
async fn midstream_cancel_cleans_partial_files_before_returning() {
    let root = tempfile::tempdir().unwrap();
    let (mut server, mut events) = LocalSendServer::builder()
        .port(0)
        .save_dir(root.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .unwrap();
    let client = client();
    let base = base(&server);
    let prep = prepare(&client, &base).await;
    let (chunks, stream) = mpsc::channel::<Result<Vec<u8>, std::io::Error>>(2);
    let stream = futures_util::stream::unfold(stream, |mut rx| async {
        rx.recv().await.map(|chunk| (chunk, rx))
    });
    let request = upload_request(&client, &base, &prep).body(reqwest::Body::wrap_stream(stream));
    let upload = tokio::spawn(request.send());
    chunks.send(Ok(b"n".to_vec())).await.unwrap();
    while !matches!(
        next(&mut events).await,
        ServerEvent::FileReceiveProgress { .. }
    ) {}
    assert_eq!(
        server.cancel_incoming().await.unwrap(),
        Some(prep.session_id)
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    drop(chunks);
    if let Ok(response) = upload.await.unwrap() {
        assert!(!response.status().is_success());
    }
    let next = prepare(&client, &base).await;
    assert!(
        upload_request(&client, &base, &next)
            .body("new")
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert_eq!(
        std::fs::read(root.path().join("received.txt")).unwrap(),
        b"new"
    );
    server.stop().await;
}

#[derive(Default)]
struct Gate {
    entered: Notify,
    release: Notify,
}

struct GatedSink {
    gate: Arc<Gate>,
    create: bool,
}
struct GatedFile {
    inner: Box<dyn PendingReceive>,
    gate: Option<Arc<Gate>>,
}

#[async_trait::async_trait]
impl ReceiveSink for GatedSink {
    async fn create(
        &self,
        directory: &Path,
        name: &str,
    ) -> Result<Box<dyn PendingReceive>, SinkError> {
        if self.create {
            self.gate.entered.notify_one();
            self.gate.release.notified().await;
        }
        Ok(Box::new(GatedFile {
            inner: AtomicFileSink.create(directory, name).await?,
            gate: (!self.create).then(|| self.gate.clone()),
        }))
    }
}

#[async_trait::async_trait]
impl PendingReceive for GatedFile {
    fn writer(&mut self) -> &mut (dyn tokio::io::AsyncWrite + Unpin + Send) {
        self.inner.writer()
    }
    fn display_path(&self) -> &Path {
        self.inner.display_path()
    }
    async fn commit(self: Box<Self>) -> Result<PathBuf, SinkError> {
        if let Some(gate) = &self.gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        self.inner.commit().await
    }
    async fn abort(self: Box<Self>) -> Result<(), SinkError> {
        self.inner.abort().await
    }
}

async fn cancellation_waits_for_sink(create: bool) {
    let root = tempfile::tempdir().unwrap();
    let gate = Arc::new(Gate::default());
    let (server, mut events) = LocalSendServer::builder()
        .port(0)
        .save_dir(root.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .sink(Arc::new(GatedSink {
            gate: gate.clone(),
            create,
        }))
        .build()
        .await
        .unwrap();
    let server = Arc::new(server);
    let client = client();
    let base = base(&server);
    let prep = prepare(&client, &base).await;
    let upload = tokio::spawn(upload_request(&client, &base, &prep).body("new").send());
    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
    let mut cancellation = Box::pin(server.cancel_incoming());
    // Poll the actual future, ensuring cancellation has started before release.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut cancellation)
            .await
            .is_err()
    );
    gate.release.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), cancellation)
            .await
            .unwrap()
            .unwrap(),
        Some(prep.session_id.clone())
    );
    let response = upload.await.unwrap().unwrap();
    assert_eq!(response.status().is_success(), !create);
    if create {
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    } else {
        assert_eq!(
            std::fs::read(root.path().join("received.txt")).unwrap(),
            b"new"
        );
    }
    let mut done = 0;
    while let Ok(event) = events.try_recv() {
        if matches!(event, ServerEvent::SessionDone { session_id } if session_id == prep.session_id)
        {
            done += 1;
        }
    }
    assert_eq!(
        done, 1,
        "cancellation and publication must not emit duplicate terminal events"
    );
    let next = prepare(&client, &base).await;
    assert_ne!(next.session_id, prep.session_id);
    assert_eq!(
        server.cancel_incoming().await.unwrap(),
        Some(next.session_id)
    );
    let mut server = Arc::try_unwrap(server).ok().unwrap();
    server.stop().await;
}

#[tokio::test]
async fn cancel_waits_for_a_publication_that_already_started() {
    cancellation_waits_for_sink(false).await;
}

#[tokio::test]
async fn cancel_waits_for_an_admitted_upload_before_sink_creation() {
    cancellation_waits_for_sink(true).await;
}
