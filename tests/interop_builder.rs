mod common;

use localsend_rs::Protocol;
use localsend_rs::server::LocalSendServer;

#[tokio::test]
async fn builder_starts_on_ephemeral_port_and_reports_it() {
    let save = tempfile::tempdir().unwrap();
    let (server, _events) = LocalSendServer::builder()
        .alias("Builder Test")
        .port(0)
        .save_dir(save.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .expect("build");

    let port = server.port();
    assert_ne!(port, 0);
    common::wait_for_http_info(port).await;

    let url = format!("http://127.0.0.1:{port}/api/localsend/v2/info");
    let info: serde_json::Value = reqwest::get(&url).await.unwrap().json().await.unwrap();
    assert_eq!(info["alias"], "Builder Test");
    assert_eq!(info["version"], "2.1");
    assert_eq!(info["port"], serde_json::json!(port));
}

/// `stop()` must actually release `ServerState`, not merely request that the
/// tasks holding it wind down.
///
/// `ServerState` carries the save directory, the event sender, the pin gate and
/// any host-supplied protected upload gate, so "the listener has stopped" has to
/// mean the host has its gate back. It did not: `stop` aborted the serve task and
/// the sweep task without awaiting them, and never dropped its own strong
/// reference, so the state stayed alive for an unbounded time after `stop`
/// returned — measured with a drop probe, still alive ten seconds later in about
/// half of the runs, and released only when the runtime was torn down.
///
/// The field is private, so this asserts the observable consequence: every method
/// that needs the state reports "Server is not running" once it is gone. Before
/// the fix the state was still installed and `stop_web_share` returned `Ok`.
#[tokio::test]
async fn stop_releases_the_server_state() {
    let save = tempfile::tempdir().unwrap();
    let (mut server, _events) = LocalSendServer::builder()
        .alias("Stop Releases State")
        .port(0)
        .save_dir(save.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .expect("build");
    common::wait_for_http_info(server.port()).await;

    server.stop().await;

    let error = server
        .stop_web_share()
        .await
        .expect_err("a stopped server must no longer hold its ServerState");
    assert!(
        error.to_string().contains("not running"),
        "expected a not-running error once the state is released, got: {error}"
    );
}
