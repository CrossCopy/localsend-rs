mod common;

use localsend_rs::server::{LocalSendServer, PendingTransfer};
use localsend_rs::{DeviceInfo, LocalSendClient, Protocol, build_file_metadata, sha256_from_file};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[tokio::test]
async fn uploads_a_file_byte_for_byte_rs_to_rs() {
    let port = common::free_port();
    let save_dir = tempfile::tempdir().expect("save dir");
    let src_dir = tempfile::tempdir().expect("src dir");

    // --- receiver ---
    let mut device = DeviceInfo::new("Test Receiver".to_string(), port, Protocol::Http);
    device.fingerprint = "receiver-fp".to_string();
    // Constructing the dummy pending/received Arcs is still required until
    // Task 2.5 removes those `new_with_device` params.
    let pending: Arc<RwLock<Option<PendingTransfer>>> = Arc::new(RwLock::new(None));
    let received = Arc::new(RwLock::new(Vec::new()));
    let mut server = LocalSendServer::new_with_device(
        device,
        save_dir.path().to_path_buf(),
        false,
        pending,
        received,
    )
    .expect("server");
    server.set_auto_accept(true);
    server.start(None).await.expect("start");

    common::wait_for_http_info(port).await;

    // --- sender ---
    let (file_path, want_sha) = common::make_random_file(src_dir.path(), "hello.bin", 1024);
    let meta = build_file_metadata(&file_path).await.expect("metadata");
    let file_id = meta.id.clone();
    let mut files = HashMap::new();
    files.insert(file_id.clone(), meta);

    let mut sender_dev = DeviceInfo::new("Test Sender".to_string(), 0, Protocol::Http);
    sender_dev.fingerprint = "sender-fp".to_string();
    let client = LocalSendClient::new(sender_dev);
    let target = common::target_device(port);

    let prep = client
        .prepare_upload(&target, files, None)
        .await
        .expect("prepare");
    let token = prep.files.get(&file_id).expect("token").clone();
    client
        .upload_file(
            &target,
            &prep.session_id,
            &file_id,
            &token,
            &file_path,
            None,
        )
        .await
        .expect("upload");

    // --- assert byte-identical ---
    let got_sha = sha256_from_file(&save_dir.path().join("hello.bin"))
        .await
        .expect("saved file");
    assert_eq!(got_sha, want_sha);

    server.stop();
}
