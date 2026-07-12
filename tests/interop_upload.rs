mod common;

use localsend_rs::server::LocalSendServer;
use localsend_rs::{DeviceInfo, LocalSendClient, Protocol, build_file_metadata, sha256_from_file};
use std::collections::HashMap;

#[tokio::test]
async fn uploads_a_file_byte_for_byte_rs_to_rs() {
    let save_dir = tempfile::tempdir().expect("save dir");
    let src_dir = tempfile::tempdir().expect("src dir");

    // --- receiver ---
    let (mut server, _events) = LocalSendServer::builder()
        .alias("Test Receiver")
        .port(0)
        .save_dir(save_dir.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .expect("build");
    let port = server.port();

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
