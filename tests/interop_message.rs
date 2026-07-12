mod common;

use localsend_rs::server::{LocalSendServer, ServerEvent};
use localsend_rs::{DeviceInfo, FileMetadata, LocalSendClient, Protocol};
use std::collections::HashMap;
use std::time::Duration;

/// M1 regression test: the `is_message` branch of `handle_prepare_upload`
/// (triggered when every offered file carries a non-empty `preview` and is
/// under 1 MiB -- the text-message path) used to save the message to disk
/// and return 204 without ever touching `state.events_tx`. That meant a
/// received text message was silently written to the save dir and never
/// surfaced to the CLI event loop or the TUI Inbox, since both are driven
/// purely by `ServerEvent`s.
///
/// This drives the real client-side message path (`LocalSendClient::
/// prepare_upload` with a `FileMetadata` whose `preview` is `Some(text)`,
/// exactly how `src/cli/commands/send.rs` and the TUI's `send_text_message`
/// build a text message) and asserts the receiver now emits a
/// `FileReceived` event for the message file followed by a `SessionDone`,
/// and that the message content actually landed on disk.
#[tokio::test(flavor = "multi_thread")]
async fn message_path_emits_file_received_and_session_done() {
    let save = tempfile::tempdir().expect("save dir");

    let (mut server, mut events) = LocalSendServer::builder()
        .alias("Receiver")
        .port(0)
        .save_dir(save.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .expect("build");
    let port = server.port();
    common::wait_for_http_info(port).await;

    // Build a message-shaped file offer the same way the CLI/TUI senders do:
    // a small file whose `preview` carries the text itself.
    let message_text = "hello world";
    let file_id = localsend_rs::FileId::new();
    let file_name = format!("{}.txt", file_id);
    let metadata = FileMetadata {
        id: file_id.clone(),
        file_name: file_name.clone(),
        size: message_text.len() as u64,
        file_type: "text/plain".to_string(),
        sha256: None,
        preview: Some(message_text.to_string()),
        metadata: None,
    };
    let mut files = HashMap::new();
    files.insert(file_id.clone(), metadata);

    let mut sender_dev = DeviceInfo::new("Test Sender".to_string(), 0, Protocol::Http);
    sender_dev.fingerprint = "sender-fp".to_string();
    let client = LocalSendClient::new(sender_dev);
    let target = common::target_device(port);

    let prep = client
        .prepare_upload(&target, files, None)
        .await
        .expect("prepare-upload should succeed for a message-shaped offer");

    // The message path answers 204: session_id comes back empty and the
    // client must NOT call `upload` (the content already rode along in
    // `preview`).
    assert!(
        prep.session_id.as_str().is_empty(),
        "message-shaped prepare-upload should return 204 (empty session id), got {:?}",
        prep.session_id
    );
    assert!(prep.files.is_empty());

    // Collect events with a bounded wait -- the handler emits them via
    // `try_send` synchronously before responding, but give the receiver
    // loop a moment regardless.
    let file_received = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Some(ServerEvent::FileReceived {
                    file_name,
                    path,
                    size,
                    sender_alias,
                    ..
                }) => return (file_name, path, size, sender_alias),
                Some(_) => continue,
                None => panic!("event channel closed before FileReceived"),
            }
        }
    })
    .await
    .expect("timed out waiting for FileReceived");

    let (recv_file_name, recv_path, recv_size, recv_sender) = file_received;
    assert!(
        recv_file_name.ends_with(".txt"),
        "message file_name should end in .txt, got {recv_file_name}"
    );
    assert_eq!(recv_size, message_text.len() as u64);
    assert_eq!(recv_sender, "Test Sender");

    // The FileReceived path must be the FINAL on-disk name -- read it back
    // and assert the saved content matches the message, not just that a
    // file exists.
    let on_disk = std::fs::read_to_string(&recv_path).expect("message file should exist on disk");
    assert_eq!(on_disk, message_text);
    assert_eq!(
        recv_path.file_name().unwrap().to_string_lossy(),
        recv_file_name
    );

    // SessionDone must follow.
    let session_done = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Some(ServerEvent::SessionDone { session_id }) => return session_id,
                Some(_) => continue,
                None => panic!("event channel closed before SessionDone"),
            }
        }
    })
    .await
    .expect("timed out waiting for SessionDone");
    assert!(!session_done.as_str().is_empty());

    server.stop();
}
