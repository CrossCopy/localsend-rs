mod common;

use localsend_rs::server::{LocalSendServer, ServerEvent};
use localsend_rs::{DeviceInfo, LocalSendClient, Protocol, build_file_metadata, sha256_from_file};
use std::collections::HashMap;

#[tokio::test]
async fn failed_upload_resets_file_progress_and_can_be_retried() {
    let output = tempfile::tempdir().unwrap();
    let input = tempfile::tempdir().unwrap();
    let (mut server, mut events) = LocalSendServer::builder()
        .port(0)
        .save_dir(output.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .unwrap();
    let (path, _) = common::make_random_file(input.path(), "retry.bin", 1024);
    let meta = build_file_metadata(&path).await.unwrap();
    let id = meta.id.clone();
    let client = LocalSendClient::new(DeviceInfo::new("sender".into(), 0, Protocol::Http));
    let target = common::target_device(server.port());
    let prep = client
        .prepare_upload(&target, HashMap::from([(id.clone(), meta)]), None)
        .await
        .unwrap();
    let mut url = reqwest::Url::parse(&format!(
        "http://127.0.0.1:{}/api/localsend/v2/upload",
        server.port()
    ))
    .unwrap();
    url.query_pairs_mut().extend_pairs([
        ("sessionId", prep.session_id.as_str()),
        ("fileId", id.as_str()),
        ("token", prep.files[&id].as_str()),
    ]);
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(url)
        .body(vec![1_u8; 512])
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        reqwest::StatusCode::INTERNAL_SERVER_ERROR
    );
    let mut last_progress = None;
    while let Ok(event) = events.try_recv() {
        if let ServerEvent::FileReceiveProgress {
            bytes_received,
            file_bytes_received,
            file_size,
            ..
        } = event
        {
            assert_eq!(file_size, 1024);
            last_progress = Some((bytes_received, file_bytes_received));
        }
    }
    assert_eq!(last_progress, Some((0, 0)));
    assert_eq!(std::fs::read_dir(output.path()).unwrap().count(), 0);
    client
        .upload_file(
            &target,
            &prep.session_id,
            &id,
            &prep.files[&id],
            &path,
            None,
        )
        .await
        .unwrap();
    while let Ok(event) = events.try_recv() {
        if let ServerEvent::FileReceiveProgress {
            bytes_received,
            file_bytes_received,
            ..
        } = event
        {
            assert_eq!(bytes_received, file_bytes_received);
            last_progress = Some((bytes_received, file_bytes_received));
        }
    }
    assert_eq!(last_progress, Some((1024, 1024)));
    server.stop().await;
}

#[tokio::test]
async fn uploads_a_file_byte_for_byte_rs_to_rs() {
    let save_dir = tempfile::tempdir().expect("save dir");
    let src_dir = tempfile::tempdir().expect("src dir");

    // --- receiver ---
    let (mut server, mut events) = LocalSendServer::builder()
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

    let mut progress_samples = Vec::new();
    let mut saw_file_received = false;
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .expect("receiver event should arrive")
            .expect("receiver event stream should stay open");
        match event {
            ServerEvent::FileReceiveProgress {
                session_id,
                file_id: event_file_id,
                file_name,
                sender_alias,
                bytes_received,
                total_bytes,
                file_bytes_received,
                file_size,
                file_count,
            } => {
                assert_eq!(session_id, prep.session_id);
                assert_eq!(event_file_id, file_id);
                assert_eq!(file_name, "hello.bin");
                assert_eq!(sender_alias, "Test Sender");
                assert_eq!(total_bytes, 1_024);
                assert_eq!(file_bytes_received, bytes_received);
                assert_eq!(file_size, 1_024);
                assert_eq!(file_count, 1);
                progress_samples.push(bytes_received);
            }
            ServerEvent::FileReceived { .. } => saw_file_received = true,
            ServerEvent::SessionDone { .. } => break,
            _ => {}
        }
    }
    assert!(!progress_samples.is_empty());
    assert!(progress_samples.windows(2).all(|pair| pair[0] <= pair[1]));
    assert_eq!(progress_samples.last(), Some(&1_024));
    assert!(saw_file_received);

    // --- assert byte-identical ---
    let got_sha = sha256_from_file(&save_dir.path().join("hello.bin"))
        .await
        .expect("saved file");
    assert_eq!(got_sha, want_sha);

    server.stop().await;
}

#[tokio::test]
async fn multi_file_progress_separates_session_and_current_file_bytes() {
    let save_dir = tempfile::tempdir().expect("save dir");
    let src_dir = tempfile::tempdir().expect("src dir");
    let (mut server, mut events) = LocalSendServer::builder()
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

    let (first_path, _) = common::make_random_file(src_dir.path(), "first.bin", 1_024);
    let (second_path, _) = common::make_random_file(src_dir.path(), "second.bin", 2_048);
    let first = build_file_metadata(&first_path)
        .await
        .expect("first metadata");
    let second = build_file_metadata(&second_path)
        .await
        .expect("second metadata");
    let first_id = first.id.clone();
    let second_id = second.id.clone();
    let mut files = HashMap::new();
    files.insert(first_id.clone(), first);
    files.insert(second_id.clone(), second);

    let mut sender = DeviceInfo::new("Test Sender".into(), 0, Protocol::Http);
    sender.fingerprint = "sender-fp".into();
    let client = LocalSendClient::new(sender);
    let target = common::target_device(port);
    let prep = client
        .prepare_upload(&target, files, None)
        .await
        .expect("prepare");

    client
        .upload_file(
            &target,
            &prep.session_id,
            &first_id,
            prep.files.get(&first_id).expect("first token"),
            &first_path,
            None,
        )
        .await
        .expect("first upload");

    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .expect("first upload timeout")
            .expect("first upload event")
        {
            ServerEvent::FileReceiveProgress {
                file_id,
                bytes_received,
                total_bytes,
                file_bytes_received,
                file_size,
                ..
            } => {
                assert_eq!(file_id, first_id);
                assert_eq!(bytes_received, file_bytes_received);
                assert_eq!(total_bytes, 3_072);
                assert_eq!(file_size, 1_024);
            }
            ServerEvent::FileReceived { file_id, .. } if file_id == first_id => break,
            ServerEvent::SessionDone { .. } => panic!("session ended after only one file"),
            _ => {}
        }
    }

    client
        .upload_file(
            &target,
            &prep.session_id,
            &second_id,
            prep.files.get(&second_id).expect("second token"),
            &second_path,
            None,
        )
        .await
        .expect("second upload");

    let mut final_sample = None;
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .expect("second upload timeout")
            .expect("second upload event")
        {
            ServerEvent::FileReceiveProgress {
                file_id,
                bytes_received,
                total_bytes,
                file_bytes_received,
                file_size,
                ..
            } => {
                assert_eq!(file_id, second_id);
                assert_eq!(bytes_received, 1_024 + file_bytes_received);
                assert_eq!(total_bytes, 3_072);
                assert_eq!(file_size, 2_048);
                final_sample = Some((bytes_received, file_bytes_received));
            }
            ServerEvent::SessionDone { .. } => break,
            _ => {}
        }
    }
    assert_eq!(final_sample, Some((3_072, 2_048)));
    server.stop().await;
}

#[tokio::test]
async fn upload_completes_when_progress_event_channel_is_not_drained() {
    let save_dir = tempfile::tempdir().expect("save dir");
    let src_dir = tempfile::tempdir().expect("src dir");
    let (mut server, _unread_events) = LocalSendServer::builder()
        .alias("Backpressured Receiver")
        .port(0)
        .save_dir(save_dir.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .expect("build");
    let port = server.port();
    common::wait_for_http_info(port).await;

    let (file_path, want_sha) =
        common::make_random_file(src_dir.path(), "large.bin", 8 * 1024 * 1024);
    let meta = build_file_metadata(&file_path).await.expect("metadata");
    let file_id = meta.id.clone();
    let mut files = HashMap::new();
    files.insert(file_id.clone(), meta);

    let mut sender = DeviceInfo::new("Sender".into(), 0, Protocol::Http);
    sender.fingerprint = "sender-fp".into();
    let client = LocalSendClient::new(sender);
    let target = common::target_device(port);
    let prep = client
        .prepare_upload(&target, files, None)
        .await
        .expect("prepare");
    let token = prep.files.get(&file_id).expect("token").clone();

    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        client.upload_file(
            &target,
            &prep.session_id,
            &file_id,
            &token,
            &file_path,
            None,
        ),
    )
    .await
    .expect("full progress channel must not block upload")
    .expect("upload");

    let got_sha = sha256_from_file(&save_dir.path().join("large.bin"))
        .await
        .expect("saved file");
    assert_eq!(got_sha, want_sha);
    server.stop().await;
}
