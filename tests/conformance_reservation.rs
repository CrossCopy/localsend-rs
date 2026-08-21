//! What happens to the single session slot while nobody has answered yet.
//!
//! A LocalSend receiver holds one session at a time and reserves it *before*
//! asking, which is the official receiver's ordering too — it answers 409
//! "Blocked by another session" to anybody who knocks while a decision is
//! pending. That is correct, and it makes the release path load-bearing: every
//! way a pending offer can end has to give the slot back, or the device is deaf
//! to the whole network until the accept timeout expires.
//!
//! Three ways it can end are covered here. A fourth — somebody answering — is
//! `interop_accept.rs`.

mod common;

use std::time::Duration;

use localsend_rs::Protocol;
use localsend_rs::server::{LocalSendServer, ServerEvent};
use serde_json::{Value, json};

fn offer(name: &str) -> Value {
    json!({
        "info": { "alias": "raw", "version": "2.1", "deviceType": "headless",
                  "fingerprint": "fp", "port": 53317, "protocol": "http", "download": false },
        "files": { "f1": { "id": "f1", "fileName": name, "size": 4, "fileType": "application/octet-stream" } }
    })
}

/// A receiver that asks and is never answered, so every offer stays pending
/// until something else ends it.
async fn silent_receiver() -> (LocalSendServer, u16, tempfile::TempDir) {
    let save = tempfile::tempdir().unwrap();
    let (server, events) = LocalSendServer::builder()
        .alias("R")
        .port(0)
        .save_dir(save.path())
        .protocol(Protocol::Http)
        .accept_timeout(Duration::from_secs(120))
        .build()
        .await
        .unwrap();
    let port = server.port();
    // Hold every request open. Dropping the `PendingRequest` would answer it.
    tokio::spawn(async move {
        let mut events = events;
        let mut held = Vec::new();
        while let Some(event) = events.recv().await {
            if let ServerEvent::TransferRequest(request) = event {
                held.push(request);
            }
        }
    });
    common::wait_for_http_info(port).await;
    // Returned rather than dropped: the save directory has to outlive the
    // server, and nothing here writes to it anyway.
    (server, port, save)
}

async fn prepare(port: u16, name: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/api/localsend/v2/prepare-upload"
        ))
        .json(&offer(name))
        .send()
        .await
        .unwrap()
}

/// The sender walks away mid-question. Before the reservation guard existed the
/// placeholder stayed until the sweep — up to `session_timeout`, five minutes by
/// default — and every other peer got 409 for the duration.
#[tokio::test(flavor = "multi_thread")]
async fn a_sender_that_disconnects_while_waiting_does_not_hold_the_slot() {
    let (mut server, port, _save) = silent_receiver().await;

    // A one-second client timeout is the disconnect: the request is dropped
    // client-side while the receiver is still parked on the decision.
    let abandoned = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap()
        .post(format!(
            "http://127.0.0.1:{port}/api/localsend/v2/prepare-upload"
        ))
        .json(&offer("gone.bin"))
        .send()
        .await;
    assert!(
        abandoned.is_err(),
        "the abandoned request should not answer"
    );

    // The guard's release runs in a spawned task, so give the runtime a beat.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let next = tokio::time::timeout(Duration::from_secs(2), prepare(port, "next.bin")).await;
    // The next sender gets *asked* rather than refused. It never returns,
    // because nobody answers — but a timeout here means the slot was free,
    // where a 409 would mean it was not.
    assert!(
        next.is_err(),
        "the abandoned reservation was still holding the slot: {next:?}"
    );
    server.stop().await;
}

/// The sender says so explicitly. It has never been told a session id — the
/// reservation's id is internal until the offer is accepted — so the official
/// receiver does not require one in the waiting state, and neither does this.
#[tokio::test(flavor = "multi_thread")]
async fn a_sender_can_cancel_before_it_has_a_session_id() {
    let (mut server, port, _save) = silent_receiver().await;

    let pending = tokio::spawn(prepare(port, "changed-my-mind.bin"));
    tokio::time::sleep(Duration::from_millis(150)).await;

    let cancelled = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/api/localsend/v2/cancel"))
        .send()
        .await
        .unwrap();
    assert_eq!(cancelled.status(), 200);

    // The parked handler wakes and unwinds rather than sitting until the accept
    // timeout: the sender gets its 403 now.
    let answered = tokio::time::timeout(Duration::from_secs(3), pending)
        .await
        .expect("the withdrawn offer should have been answered")
        .unwrap();
    assert_eq!(answered.status(), 403);

    // And the slot is genuinely free.
    let next = tokio::time::timeout(Duration::from_secs(2), prepare(port, "next.bin")).await;
    assert!(next.is_err(), "the slot was not released: {next:?}");
    server.stop().await;
}

/// A cancel that names no session and has nothing to withdraw is a malformed
/// request, not a silent success — otherwise a client with a bug looks like it
/// is working.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_with_nothing_to_cancel_is_refused() {
    let save = tempfile::tempdir().unwrap();
    let (mut server, _events) = LocalSendServer::builder()
        .alias("R")
        .port(0)
        .save_dir(save.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .unwrap();
    let port = server.port();
    common::wait_for_http_info(port).await;

    let answer = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/api/localsend/v2/cancel"))
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), 400);
    server.stop().await;
}
