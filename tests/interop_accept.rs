mod common;

use localsend_rs::server::{LocalSendServer, ServerEvent};
use localsend_rs::{DeviceInfo, LocalSendClient, LocalSendError, Protocol, build_file_metadata};
use std::collections::HashMap;

async fn start_receiver(
    save_dir: std::path::PathBuf,
) -> (LocalSendServer, tokio::sync::mpsc::Receiver<ServerEvent>) {
    LocalSendServer::builder()
        .alias("Receiver")
        .port(0)
        .save_dir(save_dir)
        .protocol(Protocol::Http)
        .auto_accept(false)
        .build()
        .await
        .expect("build")
}

fn one_file(
    dir: &std::path::Path,
) -> (
    HashMap<localsend_rs::FileId, localsend_rs::FileMetadata>,
    localsend_rs::FileId,
    std::path::PathBuf,
) {
    let (path, _) = common::make_random_file(dir, "a.bin", 512);
    let meta = futures_blocking(build_file_metadata(&path));
    let id = meta.id.clone();
    let mut m = HashMap::new();
    m.insert(id.clone(), meta);
    (m, id, path)
}

// tiny helper: run an async fn to completion inside a test-owned runtime piece
fn futures_blocking<T>(fut: impl std::future::Future<Output = localsend_rs::Result<T>>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut)).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn event_consumer_can_accept_a_transfer() {
    let save = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let (mut server, mut events) = start_receiver(save.path().to_path_buf()).await;
    let port = server.port();
    common::wait_for_http_info(port).await;

    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            if let ServerEvent::TransferRequest(req) = ev {
                assert_eq!(req.sender().alias, "Sender");
                req.accept();
            }
        }
    });

    let (files, id, path) = one_file(src.path());
    let mut dev = DeviceInfo::new("Sender".to_string(), 0, Protocol::Http);
    dev.fingerprint = "sender-fp".to_string();
    let client = LocalSendClient::new(dev);
    let target = common::target_device(port);
    let prep = client
        .prepare_upload(&target, files, None)
        .await
        .expect("accepted");
    let token = prep.files.get(&id).unwrap().clone();
    client
        .upload_file(&target, &prep.session_id, &id, &token, &path, None)
        .await
        .unwrap();
    assert!(save.path().join("a.bin").exists());
    server.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn event_consumer_can_decline_a_transfer() {
    let save = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let (mut server, mut events) = start_receiver(save.path().to_path_buf()).await;
    let port = server.port();
    common::wait_for_http_info(port).await;

    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            if let ServerEvent::TransferRequest(req) = ev {
                req.decline();
            }
        }
    });

    let (files, _id, _path) = one_file(src.path());
    let mut dev = DeviceInfo::new("Sender".to_string(), 0, Protocol::Http);
    dev.fingerprint = "sender-fp".to_string();
    let client = LocalSendClient::new(dev);
    let target = common::target_device(port);
    let err = client
        .prepare_upload(&target, files, None)
        .await
        .expect_err("declined");
    assert!(matches!(err, LocalSendError::Rejected { status: 403 }));
    server.stop();
}
