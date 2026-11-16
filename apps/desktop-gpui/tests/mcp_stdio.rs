//! The MCP server as an AI client actually meets it: a child process, newline
//! JSON-RPC on stdin/stdout, no shortcuts.

// A build without the feature has no `mcp` subcommand to test.
#![cfg(feature = "mcp")]
//!
//! These tests spawn the real `localsend-gpui mcp` binary and talk to it the way
//! an editor's MCP client does — `initialize`, `tools/list`, `tools/call` — then
//! check the wire with a real `LocalSendServer` / `LocalSendClient` on the other
//! side. Nothing here links the server in-process, so a broken subcommand, a
//! startup line written to stdout, or a bad schema would fail the test.

use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(30);

/// A spawned MCP server, spoken to over its stdin/stdout.
struct Mcp {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    stderr: Arc<Mutex<Vec<String>>>,
    next_id: i64,
    dir: TempDir,
}

impl Mcp {
    /// Start `localsend-gpui mcp` with an isolated inbox and state directory.
    async fn spawn(extra: &[&str]) -> Self {
        let dir = TempDir::new().unwrap();
        let inbox = dir.path().join("inbox");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::create_dir_all(&state).unwrap();

        let mut command = Command::new(env!("CARGO_BIN_EXE_localsend-gpui"));
        command
            .arg("mcp")
            .arg("--no-https")
            .arg("--port")
            .arg("0")
            .arg("--alias")
            .arg("Mcp Test")
            .arg("--dir")
            .arg(&inbox)
            .arg("--state-dir")
            .arg(&state)
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A stray LOCAL… env var must not move the state or the port under us.
            .env_remove("LOCALSEND_GPUI_THEME");

        let mut child = command.spawn().expect("the app binary runs");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        // Drain stderr or a chatty child fills the pipe and stops.
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                sink.lock().unwrap().push(line);
            }
        });

        let mut mcp = Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            stderr: log,
            next_id: 0,
            dir,
        };
        mcp.handshake().await;
        mcp
    }

    fn inbox(&self) -> std::path::PathBuf {
        self.dir.path().join("inbox")
    }

    fn journal(&self) -> std::path::PathBuf {
        self.dir.path().join("state").join("inbox.jsonl")
    }

    fn logs(&self) -> String {
        self.stderr.lock().unwrap().join("\n")
    }

    /// A message straight onto the wire, with no framing beyond the newline the
    /// spec uses for stdio.
    async fn send(&mut self, message: Value) {
        let mut line = serde_json::to_string(&message).unwrap();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    /// The next message whose `id` matches, skipping notifications.
    async fn response(&mut self, id: i64) -> Value {
        loop {
            let line = timeout(WAIT, self.lines.next_line())
                .await
                .unwrap_or_else(|_| panic!("no answer to request {id}; stderr:\n{}", self.logs()))
                .expect("the child's stdout is open")
                .unwrap_or_else(|| panic!("the child closed stdout; stderr:\n{}", self.logs()));
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&line)
                .unwrap_or_else(|error| panic!("stdout is protocol-only, got {line:?}: {error}"));
            if value.get("id") == Some(&json!(id)) {
                return value;
            }
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        self.response(id).await
    }

    async fn notify(&mut self, method: &str) {
        self.send(json!({ "jsonrpc": "2.0", "method": method }))
            .await;
    }

    /// The protocol handshake every client performs before anything else.
    async fn handshake(&mut self) {
        let response = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "localsend-tests", "version": "1" }
                }),
            )
            .await;
        let result = &response["result"];
        assert!(
            result["serverInfo"]["name"].is_string(),
            "initialize must name the server: {response}"
        );
        assert!(
            result["capabilities"]["tools"].is_object(),
            "tools are advertised: {response}"
        );
        assert!(
            result["capabilities"]["resources"].is_object(),
            "resources are advertised: {response}"
        );
        self.notify("notifications/initialized").await;
    }

    /// Call a tool and return its text. `Err` is a tool-level failure, which is
    /// how a model is told a transfer did not happen.
    async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<String, String> {
        let response = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await;
        if let Some(error) = response.get("error") {
            panic!("tools/call {name} failed at the protocol level: {error}");
        }
        let result = &response["result"];
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{name} returned no text: {result}"))
            .to_string();
        if result["isError"].as_bool().unwrap_or(false) {
            return Err(text);
        }
        Ok(text)
    }

    /// `server_status` as JSON — the port the node really bound.
    async fn status(&mut self) -> Value {
        let text = self.call_tool("server_status", json!({})).await.unwrap();
        serde_json::from_str(&text).expect("server_status is JSON")
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// A sender identity with no `ip`, exactly what this app's own client sends.
fn sender(fingerprint: &str) -> localsend_rs::DeviceInfo {
    localsend_rs::DeviceInfo {
        alias: "Test Sender".to_string(),
        version: localsend_rs::PROTOCOL_VERSION.to_string(),
        device_model: Some("test".to_string()),
        device_type: Some(localsend_rs::DeviceType::Desktop),
        fingerprint: fingerprint.to_string(),
        port: 53317,
        protocol: localsend_rs::Protocol::Http,
        download: false,
        ip: None,
    }
}

fn target(port: u16, fingerprint: &str) -> localsend_rs::DeviceInfo {
    localsend_rs::DeviceInfo {
        alias: "Mcp Test (MCP)".to_string(),
        version: localsend_rs::PROTOCOL_VERSION.to_string(),
        device_model: None,
        device_type: Some(localsend_rs::DeviceType::Desktop),
        fingerprint: fingerprint.to_string(),
        port,
        protocol: localsend_rs::Protocol::Http,
        download: false,
        ip: Some("127.0.0.1".to_string()),
    }
}

fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|index| (index % 251) as u8).collect()
}

async fn sha(path: &Path) -> String {
    localsend_rs::sha256_from_file(path)
        .await
        .expect("the file can be hashed")
}

/// Push one file into the MCP node's receiver with the real client, whether or
/// not the node is waiting for a person to answer the offer.
async fn push_file(
    mcp_port: u16,
    fingerprint: String,
    source: std::path::PathBuf,
) -> (String, u64) {
    use localsend_rs::{build_file_metadata, LocalSendClient};
    use std::collections::HashMap;

    let client = LocalSendClient::new(sender("test-sender-0123456789abcdef"));
    let target = target(mcp_port, &fingerprint);
    let meta = build_file_metadata(&source).await.unwrap();
    let mut files = HashMap::new();
    files.insert(meta.id.clone(), meta.clone());

    let prepared = client
        .prepare_upload(&target, files, None)
        .await
        .expect("the MCP node takes the offer");
    assert!(
        !prepared.session_id.as_str().is_empty(),
        "an accepted offer carries a session"
    );
    assert_eq!(
        prepared.files.len(),
        1,
        "one file was offered, one accepted"
    );
    let token = prepared.files.get(&meta.id).expect("the file was accepted");
    client
        .upload_file(
            &target,
            &prepared.session_id,
            &meta.id,
            token,
            &source,
            None,
        )
        .await
        .expect("the upload lands");
    (meta.id.to_string(), meta.size)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ai_client_can_list_the_tools_and_see_what_the_node_is() {
    let mut mcp = Mcp::spawn(&[]).await;

    let listed = mcp.request("tools/list", json!({})).await;
    let tools = listed["result"]["tools"].as_array().expect("an array");
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for expected in [
        "list_devices",
        "send_files",
        "list_received",
        "wait_for_received",
        "list_offers",
        "accept_offer",
        "decline_offer",
        "server_status",
    ] {
        assert!(
            names.contains(&expected),
            "a model cannot call a tool that is not listed; missing {expected} in {names:?}"
        );
    }
    let send = tools
        .iter()
        .find(|tool| tool["name"] == "send_files")
        .unwrap();
    assert!(
        send["inputSchema"]["properties"]["files"].is_object(),
        "send_files must describe its arguments: {send}"
    );
    assert!(
        send["description"].as_str().unwrap().len() > 20,
        "a tool with no description is a tool a model will not use"
    );

    let status = mcp.status().await;
    assert_eq!(status["alias"], "Mcp Test (MCP)");
    assert_eq!(status["protocol"], "HTTP");
    assert_eq!(status["listening"], true);
    assert!(status["port"].as_u64().unwrap() > 0);
    assert!(
        status["fingerprint"].as_str().unwrap().len() >= 32,
        "a node without a fingerprint is not addressable: {status}"
    );

    let resources = mcp.request("resources/list", json!({})).await;
    let uris: Vec<&str> = resources["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|resource| resource["uri"].as_str().unwrap())
        .collect();
    assert!(uris.contains(&"localsend://inbox"), "{uris:?}");
    assert!(uris.contains(&"localsend://devices"), "{uris:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_files_puts_the_bytes_on_another_machine() {
    use localsend_rs::{LocalSendServer, Protocol};

    let mut mcp = Mcp::spawn(&[]).await;

    // The other computer: a real receiver, auto-accepting, on its own port.
    let received_dir = TempDir::new().unwrap();
    let (peer, mut peer_events) = LocalSendServer::builder()
        .alias("Other Computer")
        .port(0)
        .save_dir(received_dir.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .expect("the peer receiver starts");
    let peer_port = peer.port();
    // Keeping `peer` alive keeps the listener up; dropping it stops the server.

    let outgoing = TempDir::new().unwrap();
    let source = outgoing.path().join("report.txt");
    let bytes = payload(64 * 1024);
    std::fs::write(&source, &bytes).unwrap();

    let report = mcp
        .call_tool(
            "send_files",
            json!({
                "device": format!("127.0.0.1:{peer_port}"),
                "files": [source.display().to_string()],
            }),
        )
        .await
        .expect("a reachable address is enough; no discovery needed");
    assert!(
        report.contains("report.txt"),
        "the report names the file: {report}"
    );

    // The bytes, on disk, at the far end.
    let landed = loop {
        match timeout(WAIT, peer_events.recv())
            .await
            .expect("the peer reports the transfer")
            .expect("the peer's event channel is open")
        {
            localsend_rs::server::ServerEvent::FileReceived { path, .. } => break path,
            _ => continue,
        }
    };
    assert_eq!(std::fs::read(&landed).unwrap(), bytes, "byte for byte");
    assert_eq!(sha(&landed).await, sha(&source).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_arrives_is_journalled_and_read_back_through_the_tools() {
    let mut mcp = Mcp::spawn(&[]).await;
    let status = mcp.status().await;
    let port = status["port"].as_u64().unwrap() as u16;
    let fingerprint = status["fingerprint"].as_str().unwrap().to_string();

    let incoming = TempDir::new().unwrap();
    let source = incoming.path().join("holiday.jpg");
    let bytes = payload(32 * 1024);
    std::fs::write(&source, &bytes).unwrap();

    // Auto-accept is the default, so this call completes without any other tool.
    let (_file_id, size) = push_file(port, fingerprint, source).await;
    assert!(size > 0);

    // The tool a model would call to see it.
    let text = mcp.call_tool("list_received", json!({})).await.unwrap();
    let page: Value = serde_json::from_str(&text).unwrap();
    let entry = &page["entries"][0];
    assert_eq!(entry["name"], "holiday.jpg");
    assert_eq!(entry["sender"], "Test Sender");
    assert_eq!(entry["size"], size);
    let path = std::path::PathBuf::from(entry["path"].as_str().unwrap());
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes,
        "the inbox file is the sent one"
    );

    // Durable: the journal on disk has the same entry, so the next session's
    // process can answer "what arrived?" too.
    let journal = std::fs::read_to_string(mcp.journal()).expect("the journal was written");
    let lines: Vec<&str> = journal.lines().filter(|line| !line.is_empty()).collect();
    assert_eq!(lines.len(), 1, "one arrival, one line: {journal}");
    let journalled: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(journalled["id"], entry["id"]);
    assert_eq!(journalled["name"], "holiday.jpg");
    assert!(mcp.inbox().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_for_received_returns_the_next_arrival_instead_of_polling() {
    let mut mcp = Mcp::spawn(&[]).await;
    let status = mcp.status().await;
    let port = status["port"].as_u64().unwrap() as u16;
    let fingerprint = status["fingerprint"].as_str().unwrap().to_string();
    let cursor = mcp
        .call_tool("list_received", json!({}))
        .await
        .unwrap()
        .contains("Nothing has been received");

    // Start the long poll first: this is the "notify me" path, and it has to
    // survive a file arriving while it is waiting.
    let waiting = mcp.call_tool(
        "wait_for_received",
        json!({ "cursor": 0, "timeout_seconds": 30 }),
    );
    let incoming = TempDir::new().unwrap();
    let source = incoming.path().join("note.txt");
    std::fs::write(&source, payload(1024)).unwrap();
    let push = push_file(port, fingerprint, source);
    let (text, _) = tokio::join!(waiting, push);

    assert!(cursor, "the node started with an empty inbox");
    let page: Value = serde_json::from_str(&text.unwrap()).unwrap();
    assert_eq!(page["entries"][0]["name"], "note.txt", "{page}");
    assert_eq!(page["cursor"], 1, "the cursor moves with the journal");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ask_mode_holds_the_transfer_until_a_model_answers() {
    let mut mcp = Mcp::spawn(&["--ask"]).await;
    let status = mcp.status().await;
    let port = status["port"].as_u64().unwrap() as u16;
    let fingerprint = status["fingerprint"].as_str().unwrap().to_string();
    assert_eq!(status["auto_accept"], false, "--ask must not auto-accept");

    let incoming = TempDir::new().unwrap();
    let source = incoming.path().join("ask-me.txt");
    let bytes = payload(2048);
    std::fs::write(&source, &bytes).unwrap();

    // The sender blocks in `prepare_upload` until somebody answers the offer, so
    // it has to run beside the tool calls rather than before them.
    let sender = tokio::spawn(push_file(port, fingerprint, source));
    let offer_id = loop {
        let text = mcp.call_tool("list_offers", json!({})).await.unwrap();
        if let Ok(offers) = serde_json::from_str::<Value>(&text) {
            if let Some(first) = offers.as_array().and_then(|list| list.first()) {
                break first["id"].as_u64().unwrap();
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    let accepted = mcp
        .call_tool("accept_offer", json!({ "id": offer_id }))
        .await
        .expect("the offer was still waiting");
    assert!(
        accepted.contains("accept-me") || accepted.contains("Accepted"),
        "{accepted}"
    );
    let (_id, _size) = sender.await.expect("the sender task finished");

    let landed = mcp.call_tool("list_received", json!({})).await.unwrap();
    let page: Value = serde_json::from_str(&landed).unwrap();
    assert_eq!(page["entries"][0]["name"], "ask-me.txt", "{page}");
    assert_eq!(
        std::fs::read(page["entries"][0]["path"].as_str().unwrap()).unwrap(),
        bytes
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_only_node_does_not_listen_and_still_lists_devices() {
    let mut mcp = Mcp::spawn(&["--no-receive"]).await;
    let status = mcp.status().await;
    assert_eq!(status["listening"], false, "send-only means no listener");
    assert_eq!(status["port"], 0, "nothing was bound");

    // Discovery is still up: the call must answer (here, with an explanation)
    // rather than fail or hang.
    let listed = mcp
        .call_tool("list_devices", json!({ "refresh": false }))
        .await
        .expect("list_devices works without a receiver");
    assert!(
        listed.contains("list_devices") || listed.contains("alias"),
        "{listed}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_file_is_a_refusal_a_model_can_act_on() {
    use localsend_rs::{LocalSendServer, Protocol};

    let mut mcp = Mcp::spawn(&[]).await;
    // A reachable peer, so the refusal is about the file and not the address.
    let dir = TempDir::new().unwrap();
    let (peer, _events) = LocalSendServer::builder()
        .alias("Other Computer")
        .port(0)
        .save_dir(dir.path())
        .protocol(Protocol::Http)
        .auto_accept(true)
        .build()
        .await
        .unwrap();

    let error = mcp
        .call_tool(
            "send_files",
            json!({
                "device": format!("127.0.0.1:{}", peer.port()),
                "files": ["/definitely/not/here.txt"],
            }),
        )
        .await
        .expect_err("a file that does not exist cannot be sent");
    assert!(
        error.contains("not/here.txt"),
        "the refusal names the file: {error}"
    );
}
