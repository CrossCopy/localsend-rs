//! End-to-end tests for the network service — the half a screenshot cannot check.
//!
//! These drive the real controller task against real peers: `LocalSendClient`
//! sending to our receiver (offer → accept → bytes on disk) and our controller
//! sending to a standalone `LocalSendServer` (progress → completion → bytes on the
//! far side). No GPUI, no window, no mocks of the HTTP or TLS layers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use localsend_gpui::service::{
    Command, Peer, ServerStatus, ServiceEvent, ServiceHandle, Settings, TransferOutcome,
};
use localsend_rs::{
    build_file_metadata, DeviceInfo, DeviceType, LocalSendClient, LocalSendServer, Protocol,
    PROTOCOL_VERSION,
};
use tempfile::TempDir;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(20);

fn settings(save_dir: &Path) -> Settings {
    Settings {
        alias: "Test Receiver".to_string(),
        save_dir: save_dir.to_path_buf(),
        // 0 asks the OS for a free port, so two test binaries never collide.
        port: 0,
        pin: None,
        auto_accept: false,
        auto_accept_favorites: false,
        favorites: Vec::new(),
        https: false,
        glass_enabled: true,
        surface_opacity: localsend_gpui::service::DEFAULT_SURFACE_OPACITY,
    }
}

/// A sender identity with no `ip`, exactly what this app's own client sends when
/// it cannot work out a local address.
fn sender_device() -> DeviceInfo {
    DeviceInfo {
        alias: "Test Sender".to_string(),
        version: PROTOCOL_VERSION.to_string(),
        device_model: Some("test".to_string()),
        device_type: Some(DeviceType::Desktop),
        fingerprint: "sender-fingerprint-0123456789".to_string(),
        port: 53317,
        protocol: Protocol::Http,
        download: false,
        ip: None,
    }
}

fn target(port: u16, fingerprint: &str) -> DeviceInfo {
    DeviceInfo {
        alias: "Test Receiver".to_string(),
        version: PROTOCOL_VERSION.to_string(),
        device_model: None,
        device_type: Some(DeviceType::Desktop),
        fingerprint: fingerprint.to_string(),
        port,
        protocol: Protocol::Http,
        download: false,
        ip: Some("127.0.0.1".to_string()),
    }
}

async fn next_event(events: &smol::channel::Receiver<ServiceEvent>) -> ServiceEvent {
    timeout(WAIT, events.recv())
        .await
        .expect("timed out waiting for a service event")
        .expect("the service hung up")
}

/// The port the controller actually bound (settings said 0).
///
/// Takes the caller's receiver: `smol::channel` hands each message to exactly one
/// receiver clone, so a second clone would silently steal the events.
async fn wait_listening(events: &smol::channel::Receiver<ServiceEvent>) -> (u16, String) {
    loop {
        match next_event(events).await {
            ServiceEvent::Status(ServerStatus::Listening {
                port, fingerprint, ..
            }) => return (port, fingerprint),
            ServiceEvent::Status(ServerStatus::Failed { message }) => {
                panic!("the receiver failed to start: {message}")
            }
            _ => continue,
        }
    }
}

fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|index| (index % 251) as u8).collect()
}

/// Running the official app next to this one is normal, and both want 53317.
/// Taking an OS-assigned port keeps us sendable and receivable instead of
/// leaving a dead window, and the notice has to say which port we moved to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_taken_port_falls_back_to_a_free_one_instead_of_failing() {
    let tmp = TempDir::new().unwrap();
    let save_dir = tmp.path().join("received");
    std::fs::create_dir_all(&save_dir).unwrap();

    // Stand in for the official app: hold a port, then ask the receiver for it.
    let squatter = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let taken = squatter.local_addr().unwrap().port();

    let mut settings = settings(&save_dir);
    settings.port = taken;
    let handle = ServiceHandle::spawn(&tokio::runtime::Handle::current(), settings);
    let events = handle.events().clone();

    let mut stated_port = None;
    let mut warning = None;
    while stated_port.is_none() || warning.is_none() {
        match next_event(&events).await {
            ServiceEvent::Status(ServerStatus::Failed { message }) => {
                panic!("a taken port must not be fatal: {message}")
            }
            ServiceEvent::Status(ServerStatus::Listening { port, .. }) => stated_port = Some(port),
            ServiceEvent::Notice { level, message } if message.contains(&taken.to_string()) => {
                assert_eq!(level, localsend_gpui::service::NoticeLevel::Warning);
                warning = Some(message);
            }
            _ => {}
        }
    }

    let port = stated_port.unwrap();
    assert_ne!(
        port, taken,
        "the receiver must not claim a port it could not bind"
    );
    assert!(port > 0, "the fallback port must be a real one");
    let text = warning.unwrap();
    assert!(
        text.contains(&port.to_string()),
        "the notice must name the port it moved to: {text}"
    );

    // And the fallback port actually answers, so it is not just a number in a label.
    let info = localsend_rs::LocalSendClient::new(sender_device())
        .register(&target(port, "unused"))
        .await;
    assert!(
        info.is_ok(),
        "the receiver must answer on the fallback port: {info:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_favorite_is_accepted_automatically_and_a_stranger_is_not() {
    // Quick save for favorites is the narrow form of auto-accept: a device the
    // user starred is taken straight away, a device they did not star still gets
    // a prompt. Both halves matter — a test that only checked the favorite would
    // pass with plain "accept everything" underneath.
    let tmp = TempDir::new().unwrap();
    let save_dir = tmp.path().join("received");
    std::fs::create_dir_all(&save_dir).unwrap();
    let source = tmp.path().join("from-a-friend.bin");
    let bytes = payload(8 * 1024);
    std::fs::write(&source, &bytes).unwrap();

    let mut favorite_settings = settings(&save_dir);
    favorite_settings.auto_accept_favorites = true;
    // The sender identity below has this fingerprint; star it.
    favorite_settings.favorites = vec![sender_device().fingerprint.clone()];

    let handle = ServiceHandle::spawn(&tokio::runtime::Handle::current(), favorite_settings);
    let events = handle.events().clone();
    let (port, fingerprint) = wait_listening(&events).await;

    let sender = {
        let client = LocalSendClient::new(sender_device());
        let target = target(port, &fingerprint);
        let source = source.clone();
        tokio::spawn(async move {
            client.register(&target).await.unwrap();
            let metadata = build_file_metadata(&source).await.unwrap();
            let mut files = HashMap::new();
            files.insert(metadata.id.clone(), metadata);
            let prepared = client.prepare_upload(&target, files, None).await.unwrap();
            assert!(
                !prepared.session_id.as_str().is_empty(),
                "an auto-accepted offer must carry a session id without a prompt"
            );
            for (file_id, token) in &prepared.files {
                client
                    .upload_file(&target, &prepared.session_id, file_id, token, &source, None)
                    .await
                    .unwrap();
            }
            prepared.session_id
        })
    };

    let mut auto_accepted = false;
    let mut prompted = false;
    let mut received_path: Option<PathBuf> = None;
    let deadline = tokio::time::Instant::now() + WAIT;
    while received_path.is_none() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = timeout(remaining, next_event(&events))
            .await
            .expect("the favorite transfer must complete without an answer");
        match event {
            ServiceEvent::Offer(_) => prompted = true,
            ServiceEvent::OfferAcceptedAutomatically(offer) => {
                assert_eq!(offer.from.alias, "Test Sender");
                auto_accepted = true;
            }
            ServiceEvent::Received { path, .. } => received_path = Some(path),
            ServiceEvent::Notice { message, .. } => panic!("unexpected notice: {message}"),
            _ => {}
        }
    }

    sender.await.unwrap();
    assert!(
        auto_accepted,
        "a starred sender must be accepted without a prompt"
    );
    assert!(
        !prompted,
        "a starred sender must not produce an Offer prompt"
    );
    let path = received_path.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);

    // Now the other half: the same receiver, a sender it has never starred.
    let tmp2 = TempDir::new().unwrap();
    let save_dir2 = tmp2.path().join("received");
    std::fs::create_dir_all(&save_dir2).unwrap();
    let mut settings2 = settings(&save_dir2);
    settings2.auto_accept_favorites = true;
    settings2.favorites = vec!["somebody-else".to_string()];
    let handle2 = ServiceHandle::spawn(&tokio::runtime::Handle::current(), settings2);
    let events2 = handle2.events().clone();
    let (port2, fingerprint2) = wait_listening(&events2).await;

    let stranger = {
        let mut device = sender_device();
        device.fingerprint = "stranger-fingerprint-9876543210".to_string();
        let client = LocalSendClient::new(device);
        let target = target(port2, &fingerprint2);
        let source = source.clone();
        tokio::spawn(async move {
            client.register(&target).await.unwrap();
            let metadata = build_file_metadata(&source).await.unwrap();
            let mut files = HashMap::new();
            files.insert(metadata.id.clone(), metadata);
            client.prepare_upload(&target, files, None).await
        })
    };

    // A stranger must surface as a prompt, and declining it must leave nothing on disk.
    loop {
        match timeout(WAIT, next_event(&events2))
            .await
            .expect("a stranger must produce a prompt")
        {
            ServiceEvent::OfferAcceptedAutomatically(_) => {
                panic!("a device that is not a favorite must not be auto-accepted")
            }
            ServiceEvent::Offer(offer) => {
                assert_eq!(offer.from.alias, "Test Sender");
                handle2.command(Command::Decline { id: offer.id });
                break;
            }
            _ => {}
        }
    }
    let _ = stranger.await.unwrap();
    assert_eq!(
        std::fs::read_dir(&save_dir2).unwrap().count(),
        0,
        "a declined stranger must not leave a file behind"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receives_a_file_over_http_after_the_offer_is_accepted() {
    let tmp = TempDir::new().unwrap();
    let save_dir = tmp.path().join("received");
    std::fs::create_dir_all(&save_dir).unwrap();
    let source = tmp.path().join("hello.bin");
    let bytes = payload(64 * 1024);
    std::fs::write(&source, &bytes).unwrap();

    let handle = ServiceHandle::spawn(&tokio::runtime::Handle::current(), settings(&save_dir));
    let events = handle.events().clone();
    let (port, fingerprint) = wait_listening(&events).await;

    // The sender: register first (so the receiver's peer list learns about it),
    // then offer, wait for acceptance and upload one file.
    let sender = {
        let client = LocalSendClient::new(sender_device());
        let target = target(port, &fingerprint);
        let source = source.clone();
        tokio::spawn(async move {
            client.register(&target).await.unwrap();
            let metadata = build_file_metadata(&source).await.unwrap();
            let mut files = HashMap::new();
            files.insert(metadata.id.clone(), metadata);
            let prepared = client.prepare_upload(&target, files, None).await.unwrap();
            assert!(
                !prepared.session_id.as_str().is_empty(),
                "an accepted offer must carry a session id"
            );
            for (file_id, token) in &prepared.files {
                client
                    .upload_file(&target, &prepared.session_id, file_id, token, &source, None)
                    .await
                    .unwrap();
            }
            prepared.session_id
        })
    };

    // The receiver: see the peer, see the offer, accept it.
    let mut saw_peer = false;
    let mut saw_offer = false;
    let mut received_path: Option<PathBuf> = None;
    let mut finished = false;
    while !(saw_peer && saw_offer && received_path.is_some() && finished) {
        match next_event(&events).await {
            ServiceEvent::PeerFound(peer) if peer.alias == "Test Sender" => {
                saw_peer = true;
            }
            ServiceEvent::PeerFound(_) => {}
            ServiceEvent::Offer(offer) => {
                assert_eq!(offer.from.alias, "Test Sender");
                // `prepare-upload` carried no ip; the offer must survive that.
                assert_eq!(offer.from.ip, "unknown");
                assert_eq!(offer.files.len(), 1);
                assert_eq!(offer.files[0].name, "hello.bin");
                assert_eq!(offer.files[0].size, bytes.len() as u64);
                assert!(handle.command(Command::Accept {
                    id: offer.id,
                    files: None
                }));
                saw_offer = true;
            }
            ServiceEvent::OfferClosed { accepted, .. } => assert!(accepted),
            ServiceEvent::Received { path, size, .. } => {
                assert_eq!(size, bytes.len() as u64);
                received_path = Some(path);
            }
            ServiceEvent::ReceiveFinished { outcome, .. } => {
                assert_eq!(outcome, TransferOutcome::Completed);
                finished = true;
            }
            ServiceEvent::Notice { message, .. } => panic!("unexpected notice: {message}"),
            _ => {}
        }
    }

    sender.await.unwrap();

    let path = received_path.expect("a file was reported received");
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(
        path.starts_with(&save_dir),
        "saved outside the chosen folder"
    );
    let _ = handle;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sends_a_file_to_a_real_receiver_and_reports_completion() {
    let tmp = TempDir::new().unwrap();
    let receiver_dir = tmp.path().join("receiver");
    std::fs::create_dir_all(&receiver_dir).unwrap();

    // A standalone receiver — the same library the app wraps, with no controller
    // in between, so this asserts the wire rather than our own bookkeeping.
    let (mut server, mut server_events) = LocalSendServer::builder()
        .alias("Standalone Receiver")
        .port(0)
        .save_dir(&receiver_dir)
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .unwrap();
    let wire = DeviceInfo {
        ip: Some("127.0.0.1".to_string()),
        port: server.port(),
        protocol: Protocol::Http,
        ..server.device().clone()
    };
    let peer = Peer::from_device(&wire).expect("a local receiver is a sendable peer");

    let our_dir = tmp.path().join("ours");
    std::fs::create_dir_all(&our_dir).unwrap();
    let handle = ServiceHandle::spawn(&tokio::runtime::Handle::current(), settings(&our_dir));
    let events = handle.events().clone();
    let _ = wait_listening(&events).await;

    let source = tmp.path().join("payload.bin");
    let bytes = payload(200_000);
    std::fs::write(&source, &bytes).unwrap();

    assert!(handle.command(Command::Send {
        peer,
        files: vec![source.clone()],
        pin: None,
    }));

    let mut started = false;
    let mut last_progress = 0u64;
    let mut outcome = None;
    while outcome.is_none() {
        match next_event(&events).await {
            ServiceEvent::SendStarted {
                peer_alias,
                files,
                total_bytes,
                ..
            } => {
                assert_eq!(peer_alias, "Standalone Receiver");
                assert_eq!(files.len(), 1);
                assert_eq!(total_bytes, bytes.len() as u64);
                started = true;
            }
            ServiceEvent::SendProgress { session_done, .. } => {
                last_progress = last_progress.max(session_done);
            }
            ServiceEvent::SendFinished {
                outcome: result, ..
            } => outcome = Some(result),
            ServiceEvent::Notice { message, .. } => panic!("unexpected notice: {message}"),
            _ => {}
        }
    }
    assert!(started, "the send announced itself before any bytes moved");
    assert_eq!(outcome.unwrap(), TransferOutcome::Completed);
    assert_eq!(
        last_progress,
        bytes.len() as u64,
        "the last progress report must be the whole file"
    );

    // The receiver's own event stream is the independent witness.
    let landed = loop {
        let event = timeout(WAIT, server_events.recv())
            .await
            .expect("the standalone receiver went quiet")
            .expect("the standalone receiver hung up");
        if let localsend_rs::server::ServerEvent::FileReceived { path, size, .. } = event {
            assert_eq!(size, bytes.len() as u64);
            break path;
        }
    };
    // `landed` is the path the receiver announced.
    assert_eq!(std::fs::read(&landed).unwrap(), bytes);

    server.stop().await;
    let _ = handle;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declined_offer_never_writes_a_file() {
    let tmp = TempDir::new().unwrap();
    let save_dir = tmp.path().join("received");
    std::fs::create_dir_all(&save_dir).unwrap();
    let source = tmp.path().join("refused.bin");
    std::fs::write(&source, payload(4096)).unwrap();

    let handle = ServiceHandle::spawn(&tokio::runtime::Handle::current(), settings(&save_dir));
    let events = handle.events().clone();
    let (port, fingerprint) = wait_listening(&events).await;

    let sender = {
        let client = LocalSendClient::new(sender_device());
        let target = target(port, &fingerprint);
        let source = source.clone();
        tokio::spawn(async move {
            let metadata = build_file_metadata(&source).await.unwrap();
            let mut files = HashMap::new();
            files.insert(metadata.id.clone(), metadata);
            client.prepare_upload(&target, files, None).await
        })
    };

    loop {
        if let ServiceEvent::Offer(offer) = next_event(&events).await {
            assert!(handle.command(Command::Decline { id: offer.id }));
            break;
        }
    }

    let result = sender.await.unwrap();
    assert!(
        result.is_err(),
        "the sender must be told the offer was declined"
    );
    assert!(
        std::fs::read_dir(&save_dir).unwrap().next().is_none(),
        "a declined offer must leave the folder empty"
    );
    let _ = handle;
}
