mod common;

use localsend_rs::server::LocalSendServer;
use localsend_rs::{DeviceInfo, LocalSendClient, Protocol, build_file_metadata};
use std::collections::HashMap;

async fn receiver(save: &std::path::Path) -> (LocalSendServer, u16) {
    let (server, _events) = LocalSendServer::builder()
        .alias("R")
        .port(0)
        .save_dir(save)
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .unwrap();
    let port = server.port();
    common::wait_for_http_info(port).await;
    (server, port)
}

fn client() -> LocalSendClient {
    let mut d = DeviceInfo::new("S".to_string(), 0, Protocol::Http);
    d.fingerprint = "s-fp".to_string();
    LocalSendClient::new(d)
}

#[tokio::test]
async fn multi_file_session_completes_and_frees_the_slot() {
    let save = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let (_server, port) = receiver(save.path()).await;
    let c = client();
    let target = common::target_device(port);

    // 3 files in ONE session
    let mut files = HashMap::new();
    let mut paths = HashMap::new();
    let mut shas = HashMap::new();
    for name in ["one.bin", "two.bin", "three.bin"] {
        let (p, sha) = common::make_random_file(src.path(), name, 2048);
        let m = build_file_metadata(&p).await.unwrap();
        paths.insert(m.id.clone(), p);
        shas.insert(name.to_string(), sha);
        files.insert(m.id.clone(), m);
    }
    let prep = c.prepare_upload(&target, files, None).await.unwrap();
    assert_eq!(prep.files.len(), 3);
    for (id, token) in &prep.files {
        c.upload_file(&target, &prep.session_id, id, token, &paths[id], None)
            .await
            .unwrap();
    }
    for name in ["one.bin", "two.bin", "three.bin"] {
        let got = localsend_rs::sha256_from_file(&save.path().join(name))
            .await
            .unwrap();
        assert_eq!(&got, shas.get(name).unwrap());
    }

    // Session must be CLOSED now: a new prepare-upload succeeds (no 409). (R5)
    let (p2, _) = common::make_random_file(src.path(), "again.bin", 128);
    let m2 = build_file_metadata(&p2).await.unwrap();
    let mut f2 = HashMap::new();
    f2.insert(m2.id.clone(), m2);
    let second = c.prepare_upload(&target, f2, None).await;
    assert!(
        second.is_ok(),
        "expected new session after completion, got {second:?}"
    );
}

#[tokio::test]
async fn concurrent_second_session_is_blocked_with_409() {
    let save = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let (_server, port) = receiver(save.path()).await;
    let c = client();
    let target = common::target_device(port);

    let (p, _) = common::make_random_file(src.path(), "held.bin", 128);
    let m = build_file_metadata(&p).await.unwrap();
    let mut f = HashMap::new();
    f.insert(m.id.clone(), m);
    let _prep = c.prepare_upload(&target, f, None).await.unwrap(); // session open, file NOT uploaded

    let (p2, _) = common::make_random_file(src.path(), "blocked.bin", 128);
    let m2 = build_file_metadata(&p2).await.unwrap();
    let mut f2 = HashMap::new();
    f2.insert(m2.id.clone(), m2);
    let err = c
        .prepare_upload(&target, f2, None)
        .await
        .expect_err("blocked");
    assert!(matches!(err, localsend_rs::LocalSendError::SessionBlocked));
}
