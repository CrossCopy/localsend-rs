//! The service barrier must coordinate the server's admission lock with the
//! controller-owned outgoing tasks, not infer quiescence from UI progress rows.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use localsend_gpui::service::{
    Command, Peer, ServerStatus, ServiceEvent, ServiceHandle, Settings, TransferOutcome,
    UpdateQuiescenceError,
};
use localsend_rs::server::{LocalSendServer, ServerEvent};
use localsend_rs::{build_file_metadata, DeviceInfo, DeviceType, LocalSendClient, Protocol};
use tempfile::TempDir;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

fn settings(save_dir: &Path) -> Settings {
    Settings {
        alias: "Update Barrier Test".to_string(),
        save_dir: save_dir.to_path_buf(),
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

fn sender() -> DeviceInfo {
    DeviceInfo {
        alias: "Barrier Sender".to_string(),
        version: "2.1".to_string(),
        device_model: Some("test".to_string()),
        device_type: Some(DeviceType::Desktop),
        fingerprint: "barrier-sender-fingerprint".to_string(),
        port: 53317,
        protocol: Protocol::Http,
        download: false,
        ip: None,
    }
}

fn target(port: u16) -> DeviceInfo {
    DeviceInfo {
        alias: "Update Barrier Test".to_string(),
        version: "2.1".to_string(),
        device_model: None,
        device_type: Some(DeviceType::Desktop),
        fingerprint: "receiver-fingerprint".to_string(),
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
        .expect("the service stopped unexpectedly")
}

async fn wait_listening(events: &smol::channel::Receiver<ServiceEvent>) -> u16 {
    loop {
        match next_event(events).await {
            ServiceEvent::Status(ServerStatus::Listening { port, .. }) => return port,
            ServiceEvent::Status(ServerStatus::Failed { message }) => {
                panic!("service failed to listen: {message}")
            }
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_receive_reports_busy_then_can_drain_before_prepare_succeeds() {
    let temp = TempDir::new().unwrap();
    let save_dir = temp.path().join("received");
    tokio::fs::create_dir_all(&save_dir).await.unwrap();
    let handle = ServiceHandle::spawn(&tokio::runtime::Handle::current(), settings(&save_dir));
    let events = handle.events().clone();
    let port = wait_listening(&events).await;

    let input = temp.path().join("pending.bin");
    tokio::fs::write(&input, b"pending receive").await.unwrap();
    let metadata = build_file_metadata(&input).await.unwrap();
    let file_id = metadata.id.clone();
    let client = LocalSendClient::new(sender());
    let receiver = target(port);
    let preparing = tokio::spawn(async move {
        client
            .prepare_upload(&receiver, HashMap::from([(file_id, metadata)]), None)
            .await
    });
    let offer = loop {
        if let ServiceEvent::Offer(offer) = next_event(&events).await {
            break offer;
        }
    };

    assert_eq!(
        handle.prepare_update_quiescence().await,
        Err(UpdateQuiescenceError::Busy)
    );
    assert_eq!(
        handle.finalize_update_stop().await,
        Err(UpdateQuiescenceError::NotConfirmed),
        "a busy drain check must never authorize listener shutdown"
    );
    assert!(handle.command(Command::Accept {
        id: offer.id,
        files: None,
    }));
    let prepared = preparing.await.unwrap().unwrap();
    let file_id = prepared.files.keys().next().unwrap();
    LocalSendClient::new(sender())
        .upload_file(
            &target(port),
            &prepared.session_id,
            file_id,
            &prepared.files[file_id],
            &input,
            None,
        )
        .await
        .unwrap();

    loop {
        if let ServiceEvent::ReceiveFinished { outcome, .. } = next_event(&events).await {
            assert_eq!(outcome, TransferOutcome::Completed);
            break;
        }
    }
    assert_eq!(handle.prepare_update_quiescence().await, Ok(()));
    handle.cancel_update_quiescence().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn update_intent_is_ordered_before_a_queued_restart() {
    let temp = TempDir::new().unwrap();
    let save_dir = temp.path().join("received");
    tokio::fs::create_dir_all(&save_dir).await.unwrap();
    let original_settings = settings(&save_dir);
    let handle = ServiceHandle::spawn(
        &tokio::runtime::Handle::current(),
        original_settings.clone(),
    );
    let events = handle.events().clone();
    let port = wait_listening(&events).await;

    let input = temp.path().join("survives-restart-race.bin");
    tokio::fs::write(&input, b"must not be interrupted")
        .await
        .unwrap();
    let metadata = build_file_metadata(&input).await.unwrap();
    let file_id = metadata.id.clone();
    let client = LocalSendClient::new(sender());
    let receiver = target(port);
    let preparing = tokio::spawn(async move {
        client
            .prepare_upload(&receiver, HashMap::from([(file_id, metadata)]), None)
            .await
    });
    let offer = loop {
        if let ServiceEvent::Offer(offer) = next_event(&events).await {
            break offer;
        }
    };

    // Calling this method records the update intent synchronously. The later
    // restart must not overtake it through a higher-priority command channel.
    let quiescence = handle.prepare_update_quiescence();
    assert!(handle.command(Command::Restart(Box::new(Settings {
        alias: "racing-restart".to_string(),
        ..original_settings
    }))));
    assert_eq!(
        quiescence.await,
        Err(UpdateQuiescenceError::Busy),
        "the pre-existing offer must be observed before any restart can stop its server"
    );
    assert!(handle.command(Command::Accept {
        id: offer.id,
        files: None,
    }));

    let prepared = preparing.await.unwrap().unwrap();
    let file_id = prepared.files.keys().next().unwrap();
    LocalSendClient::new(sender())
        .upload_file(
            &target(port),
            &prepared.session_id,
            file_id,
            &prepared.files[file_id],
            &input,
            None,
        )
        .await
        .unwrap();
    loop {
        if let ServiceEvent::ReceiveFinished { outcome, .. } = next_event(&events).await {
            assert_eq!(outcome, TransferOutcome::Completed);
            break;
        }
    }
    assert_eq!(handle.prepare_update_quiescence().await, Ok(()));
    handle.cancel_update_quiescence().await.unwrap();
}

#[tokio::test]
async fn update_intent_freezes_a_restart_queued_before_prepare_is_processed() {
    let temp = TempDir::new().unwrap();
    let save_dir = temp.path().join("received");
    tokio::fs::create_dir_all(&save_dir).await.unwrap();
    let original_settings = settings(&save_dir);
    let handle = ServiceHandle::spawn(
        &tokio::runtime::Handle::current(),
        original_settings.clone(),
    );
    let events = handle.events().clone();
    let port = wait_listening(&events).await;

    let input = temp.path().join("queued-restart.bin");
    tokio::fs::write(&input, b"keep the admitted transfer")
        .await
        .unwrap();
    let metadata = build_file_metadata(&input).await.unwrap();
    let file_id = metadata.id.clone();
    let client = LocalSendClient::new(sender());
    let receiver = target(port);
    let preparing = tokio::spawn(async move {
        client
            .prepare_upload(&receiver, HashMap::from([(file_id, metadata)]), None)
            .await
    });
    let offer = loop {
        if let ServiceEvent::Offer(offer) = next_event(&events).await {
            break offer;
        }
    };

    // The test uses a current-thread runtime so neither controller message can
    // be processed between these synchronous enqueue operations.
    assert!(handle.command(Command::Restart(Box::new(Settings {
        alias: "racing-restart".to_string(),
        ..original_settings
    }))));
    let quiescence = handle.prepare_update_quiescence();
    assert_eq!(
        quiescence.await,
        Err(UpdateQuiescenceError::Busy),
        "the restart must see update intent before it can stop the occupied listener"
    );
    assert!(handle.command(Command::Accept {
        id: offer.id,
        files: None,
    }));

    let prepared = preparing.await.unwrap().unwrap();
    let file_id = prepared.files.keys().next().unwrap();
    LocalSendClient::new(sender())
        .upload_file(
            &target(port),
            &prepared.session_id,
            file_id,
            &prepared.files[file_id],
            &input,
            None,
        )
        .await
        .unwrap();
    loop {
        if let ServiceEvent::ReceiveFinished { outcome, .. } = next_event(&events).await {
            assert_eq!(outcome, TransferOutcome::Completed);
            break;
        }
    }
    assert_eq!(handle.prepare_update_quiescence().await, Ok(()));
    handle.cancel_update_quiescence().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_outgoing_send_reports_busy_and_finishes_without_cancellation() {
    let temp = TempDir::new().unwrap();
    let remote_dir = temp.path().join("remote");
    tokio::fs::create_dir_all(&remote_dir).await.unwrap();
    let (mut remote, mut remote_events) = LocalSendServer::builder()
        .port(0)
        .save_dir(&remote_dir)
        .protocol(Protocol::Http)
        .accept_timeout(WAIT)
        .build()
        .await
        .unwrap();
    let source = temp.path().join("outgoing.bin");
    let bytes = vec![41_u8; 256 * 1024];
    tokio::fs::write(&source, &bytes).await.unwrap();

    let local_dir = temp.path().join("local");
    tokio::fs::create_dir_all(&local_dir).await.unwrap();
    let handle = ServiceHandle::spawn(&tokio::runtime::Handle::current(), settings(&local_dir));
    let events = handle.events().clone();
    wait_listening(&events).await;
    let remote_device = DeviceInfo {
        ip: Some("127.0.0.1".to_string()),
        port: remote.port(),
        ..remote.device().clone()
    };
    let peer = Peer::from_device(&remote_device).unwrap();

    assert!(handle.command(Command::Send {
        peer,
        files: vec![source.clone()],
        pin: None,
    }));
    let pending = loop {
        if let Some(ServerEvent::TransferRequest(request)) = remote_events.recv().await {
            break request;
        }
    };
    assert_eq!(
        handle.prepare_update_quiescence().await,
        Err(UpdateQuiescenceError::Busy),
        "a send waiting for receiver consent is active before progress exists"
    );
    pending.accept();

    loop {
        if let ServiceEvent::SendFinished { outcome, .. } = next_event(&events).await {
            assert_eq!(outcome, TransferOutcome::Completed);
            break;
        }
    }
    assert_eq!(handle.prepare_update_quiescence().await, Ok(()));
    handle.cancel_update_quiescence().await.unwrap();
    assert_eq!(
        tokio::fs::read(remote_dir.join("outgoing.bin"))
            .await
            .unwrap(),
        bytes
    );
    remote.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_service_blocks_new_receives_until_cancel_then_can_restart() {
    let temp = TempDir::new().unwrap();
    let save_dir = temp.path().join("received");
    tokio::fs::create_dir_all(&save_dir).await.unwrap();
    let remote_dir = temp.path().join("remote");
    tokio::fs::create_dir_all(&remote_dir).await.unwrap();
    let (mut remote, mut remote_events) = LocalSendServer::builder()
        .port(0)
        .save_dir(&remote_dir)
        .protocol(Protocol::Http)
        .build()
        .await
        .unwrap();
    let outgoing = temp.path().join("must-not-send.bin");
    tokio::fs::write(&outgoing, b"paused").await.unwrap();
    let original_settings = settings(&save_dir);
    let handle = ServiceHandle::spawn(
        &tokio::runtime::Handle::current(),
        original_settings.clone(),
    );
    let events = handle.events().clone();
    let port = wait_listening(&events).await;

    assert_eq!(handle.prepare_update_quiescence().await, Ok(()));
    assert_eq!(
        raw_prepare(port).await.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    let remote_peer = Peer::from_device(&DeviceInfo {
        ip: Some("127.0.0.1".to_string()),
        port: remote.port(),
        ..remote.device().clone()
    })
    .unwrap();
    assert!(handle.command(Command::Send {
        peer: remote_peer,
        files: vec![outgoing],
        pin: None,
    }));
    loop {
        if let ServiceEvent::Notice { message, .. } = next_event(&events).await {
            if message.contains("paused") {
                break;
            }
        }
    }
    assert!(
        remote_events.try_recv().is_err(),
        "a send command handled during quiescence must not start a remote prepare"
    );

    handle.cancel_update_quiescence().await.unwrap();
    let resumed_prepare = tokio::spawn(raw_prepare(port));
    let offer = loop {
        if let ServiceEvent::Offer(offer) = next_event(&events).await {
            break offer;
        }
    };
    assert!(handle.command(Command::Decline { id: offer.id }));
    assert_eq!(
        resumed_prepare.await.unwrap().status(),
        reqwest::StatusCode::FORBIDDEN,
        "the resumed request must reach normal consent handling"
    );

    assert!(handle.command(Command::Restart(Box::new(original_settings))));
    let mut saw_stopped = false;
    let mut restarted_port = None;
    while restarted_port.is_none() {
        match next_event(&events).await {
            ServiceEvent::Status(ServerStatus::Stopped) => saw_stopped = true,
            ServiceEvent::Status(ServerStatus::Listening { port, .. }) => {
                restarted_port = Some(port)
            }
            _ => {}
        }
    }
    assert!(
        saw_stopped,
        "the failed-update recovery can use normal restart"
    );
    assert!(restarted_port.unwrap() > 0);
    remote.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalize_requires_quiescence_releases_the_listener_and_cancel_restarts() {
    let temp = TempDir::new().unwrap();
    let save_dir = temp.path().join("received");
    tokio::fs::create_dir_all(&save_dir).await.unwrap();
    let original_settings = settings(&save_dir);
    let handle = ServiceHandle::spawn(
        &tokio::runtime::Handle::current(),
        original_settings.clone(),
    );
    let events = handle.events().clone();
    let original_port = wait_listening(&events).await;

    assert_eq!(
        handle.finalize_update_stop().await,
        Err(UpdateQuiescenceError::NotConfirmed),
        "stopping the listener before a successful prepare acknowledgement is unsafe"
    );
    assert_eq!(handle.prepare_update_quiescence().await, Ok(()));

    assert!(handle.command(Command::Restart(Box::new(Settings {
        alias: "must-not-restart".to_string(),
        ..original_settings.clone()
    }))));
    loop {
        if let ServiceEvent::Notice { message, .. } = next_event(&events).await {
            if message.contains("Cancel the update attempt") {
                break;
            }
        }
    }

    handle.finalize_update_stop().await.unwrap();
    loop {
        if matches!(
            next_event(&events).await,
            ServiceEvent::Status(ServerStatus::Stopped)
        ) {
            break;
        }
    }
    let reusable_port = std::net::TcpListener::bind(("0.0.0.0", original_port))
        .expect("the finalize acknowledgement means the old listener released its port");
    assert!(handle.command(Command::Restart(Box::new(Settings {
        alias: "must-not-restart-while-stopped".to_string(),
        ..original_settings.clone()
    }))));
    assert!(handle.command(Command::Send {
        peer: Peer {
            node_id: "unused".to_string(),
            alias: "unused".to_string(),
            model: String::new(),
            device_kind: localsend_gpui::service::PeerKind::Computer,
            ip: "127.0.0.1".to_string(),
            port: original_port,
            protocol: Protocol::Http,
            last_seen: std::time::SystemTime::now(),
        },
        files: vec![temp.path().join("does-not-need-to-exist.bin")],
        pin: None,
    }));
    let mut saw_stopped_send = false;
    let mut saw_stopped_restart = false;
    while !saw_stopped_send || !saw_stopped_restart {
        if let ServiceEvent::Notice { message, .. } = next_event(&events).await {
            saw_stopped_send |= message.contains("paused");
            saw_stopped_restart |= message.contains("Cancel the update attempt");
        }
    }
    assert!(
        reusable_port.local_addr().is_ok(),
        "normal commands cannot rebind the stopped listener"
    );
    drop(reusable_port);

    handle.cancel_update_quiescence().await.unwrap();
    let resumed_port = wait_listening(&events).await;
    assert!(resumed_port > 0);
    let info = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!(
            "http://127.0.0.1:{resumed_port}/api/localsend/v2/info"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(info.status(), reqwest::StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoning_finalize_before_ack_restarts_the_service() {
    let temp = TempDir::new().unwrap();
    let save_dir = temp.path().join("received");
    tokio::fs::create_dir_all(&save_dir).await.unwrap();
    let handle = ServiceHandle::spawn(&tokio::runtime::Handle::current(), settings(&save_dir));
    let events = handle.events().clone();
    let _old_port = wait_listening(&events).await;
    assert_eq!(handle.prepare_update_quiescence().await, Ok(()));

    let finalize = handle.finalize_update_stop();
    drop(finalize);

    let resumed_port = wait_listening(&events).await;
    assert!(resumed_port > 0);
    let info = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!(
            "http://127.0.0.1:{resumed_port}/api/localsend/v2/info"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(info.status(), reqwest::StatusCode::OK);
}

async fn raw_prepare(port: u16) -> reqwest::Response {
    localsend_rs::crypto::ensure_crypto_provider();
    reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/api/localsend/v2/prepare-upload"
        ))
        .json(&serde_json::json!({
            "info": {
                "alias": "raw sender",
                "version": "2.1",
                "deviceType": "desktop",
                "fingerprint": "raw-fingerprint",
                "port": 53317,
                "protocol": "http",
                "download": false
            },
            "files": {
                "raw-file": {
                    "id": "raw-file",
                    "fileName": "raw.bin",
                    "size": 1,
                    "fileType": "application/octet-stream"
                }
            }
        }))
        .send()
        .await
        .unwrap()
}
