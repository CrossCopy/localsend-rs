use super::{ANNOUNCEMENT_REGISTER_TIMEOUT, DiscoveryRun, MulticastConfig, MulticastDiscovery};
use crate::discovery::Discovery;
use crate::{DeviceInfo, Protocol};
use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use tokio::time::{Duration, timeout};

#[tokio::test]
async fn stop_and_wait_joins_pending_receive_and_response_work_across_clones() {
    let config = MulticastConfig::new(
        crate::DEFAULT_MULTICAST_ADDRESS.parse().unwrap(),
        crate::DEFAULT_MULTICAST_PORT,
        Some(BTreeSet::new()),
    )
    .unwrap();
    let discovery = MulticastDiscovery::new_with_device_and_config(
        crate::DeviceInfo::new("lifecycle test".into(), 0, crate::Protocol::Http),
        config,
    )
    .unwrap();
    let socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
    let socket_address = socket.local_addr().unwrap();
    let run = Arc::new(DiscoveryRun::new());
    *run.sockets.lock().unwrap() = vec![socket.clone()];
    *discovery.lifecycle.current.lock().unwrap() = Some(run.clone());

    let mut stop = run.stop_tx.subscribe();
    let receive_socket = socket.clone();
    drop(socket);
    assert!(run.spawn(async move {
        let mut buffer = [0; 1];
        tokio::select! {
            _ = stop.changed() => {}
            _ = receive_socket.recv_from(&mut buffer) => {}
        }
    }));

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let target_port = listener.local_addr().unwrap().port();
    let (request_accepted, accepted_request) = oneshot::channel();
    let (finish_response, pending_response) = oneshot::channel();
    let response_server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        request_accepted.send(()).unwrap();
        pending_response.await.unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
    });

    let mut target = crate::DeviceInfo::new("peer".into(), target_port, crate::Protocol::Http);
    target.ip = Some(Ipv4Addr::LOCALHOST.to_string());
    let local = crate::DeviceInfo::new("local".into(), 0, crate::Protocol::Http);
    let response_socket = run.sockets.lock().unwrap()[0].clone();
    let response_run = run.clone();
    assert!(run.spawn(async move {
        MulticastDiscovery::respond_to_announcement(
            &target,
            &local,
            Some((&response_socket, (Ipv4Addr::LOCALHOST, 53317).into())),
            &response_run,
            #[cfg(feature = "https")]
            None,
        )
        .await;
    }));
    timeout(Duration::from_secs(1), accepted_request)
        .await
        .expect("the responder should connect to the peer")
        .unwrap();

    let first_clone = discovery.clone();
    let second_clone = discovery.clone();
    let first_stop = tokio::spawn(async move {
        first_clone.stop_and_wait().await;
    });
    let second_stop = tokio::spawn(async move {
        second_clone.stop_and_wait().await;
    });

    timeout(Duration::from_secs(1), async {
        while run.running.load(std::sync::atomic::Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a stop caller should signal the shared run");
    tokio::task::yield_now().await;
    assert!(
        !first_stop.is_finished() && !second_stop.is_finished(),
        "stop_and_wait must not return while a response task is still active"
    );

    finish_response.send(()).unwrap();
    response_server.await.unwrap();
    timeout(Duration::from_secs(1), async {
        first_stop.await.unwrap();
        second_stop.await.unwrap();
    })
    .await
    .expect("concurrent stop callers should both observe the completed run");

    assert!(discovery.lifecycle.current.lock().unwrap().is_none());
    UdpSocket::bind(socket_address)
        .await
        .expect("the discovery socket should be released before stop_and_wait returns");
}

#[tokio::test]
async fn stop_and_wait_bounds_a_peer_that_never_answers_registration() {
    let config = MulticastConfig::new(
        crate::DEFAULT_MULTICAST_ADDRESS.parse().unwrap(),
        crate::DEFAULT_MULTICAST_PORT,
        Some(BTreeSet::new()),
    )
    .unwrap();
    let discovery = MulticastDiscovery::new_with_device_and_config(
        crate::DeviceInfo::new("lifecycle timeout test".into(), 0, crate::Protocol::Http),
        config,
    )
    .unwrap();
    let run = Arc::new(DiscoveryRun::new());
    *discovery.lifecycle.current.lock().unwrap() = Some(run.clone());
    let response_socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let target_port = listener.local_addr().unwrap().port();
    let (request_accepted, accepted_request) = oneshot::channel();
    let (close_peer, peer_close) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        request_accepted.send(()).unwrap();
        let _stream = stream;
        peer_close.await.unwrap();
    });

    let mut target =
        crate::DeviceInfo::new("silent peer".into(), target_port, crate::Protocol::Http);
    target.ip = Some(Ipv4Addr::LOCALHOST.to_string());
    let local = crate::DeviceInfo::new("local".into(), 0, crate::Protocol::Http);
    let response_run = run.clone();
    assert!(run.spawn(async move {
        MulticastDiscovery::respond_to_announcement(
            &target,
            &local,
            Some((&response_socket, (Ipv4Addr::LOCALHOST, 53317).into())),
            &response_run,
            #[cfg(feature = "https")]
            None,
        )
        .await;
    }));
    timeout(Duration::from_secs(1), accepted_request)
        .await
        .expect("the responder should connect to the peer")
        .unwrap();

    timeout(
        ANNOUNCEMENT_REGISTER_TIMEOUT + Duration::from_secs(1),
        discovery.stop_and_wait(),
    )
    .await
    .expect("a peer that never answers must not hang the awaited stop");

    close_peer.send(()).unwrap();
    peer.await.unwrap();
}

#[tokio::test]
async fn confirmation_finishing_during_shutdown_does_not_publish_the_peer() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let peer_port = listener.local_addr().unwrap().port();
    let reservation = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.unwrap();
    let discovery_port = reservation.local_addr().unwrap().port();
    let config = MulticastConfig {
        port: discovery_port,
        ..MulticastConfig::default()
    };
    let local = DeviceInfo {
        fingerprint: "local".into(),
        ..DeviceInfo::new("local".into(), discovery_port, Protocol::Http)
    };
    let mut discovery = MulticastDiscovery::new_with_device_and_config(local, config).unwrap();
    discovery.set_require_http_confirmation(true);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    discovery.on_discovered(move |device| {
        let _ = tx.send(device);
    });
    drop(reservation);
    discovery.start().await.unwrap();

    let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let response = serde_json::json!({
        "alias": "peer",
        "version": crate::protocol::PROTOCOL_VERSION,
        "fingerprint": "peer",
        "port": peer_port,
        "protocol": "http",
        "announce": false,
    });
    sender
        .send_to(
            &serde_json::to_vec(&response).unwrap(),
            (Ipv4Addr::LOCALHOST, discovery_port),
        )
        .await
        .unwrap();
    let (mut stream, _) = timeout(Duration::from_secs(1), listener.accept())
        .await
        .expect("the discovery run should start HTTP confirmation")
        .unwrap();
    timeout(Duration::from_secs(1), stream.read_exact(&mut [0; 1]))
        .await
        .expect("the HTTP confirmation request should reach the peer")
        .unwrap();

    discovery.stop();
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    discovery.stop_and_wait().await;
    assert_eq!(
        timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("the stopped discovery callback should close"),
        None,
        "an in-flight registration must not publish a peer after shutdown"
    );
}

#[tokio::test]
async fn synchronous_stop_cancels_pending_announcement_rounds() {
    let config = MulticastConfig::new(
        crate::DEFAULT_MULTICAST_ADDRESS.parse().unwrap(),
        crate::DEFAULT_MULTICAST_PORT,
        Some(BTreeSet::new()),
    )
    .unwrap();
    let discovery = MulticastDiscovery::new_with_device_and_config(
        crate::DeviceInfo::new("announcement stop test".into(), 0, crate::Protocol::Http),
        config,
    )
    .unwrap();
    let socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
    let socket_address = socket.local_addr().unwrap();
    let run = Arc::new(DiscoveryRun::new());
    *run.sockets.lock().unwrap() = vec![socket];
    *discovery.lifecycle.current.lock().unwrap() = Some(run.clone());

    let announcer = discovery.clone();
    let announcement = tokio::spawn(async move { announcer.announce_presence().await });
    timeout(Duration::from_secs(1), async {
        while *run.active_tasks.borrow() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the announcement should be registered as active work");

    let mut stopper = discovery.clone();
    stopper.stop();
    let result = timeout(Duration::from_millis(500), announcement)
        .await
        .expect("stop should interrupt the announcement delay")
        .unwrap();
    assert!(result.is_err());
    assert_eq!(*run.active_tasks.borrow(), 0);
    UdpSocket::bind(socket_address)
        .await
        .expect("synchronous stop should release its socket after active work exits");
}
