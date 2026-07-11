# LocalSend-RS v2.1 Foundation (Phases 0–3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up an automated test harness (in-process + containerized), refactor the server into focused modules, ship the library-first receive API (event stream + accept/decline) with real PIN enforcement, and fix the session/token/schema/collision correctness bugs in the v2 upload path.

**Architecture:** Split the 609-line `src/server/server.rs` into `state / handlers / routes / events / pin` modules. Replace the TUI-only `PendingTransfer` rendezvous with a public `mpsc::Receiver<ServerEvent>` + `PendingRequest` responder handle returned by a new `LocalSendServer::builder()`. Wire the already-written-but-unused `core::Session` in as the real session store (multi-file tracking, random tokens). CLI and TUI become thin consumers of the same events. Layer 4 container e2e mirrors CrossCopy's `e2e/` compose conventions but lives in this repo.

**Tech Stack:** Rust edition 2024, tokio, axum 0.8 (+axum-server for https), reqwest 0.13, serde, uuid, rcgen. Tests: cargo integration tests (`tests/`), dev-deps `tempfile` + `reqwest` + `serde_json`. E2E: Docker + docker-compose.

## Global Constraints

- **Protocol target:** LocalSend **v2.1**, endpoints under `/api/localsend/v2/`. Multicast `224.0.0.167:53317`, default port `53317`. (`src/protocol/constants.rs`)
- **Upload wire rule:** a file is the **entire binary body in ONE POST** — never invent chunking headers.
- **Every commit ends green:** `cargo test` + `cargo clippy --all-targets -- -D warnings` + `cargo fmt --check`. Run `cargo fmt` before every commit.
- **TDD:** write the failing test first for every behavior change.
- **Breaking public API is allowed** (v0.1.2, pre-1.0). The only known consumer is `apps/xc` in the CrossCopy superproject; it is compile-checked separately when the superproject bumps the submodule — do NOT try to keep old constructors alive.
- **All tests live in this repo** (`tests/` for cargo, `e2e/` for containers).
- **Feature flags:** default = `["cli", "https"]`. New server modules must compile with `--no-default-features` too (guard https-only code with `#[cfg(feature = "https")]`).
- **Reference spec:** `docs/superpowers/specs/2026-07-12-localsend-rs-v2.1-alignment-design.md` (issue IDs R1–R11, G1–G2, D1–D5 used below).

---

## File Structure (target after Phase 3)

- `src/server/mod.rs` — module decl + re-exports (`LocalSendServer`, `ServerEvent`, `PendingRequest`, `TransferDecision`)
- `src/server/server.rs` — `LocalSendServer` struct, builder, start/stop/port (slimmed)
- `src/server/state.rs` — `ServerState`, `write_body_to_file` (moved verbatim)
- `src/server/handlers.rs` — all `handle_*` fns + query-param structs (moved, then evolved)
- `src/server/routes.rs` — `create_router`
- `src/server/events.rs` — `ServerEvent`, `PendingRequest`, `TransferDecision` (new, Phase 2)
- `src/server/pin.rs` — `PinGate` 401/429 lockout (new, Phase 2)
- `src/core/session.rs` — `Session` gains random tokens + received-tracking (Phase 3)
- `src/core/file.rs` — gains `unique_save_path` (Phase 3)
- `src/protocol/types.rs` — `Token::random()`, serde-lenient `DeviceInfo` (Phase 3)
- `src/client/client.rs` — gains `cancel`, Content-Length + real progress on upload (Phase 3)
- **Deleted:** `src/core/transfer.rs` (D2), `src/storage/` (D3), `PendingTransfer` + notify plumbing (Phase 2)
- `tests/common/mod.rs` — shared helpers; `tests/interop_upload.rs`, `tests/interop_accept.rs`, `tests/interop_session.rs`, `tests/conformance_pin.rs`, `tests/conformance_prepare_upload.rs`, `tests/unit_schema.rs`
- `e2e/docker/localsend-rs.Dockerfile`, `e2e/docker-compose.yml`, `e2e/scripts/{receiver.sh,sender.sh}`, `e2e/run.sh`

> Migration technique: move code **verbatim** first (Phase 1), change behavior only in later, test-guarded tasks. Keep `pub use` re-exports in `mod.rs` so `lib.rs` and callers stay stable within a task.

---

## Phase 0 — Test Scaffolding

### Task 0.1: Test helpers + first green rs↔rs smoke test

**Files:**
- Modify: `Cargo.toml` (add `[dev-dependencies]`)
- Create: `tests/common/mod.rs`
- Create: `tests/interop_upload.rs`

**Interfaces:**
- Produces (used by every later test): `common::free_port() -> u16`, `common::make_random_file(dir: &Path, name: &str, size: usize) -> (PathBuf, String)` (returns path + sha256-hex), `common::wait_for_http_info(port: u16)` (async; polls until the server answers), `common::target_device(port: u16) -> DeviceInfo` (a `127.0.0.1` HTTP target).

- [ ] **Step 1: Add dev-dependencies to `Cargo.toml`**

Append after the existing `[dependencies]` block:

```toml
[dev-dependencies]
tempfile = "3"
reqwest = { version = "0.13", features = ["json", "rustls"] }
serde_json = "1.0"
tokio = { version = "1.49", features = ["full", "test-util"] }
```

(Integration tests in `tests/` only see the crate's public API plus dev-dependencies — that is why `reqwest`/`tokio` are repeated here.)

- [ ] **Step 2: Write `tests/common/mod.rs`**

```rust
#![allow(dead_code)] // each integration-test binary uses a subset of these helpers

use localsend_rs::{DeviceInfo, Protocol, sha256_from_bytes};
use std::path::{Path, PathBuf};

/// Bind port 0, read the assigned port, release it.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

/// Deterministic pseudo-random file (xorshift, no extra deps).
/// Returns (path, sha256-hex-of-contents).
pub fn make_random_file(dir: &Path, name: &str, size: usize) -> (PathBuf, String) {
    let mut buf = vec![0u8; size];
    let mut x: u64 = 0x9E3779B97F4A7C15 ^ (size as u64);
    for b in buf.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x & 0xFF) as u8;
    }
    let sha = sha256_from_bytes(&buf);
    let path = dir.join(name);
    std::fs::write(&path, &buf).expect("write random file");
    (path, sha)
}

/// Poll GET /info until the server answers (or panic after ~5 s).
pub async fn wait_for_http_info(port: u16) {
    let url = format!("http://127.0.0.1:{port}/api/localsend/v2/info");
    for _ in 0..50 {
        if reqwest::get(&url).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("server on port {port} never became ready");
}

/// A DeviceInfo pointing at a local HTTP server, usable as a client target.
pub fn target_device(port: u16) -> DeviceInfo {
    let mut d = DeviceInfo::new("test-target".to_string(), port, Protocol::Http);
    d.ip = Some("127.0.0.1".to_string());
    d.fingerprint = "test-target-fp".to_string();
    d
}
```

> If `sha256_from_bytes` returns a different casing than expected, normalize with `.to_lowercase()` in both places you compare — but check `src/crypto/hash.rs` first; it is already lowercase hex.

- [ ] **Step 3: Write `tests/interop_upload.rs` (works against CURRENT code)**

The current server can only accept via the TUI rendezvous — the test drives that rendezvous directly. This hack is deleted in Task 2.5 when the real accept API replaces it.

```rust
mod common;

use localsend_rs::server::{LocalSendServer, PendingTransfer};
use localsend_rs::{DeviceInfo, LocalSendClient, Protocol, build_file_metadata, sha256_from_file};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Notify, RwLock};

#[tokio::test]
async fn uploads_a_file_byte_for_byte_rs_to_rs() {
    let port = common::free_port();
    let save_dir = tempfile::tempdir().expect("save dir");
    let src_dir = tempfile::tempdir().expect("src dir");

    // --- receiver (current API; rendezvous hack, removed in Phase 2) ---
    let mut device = DeviceInfo::new("Test Receiver".to_string(), port, Protocol::Http);
    device.fingerprint = "receiver-fp".to_string();
    let pending: Arc<RwLock<Option<PendingTransfer>>> = Arc::new(RwLock::new(None));
    let received = Arc::new(RwLock::new(Vec::new()));
    let mut server = LocalSendServer::new_with_device(
        device,
        save_dir.path().to_path_buf(),
        false,
        pending.clone(),
        received,
    )
    .expect("server");
    let notify = Arc::new(Notify::new());
    server.set_pending_transfer_notify(notify.clone());
    server.start(None).await.expect("start");

    let pending_for_task = pending.clone();
    tokio::spawn(async move {
        notify.notified().await;
        if let Some(t) = pending_for_task.write().await.take() {
            let _ = t.response_tx.send(true);
        }
    });

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

    let prep = client.prepare_upload(&target, files, None).await.expect("prepare");
    let token = prep.files.get(&file_id).expect("token").clone();
    client
        .upload_file(&target, &prep.session_id, &file_id, &token, &file_path, None)
        .await
        .expect("upload");

    // --- assert byte-identical ---
    let got_sha = sha256_from_file(save_dir.path().join("hello.bin"))
        .await
        .expect("saved file");
    assert_eq!(got_sha, want_sha);

    server.stop();
}
```

> If `sha256_from_file`'s exact signature differs (check `src/crypto/hash.rs` — it may take `&Path` or `impl AsRef<Path>` and may be sync), adapt the call; the assertion stays the same.

- [ ] **Step 4: Run — verify GREEN**

Run: `cargo test --test interop_upload`
Expected: 1 passed. (Proves the harness drives the current code end-to-end.)

- [ ] **Step 5: Full check + commit**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
git add Cargo.toml Cargo.lock tests/
git commit -m "test: integration harness + rs<->rs upload smoke test"
```

---

## Phase 1 — Structural refactor (no behavior change)

### Task 1.1: Split `src/server/server.rs` into state / handlers / routes

**Files:**
- Create: `src/server/state.rs`
- Create: `src/server/handlers.rs`
- Create: `src/server/routes.rs`
- Modify: `src/server/server.rs` (shrinks to `LocalSendServer` impl)
- Modify: `src/server/mod.rs`

**Interfaces:**
- Produces: `state::ServerState` (fields unchanged, now `pub(crate)` usable from handlers), `state::write_body_to_file`, `handlers::{handle_info, handle_register, handle_prepare_upload, handle_upload, handle_cancel}`, `routes::create_router(state: Arc<RwLock<ServerState>>) -> Router`. Public exports from `server::` unchanged: `LocalSendServer`, `PendingTransfer`, `ProgressCallback`.

- [ ] **Step 1: Create `src/server/state.rs`** — move these items **verbatim** from `server.rs`: `ProgressCallback` type alias, `PendingTransfer`, `ActiveSession`, `ServerState`, `write_body_to_file`, `publish_pending_transfer`, and the whole `#[cfg(test)] mod tests` block (it only tests `write_body_to_file` + `publish_pending_transfer`). Add the imports those items need (`axum::body::Body`, `tokio::io::AsyncWriteExt`, `futures_util::StreamExt`, protocol types, `oneshot`/`Notify`/`RwLock`, `HashMap`, `Path`/`PathBuf`, `Arc`, `Instant`).

- [ ] **Step 2: Create `src/server/handlers.rs`** — move **verbatim**: `handle_info`, `handle_register`, `PrepareUploadParams`, `handle_prepare_upload`, `UploadParams`, `handle_upload`, `CancelParams`, `handle_cancel`. Import from `super::state::{ServerState, PendingTransfer, publish_pending_transfer, write_body_to_file}`.

- [ ] **Step 3: Create `src/server/routes.rs`**

```rust
use super::handlers::{
    handle_cancel, handle_info, handle_prepare_upload, handle_register, handle_upload,
};
use super::state::ServerState;
use axum::{
    Router,
    routing::{get, post},
};
use std::sync::Arc;
use tokio::sync::RwLock;

pub(crate) fn create_router(state: Arc<RwLock<ServerState>>) -> Router {
    Router::new()
        .route("/api/localsend/v2/info", get(handle_info))
        .route("/api/localsend/v2/register", post(handle_register))
        .route("/api/localsend/v2/prepare-upload", post(handle_prepare_upload))
        .route("/api/localsend/v2/upload", post(handle_upload))
        .route("/api/localsend/v2/cancel", post(handle_cancel))
        .with_state(state)
}
```

- [ ] **Step 4: Slim `src/server/server.rs`** to just the `LocalSendServer` struct + impl (`new`, `new_with_device`, `set_pending_transfer_notify`, `set_tls_certificate`, `start`, `stop`), replacing the private `Self::create_router(...)` call with `super::routes::create_router(...)` and importing moved types from `super::state`.

- [ ] **Step 5: Update `src/server/mod.rs`**

```rust
#![allow(clippy::module_inception)]

pub mod server;

pub(crate) mod handlers;
pub(crate) mod routes;
pub(crate) mod state;

pub use server::LocalSendServer;
pub use state::{PendingTransfer, ProgressCallback};
```

(`PendingTransfer` was previously re-exported from `server::server`; tests/TUI import it via `crate::server::PendingTransfer`, which still resolves.)

- [ ] **Step 6: Verify + commit**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: identical pass count to Task 0.1 (moves only, no behavior change).

```bash
cargo fmt
git add src/server tests
git commit -m "refactor(server): split server.rs into state/handlers/routes modules"
```

### Task 1.2: Delete dead abstractions (D2 `TransferState`, D3 `storage/`)

**Files:**
- Delete: `src/core/transfer.rs`, `src/storage/` (whole directory)
- Modify: `src/core/mod.rs`, `src/lib.rs`

- [ ] **Step 1: Prove they are unused in-repo**

Run: `grep -rn "TransferState\|FileSystem\|TokioFileSystem\|storage::" src --include="*.rs" | grep -v "src/core/transfer.rs\|src/storage/"`
Expected: only the re-export lines in `src/core/mod.rs` and `src/lib.rs`. If anything else shows up, STOP and reassess before deleting.

- [ ] **Step 2: Delete** `src/core/transfer.rs` and `src/storage/`. In `src/core/mod.rs` remove `pub mod transfer;` and `pub use transfer::TransferState;`. In `src/lib.rs` remove `pub mod storage;`, remove `TransferState` from the `pub use core::{...}` list, and delete the line `pub use storage::{FileSystem, TokioFileSystem};`.

- [ ] **Step 3: Check whether `async-trait` is still needed**

Run: `grep -rn "async_trait" src --include="*.rs"`
If the only hits were in `src/storage/`, remove `async-trait = "0.1.89"` from `Cargo.toml`; if `src/discovery/traits.rs` uses it, keep it.

- [ ] **Step 4: Verify + commit**

```bash
cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt
git add -A
git commit -m "refactor: remove unused TransferState and storage abstraction (D2, D3)"
```

### Task 1.3: Wire the dead subnet scan (D4)

**Files:**
- Modify: `src/discovery/http.rs`

- [ ] **Step 1:** Remove the file-level `#![allow(dead_code)]` from `src/discovery/http.rs:1`. Make `scan_subnet` a `pub` method (it already exists — only visibility and doc comment change). Replace the idle 30 s timer loop inside `start()` with a single scan invocation, or — if `start()`'s `Discovery` trait contract makes that awkward — leave `start()` as a no-op that logs `tracing::debug!("HttpDiscovery: passive; call scan_subnet() explicitly")`. Compile errors from newly-visible dead items tell you exactly what else to make `pub` or delete.
- [ ] **Step 2:** Add a doc comment on `scan_subnet` stating it sweeps `x.y.z.1..=255` with `GET /info` and returns discovered devices.
- [ ] **Step 3:** `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt`, then:

```bash
git add src/discovery/http.rs
git commit -m "refactor(discovery): expose subnet scan as public API; drop dead idle loop (D4)"
```

---

## Phase 2 — Library receive API + PIN enforcement

### Task 2.1: `ServerEvent` + `PendingRequest` types

**Files:**
- Create: `src/server/events.rs`
- Modify: `src/server/mod.rs`

**Interfaces:**
- Produces (relied on by Tasks 2.2–2.5, 3.1 and every consumer):

```rust
pub enum ServerEvent {
    TransferRequest(PendingRequest),
    FileReceived { session_id: SessionId, file_id: FileId, file_name: String,
                   path: PathBuf, size: u64, sender_alias: String },
    SessionDone { session_id: SessionId },
}
pub enum TransferDecision { Accept, AcceptFiles(Vec<FileId>), Decline }
impl PendingRequest {
    pub fn sender(&self) -> &DeviceInfo;
    pub fn files(&self) -> &HashMap<FileId, FileMetadata>;
    pub fn accept(self);
    pub fn accept_files(self, ids: Vec<FileId>);
    pub fn decline(self);
    // crate-internal constructor:
    pub(crate) fn new(sender: DeviceInfo, files: HashMap<FileId, FileMetadata>)
        -> (Self, tokio::sync::oneshot::Receiver<TransferDecision>);
}
```

- [ ] **Step 1: Write the failing unit test** (inside `src/server/events.rs`, bottom):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{DeviceInfo, Protocol};
    use std::collections::HashMap;

    fn req() -> (PendingRequest, tokio::sync::oneshot::Receiver<TransferDecision>) {
        let sender = DeviceInfo::new("s".to_string(), 53317, Protocol::Http);
        PendingRequest::new(sender, HashMap::new())
    }

    #[tokio::test]
    async fn accept_sends_accept_decision() {
        let (r, rx) = req();
        r.accept();
        assert!(matches!(rx.await, Ok(TransferDecision::Accept)));
    }

    #[tokio::test]
    async fn decline_sends_decline_decision() {
        let (r, rx) = req();
        r.decline();
        assert!(matches!(rx.await, Ok(TransferDecision::Decline)));
    }

    #[tokio::test]
    async fn dropping_request_closes_channel() {
        let (r, rx) = req();
        drop(r);
        assert!(rx.await.is_err()); // handler treats closed channel as decline
    }
}
```

- [ ] **Step 2: Run — verify FAIL** (module doesn't exist).

Run: `cargo test --lib server::events`
Expected: compile error.

- [ ] **Step 3: Implement `src/server/events.rs`**

```rust
//! Public event stream for library consumers (the headless accept API).

use crate::protocol::{DeviceInfo, FileId, FileMetadata, SessionId};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::oneshot;

/// Events emitted by [`crate::server::LocalSendServer`].
#[derive(Debug)]
pub enum ServerEvent {
    /// A sender wants to transfer files. Respond via the [`PendingRequest`].
    /// Dropping the request (or ignoring it past the accept timeout) declines it.
    TransferRequest(PendingRequest),
    /// One file finished writing to disk.
    FileReceived {
        session_id: SessionId,
        file_id: FileId,
        file_name: String,
        path: PathBuf,
        size: u64,
        sender_alias: String,
    },
    /// All accepted files of a session arrived (or the session was cancelled).
    SessionDone { session_id: SessionId },
}

/// The consumer's answer to a transfer request.
#[derive(Debug, Clone, PartialEq)]
pub enum TransferDecision {
    Accept,
    AcceptFiles(Vec<FileId>),
    Decline,
}

/// Handle to answer an incoming `prepare-upload`. Consume it exactly once.
#[derive(Debug)]
pub struct PendingRequest {
    sender: DeviceInfo,
    files: HashMap<FileId, FileMetadata>,
    responder: oneshot::Sender<TransferDecision>,
}

impl PendingRequest {
    pub(crate) fn new(
        sender: DeviceInfo,
        files: HashMap<FileId, FileMetadata>,
    ) -> (Self, oneshot::Receiver<TransferDecision>) {
        let (tx, rx) = oneshot::channel();
        (Self { sender, files, responder: tx }, rx)
    }

    pub fn sender(&self) -> &DeviceInfo {
        &self.sender
    }

    pub fn files(&self) -> &HashMap<FileId, FileMetadata> {
        &self.files
    }

    /// Accept every offered file. No-op if the sender already timed out.
    pub fn accept(self) {
        let _ = self.responder.send(TransferDecision::Accept);
    }

    /// Accept a subset of the offered files (empty = decline).
    pub fn accept_files(self, ids: Vec<FileId>) {
        let _ = self.responder.send(TransferDecision::AcceptFiles(ids));
    }

    pub fn decline(self) {
        let _ = self.responder.send(TransferDecision::Decline);
    }
}
```

Add to `src/server/mod.rs`: `pub mod events;` and `pub use events::{PendingRequest, ServerEvent, TransferDecision};`

- [ ] **Step 4: Run — verify PASS**

Run: `cargo test --lib server::events`
Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/server
git commit -m "feat(server): ServerEvent stream + PendingRequest accept/decline handle (R1 groundwork)"
```

### Task 2.2: Route `prepare-upload` through the event stream

**Files:**
- Modify: `src/server/state.rs` (swap rendezvous fields for `events_tx` + config)
- Modify: `src/server/handlers.rs` (`handle_prepare_upload`)
- Modify: `src/server/server.rs` (`start` creates the channel; temporary accessor)
- Create: `tests/interop_accept.rs`

**Interfaces:**
- Consumes: `PendingRequest::new`, `TransferDecision` (Task 2.1).
- Produces: `ServerState` fields `events_tx: tokio::sync::mpsc::Sender<ServerEvent>`, `auto_accept: bool`, `accept_timeout: std::time::Duration`; `LocalSendServer::take_events(&mut self) -> Option<mpsc::Receiver<ServerEvent>>` (temporary until the builder in 2.3 returns the receiver directly).

- [ ] **Step 1: Write the failing integration test `tests/interop_accept.rs`**

```rust
mod common;

use localsend_rs::server::{LocalSendServer, PendingTransfer, ServerEvent};
use localsend_rs::{DeviceInfo, LocalSendClient, LocalSendError, Protocol, build_file_metadata};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

async fn start_receiver(port: u16, save_dir: std::path::PathBuf) -> (LocalSendServer, tokio::sync::mpsc::Receiver<ServerEvent>) {
    let mut device = DeviceInfo::new("Receiver".to_string(), port, Protocol::Http);
    device.fingerprint = "receiver-fp".to_string();
    let pending: Arc<RwLock<Option<PendingTransfer>>> = Arc::new(RwLock::new(None));
    let received = Arc::new(RwLock::new(Vec::new()));
    let mut server =
        LocalSendServer::new_with_device(device, save_dir, false, pending, received).unwrap();
    server.start(None).await.unwrap();
    let events = server.take_events().expect("events receiver");
    (server, events)
}

fn one_file(dir: &std::path::Path) -> (HashMap<localsend_rs::FileId, localsend_rs::FileMetadata>, localsend_rs::FileId, std::path::PathBuf) {
    let (path, _) = common::make_random_file(dir, "a.bin", 512);
    let meta = futures_blocking(build_file_metadata(&path));
    let id = meta.id.clone();
    let mut m = HashMap::new();
    m.insert(id.clone(), meta);
    (m, id, path)
}

// tiny helper: run an async fn to completion inside a test-owned runtime piece
fn futures_blocking<T>(fut: impl std::future::Future<Output = localsend_rs::Result<T>>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut)).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn event_consumer_can_accept_a_transfer() {
    let port = common::free_port();
    let save = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let (mut server, mut events) = start_receiver(port, save.path().to_path_buf()).await;
    common::wait_for_http_info(port).await;

    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            if let ServerEvent::TransferRequest(req) = ev {
                assert_eq!(req.sender().alias, "Sender");
                req.accept();
            }
        }
    });

    let (files, id, path) = one_file(src.path());
    let mut dev = DeviceInfo::new("Sender".to_string(), 0, Protocol::Http);
    dev.fingerprint = "sender-fp".to_string();
    let client = LocalSendClient::new(dev);
    let target = common::target_device(port);
    let prep = client.prepare_upload(&target, files, None).await.expect("accepted");
    let token = prep.files.get(&id).unwrap().clone();
    client.upload_file(&target, &prep.session_id, &id, &token, &path, None).await.unwrap();
    assert!(save.path().join("a.bin").exists());
    server.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn event_consumer_can_decline_a_transfer() {
    let port = common::free_port();
    let save = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let (mut server, mut events) = start_receiver(port, save.path().to_path_buf()).await;
    common::wait_for_http_info(port).await;

    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            if let ServerEvent::TransferRequest(req) = ev {
                req.decline();
            }
        }
    });

    let (files, _id, _path) = one_file(src.path());
    let mut dev = DeviceInfo::new("Sender".to_string(), 0, Protocol::Http);
    dev.fingerprint = "sender-fp".to_string();
    let client = LocalSendClient::new(dev);
    let target = common::target_device(port);
    let err = client.prepare_upload(&target, files, None).await.expect_err("declined");
    assert!(matches!(err, LocalSendError::Rejected { status: 403 }));
    server.stop();
}
```

> `LocalSendError` must be matchable from tests; it already is (`pub use error::{LocalSendError, Result}` in `lib.rs`, `#[non_exhaustive]` still allows `matches!` on named variants).

- [ ] **Step 2: Run — verify FAIL**

Run: `cargo test --test interop_accept`
Expected: compile error (`take_events`, `ServerEvent` not exported from `server::`).

- [ ] **Step 3: Implement.** In `src/server/state.rs`:
  - Remove fields `pending_transfer`, `pending_transfer_notify` from `ServerState`; remove `publish_pending_transfer` (and its unit test). **Keep `PendingTransfer` struct + its test for now** — the CLI/TUI/Task-0.1 test still reference it until Tasks 2.4/2.5.
  - Add fields:

```rust
pub struct ServerState {
    pub device: DeviceInfo,
    pub current_session: Option<ActiveSession>,
    pub save_dir: PathBuf,
    pub _progress_callback: Option<ProgressCallback>,
    pub received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    pub events_tx: tokio::sync::mpsc::Sender<crate::server::events::ServerEvent>,
    pub auto_accept: bool,
    pub accept_timeout: std::time::Duration,
}
```

  In `src/server/server.rs`:
  - `LocalSendServer` gains `events_rx: Option<tokio::sync::mpsc::Receiver<ServerEvent>>`, `auto_accept: bool` (default `false`), `accept_timeout: Duration` (default 60 s). `start()` creates `let (events_tx, events_rx) = tokio::sync::mpsc::channel(64);`, stores the receiver in `self.events_rx`, passes `events_tx`/`auto_accept`/`accept_timeout` into `ServerState`. The old `pending_transfer`/`received_files` constructor params: keep `received_files`, **ignore** `pending_transfer` (parameter stays until 2.4/2.5 so callers compile; mark `_pending_transfer`). `set_pending_transfer_notify` becomes a no-op with `#[deprecated]` (removed in 2.5).
  - Add:

```rust
/// Take the event receiver. Returns `Some` once, after `start()`.
pub fn take_events(&mut self) -> Option<tokio::sync::mpsc::Receiver<ServerEvent>> {
    self.events_rx.take()
}

pub fn set_auto_accept(&mut self, yes: bool) {
    self.auto_accept = yes;
}
```

  In `src/server/handlers.rs`, replace the rendezvous block of `handle_prepare_upload` (currently `server.rs:305-373` logic) with:

```rust
// Decide: auto-accept, or ask the event consumer.
let decision = if auto_accept {
    crate::server::events::TransferDecision::Accept
} else {
    let (request, decision_rx) = crate::server::events::PendingRequest::new(
        request.info.clone(),
        request.files.clone(),
    );
    if events_tx
        .send(crate::server::events::ServerEvent::TransferRequest(request))
        .await
        .is_err()
    {
        // No consumer listening -> decline.
        crate::server::events::TransferDecision::Decline
    } else {
        match tokio::time::timeout(accept_timeout, decision_rx).await {
            Ok(Ok(d)) => d,
            _ => crate::server::events::TransferDecision::Decline, // dropped or timed out
        }
    }
};

let accepted_ids: Vec<FileId> = match decision {
    crate::server::events::TransferDecision::Accept => request.files.keys().cloned().collect(),
    crate::server::events::TransferDecision::AcceptFiles(ids) => ids
        .into_iter()
        .filter(|id| request.files.contains_key(id))
        .collect(),
    crate::server::events::TransferDecision::Decline => Vec::new(),
};

if accepted_ids.is_empty() {
    let mut state = state_ref.write().await;
    state.current_session = None;
    tracing::info!("Transfer declined (or timed out)");
    return StatusCode::FORBIDDEN.into_response();
}
```

  `auto_accept`, `accept_timeout`, `events_tx` are read from `state_ref` **before** publishing the session (clone them out of a short read-lock — never hold the lock across the `timeout` await). The token map returned must contain **only** `accepted_ids` (build `files_map` after the decision, not before). The session's `files` stored in `ActiveSession` must also be filtered to accepted ids.

- [ ] **Step 4: Update Task-0.1 smoke test** — `tests/interop_upload.rs` still compiles (constructor unchanged), but its notify-hack no longer fires. Replace the rendezvous block (the `Arc<RwLock<...>>` + `Notify` + spawn) with:

```rust
    server.set_auto_accept(true);
    server.start(None).await.expect("start");
```

(Keep constructing the dummy `pending`/`received` Arcs for `new_with_device` until 2.5 removes those params.)

- [ ] **Step 5: Run — verify PASS**

Run: `cargo test`
Expected: all green, including both new interop_accept tests and the updated smoke test.

- [ ] **Step 6: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add -A
git commit -m "feat(server)!: prepare-upload accepts via public event stream; auto-accept mode (R1)"
```

### Task 2.3: `LocalSendServer::builder()` + real bound port

**Files:**
- Modify: `src/server/server.rs` (builder struct + `port()`; bind-before-spawn)
- Create: `tests/interop_builder.rs`

**Interfaces:**
- Produces (the canonical construction path from here on):

```rust
LocalSendServer::builder() -> LocalSendServerBuilder
LocalSendServerBuilder::{alias(impl Into<String>), port(u16 /*0=ephemeral*/),
    save_dir(impl Into<PathBuf>), protocol(Protocol), pin(impl Into<String>),
    auto_accept(bool), accept_timeout(Duration)}
LocalSendServerBuilder::build(self) -> crate::Result<(LocalSendServer, mpsc::Receiver<ServerEvent>)> // async; server is LISTENING on return
LocalSendServer::port(&self) -> u16      // actual bound port
LocalSendServer::device(&self) -> &DeviceInfo
```

- [ ] **Step 1: Write the failing test `tests/interop_builder.rs`**

```rust
mod common;

use localsend_rs::server::LocalSendServer;
use localsend_rs::Protocol;

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
```

- [ ] **Step 2: Run — verify FAIL** (`builder` not defined). Run: `cargo test --test interop_builder`

- [ ] **Step 3: Implement.**
  - **Bind before spawn** so the port is known: in `start()`'s HTTP path the `TcpListener::bind` already happens before `tokio::spawn` — capture `listener.local_addr()?.port()` into `self.bound_port: Option<u16>` and **write it back into `self.device.port` and the `ServerState.device.port`** when the requested port was 0. For the HTTPS path, bind a `std::net::TcpListener` first (`std::net::TcpListener::bind(&addr)?`, `listener.set_nonblocking(true)?`), read its port the same way, then serve with `axum_server::from_tcp_rustls(listener, tls_config)`.
  - Builder (same file):

```rust
pub struct LocalSendServerBuilder {
    alias: String,
    port: u16,
    save_dir: PathBuf,
    protocol: Protocol,
    pin: Option<String>,
    auto_accept: bool,
    accept_timeout: std::time::Duration,
}

impl LocalSendServer {
    pub fn builder() -> LocalSendServerBuilder {
        LocalSendServerBuilder {
            alias: "LocalSend-Rust".to_string(),
            port: crate::protocol::DEFAULT_HTTP_PORT,
            save_dir: PathBuf::from("./downloads"),
            protocol: Protocol::Http,
            pin: None,
            auto_accept: false,
            accept_timeout: std::time::Duration::from_secs(60),
        }
    }

    pub fn port(&self) -> u16 {
        self.device.port
    }

    pub fn device(&self) -> &DeviceInfo {
        &self.device
    }
}

impl LocalSendServerBuilder {
    pub fn alias(mut self, alias: impl Into<String>) -> Self { self.alias = alias.into(); self }
    pub fn port(mut self, port: u16) -> Self { self.port = port; self }
    pub fn save_dir(mut self, dir: impl Into<PathBuf>) -> Self { self.save_dir = dir.into(); self }
    pub fn protocol(mut self, protocol: Protocol) -> Self { self.protocol = protocol; self }
    pub fn pin(mut self, pin: impl Into<String>) -> Self { self.pin = Some(pin.into()); self }
    pub fn auto_accept(mut self, yes: bool) -> Self { self.auto_accept = yes; self }
    pub fn accept_timeout(mut self, d: std::time::Duration) -> Self { self.accept_timeout = d; self }

    pub async fn build(
        self,
    ) -> crate::Result<(LocalSendServer, tokio::sync::mpsc::Receiver<crate::server::ServerEvent>)> {
        let https = matches!(self.protocol, Protocol::Https);

        #[cfg(feature = "https")]
        let tls_cert = if https { Some(crate::crypto::generate_tls_certificate()?) } else { None };
        #[cfg(not(feature = "https"))]
        if https {
            return Err(crate::error::LocalSendError::network(
                "HTTPS support not enabled; build with --features https",
            ));
        }

        // HTTPS identity = SHA-256 of the cert (spec); HTTP = random string.
        let fingerprint = {
            #[cfg(feature = "https")]
            if let Some(ref cert) = tls_cert {
                cert.fingerprint.clone()
            } else {
                crate::crypto::generate_fingerprint()
            }
            #[cfg(not(feature = "https"))]
            crate::crypto::generate_fingerprint()
        };

        let device = DeviceInfo {
            alias: self.alias,
            version: crate::protocol::PROTOCOL_VERSION.to_string(),
            device_model: Some(crate::core::device::get_device_model()),
            device_type: Some(crate::core::device::get_device_type()),
            fingerprint,
            port: self.port,
            protocol: self.protocol,
            download: false,
            ip: None,
        };

        let mut server = LocalSendServer::from_parts(
            device,
            self.save_dir,
            https,
            self.pin,
            self.auto_accept,
            self.accept_timeout,
        )?;
        #[cfg(feature = "https")]
        if let Some(cert) = tls_cert {
            server.set_tls_certificate(cert);
        }
        server.start(None).await?;
        let events = server.take_events().expect("events available after start");
        Ok((server, events))
    }
}
```

  `from_parts` is a private constructor holding the fields `new_with_device` used to take, plus `pin`/`auto_accept`/`accept_timeout` (pin is stored now, enforced in Task 2.6). `save_dir(save.path())` must accept `&Path` — `impl Into<PathBuf>` doesn't cover `&Path`; use `save_dir(save.path().to_path_buf())` in tests **or** change the bound to `impl AsRef<Path>` + `.as_ref().to_path_buf()`. Pick `impl AsRef<Path>` (friendlier).

- [ ] **Step 4: Run — verify PASS.** `cargo test --test interop_builder` then full `cargo test`.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add -A
git commit -m "feat(server): builder API with ephemeral-port support; https fingerprint from cert"
```

### Task 2.4: Rewire CLI `receive` onto the event stream (headless receive finally works)

**Files:**
- Modify: `src/cli/commands/receive.rs`

**Interfaces:**
- Consumes: `LocalSendServer::builder()`, `ServerEvent`, `PendingRequest`.
- Produces: `localsend-rs receive --auto-accept` actually accepts (used by e2e in Task 3.6).

- [ ] **Step 1: Rewrite the server-construction half of `execute()`** (keep the directory-creation, discovery, and ctrl-c parts). Replace everything from `let fingerprint = ...` down to `server.start(None).await?;` with:

```rust
    let protocol_enum = if https_enabled {
        crate::protocol::Protocol::Https
    } else {
        crate::protocol::Protocol::Http
    };

    let mut builder = crate::server::LocalSendServer::builder()
        .alias("LocalSend-Rust".to_string())
        .port(command.port)
        .save_dir(&command.directory)
        .protocol(protocol_enum)
        .auto_accept(command.auto_accept);
    if let Some(ref pin) = command.pin {
        builder = builder.pin(pin.clone());
    }
    let (server, mut events) = builder.build().await?;

    // Discovery must announce the SAME device identity the server uses.
    let mut discovery =
        crate::discovery::MulticastDiscovery::new_with_device(server.device().clone());
    println!("Starting multicast discovery...");
    discovery.start().await?;
    println!("Announcing presence to network...");
    discovery.announce_presence().await?;

    let auto_accept = command.auto_accept;
    let event_loop = tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            match ev {
                crate::server::ServerEvent::TransferRequest(req) => {
                    println!(
                        "Incoming transfer from '{}' ({} file(s))",
                        req.sender().alias,
                        req.files().len()
                    );
                    if auto_accept {
                        req.accept();
                    } else {
                        // Headless interactive: y/n on stdin.
                        let accept = inquire::Confirm::new("Accept this transfer?")
                            .with_default(false)
                            .prompt()
                            .unwrap_or(false);
                        if accept { req.accept() } else { req.decline() }
                    }
                }
                crate::server::ServerEvent::FileReceived { file_name, path, size, sender_alias, .. } => {
                    println!("Received '{}' ({} bytes) from {} -> {}", file_name, size, sender_alias, path.display());
                }
                crate::server::ServerEvent::SessionDone { session_id } => {
                    println!("Session {} complete", session_id);
                }
            }
        }
    });
```

  Note: builder auto-generates the HTTPS cert and cert-derived fingerprint itself — delete the manual `tls_cert`/`fingerprint`/`DeviceInfo` construction and the `pending_transfer`/`received_files` Arcs from this file. Move the discovery start **after** `build()` (shown above) so it announces the cert-derived fingerprint. Keep `tokio::signal::ctrl_c().await?;` then `event_loop.abort(); server.stop(); discovery.stop();`.

  `ServerEvent::FileReceived` is only emitted starting Task 3.1 — until then the arm simply never fires; it must still compile (it does — the variant exists since 2.1).

- [ ] **Step 2: Manual verification** (two terminals):

Terminal A: `cargo run -- receive --directory /tmp/lsrs-recv --auto-accept --port 53399`
Terminal B: `echo hi > /tmp/hi.txt && cargo run -- send 127.0.0.1 /tmp/hi.txt`
(Note: `send` probes port 53317 by default (`probe_device`), so for this manual check use port 53317 in terminal A instead if the probe fails; the point is the receiver **accepts without a TUI** and `/tmp/lsrs-recv/hi.txt` appears.)
Expected: transfer completes; previously this timed out 60 s then 403.

- [ ] **Step 3: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add src/cli/commands/receive.rs
git commit -m "fix(cli)!: receive consumes the event stream; --auto-accept and interactive accept work headless (R1)"
```

### Task 2.5: Rewire TUI onto the event stream; delete the old rendezvous

**Files:**
- Modify: `src/tui/app.rs`, `src/tui/popup.rs`
- Modify: `src/server/state.rs`, `src/server/server.rs`, `src/server/mod.rs`, `src/lib.rs`
- Modify: `tests/interop_upload.rs`, `tests/interop_accept.rs`

**Interfaces:**
- Consumes: builder + events (2.3).
- Produces: `PendingTransfer`, `new`, `new_with_device`, `set_pending_transfer_notify` **removed** from the public API. `App` holds `events_rx: tokio::sync::mpsc::Receiver<ServerEvent>`.

- [ ] **Step 1: TUI seams.** In `src/tui/popup.rs`, change the `TransferConfirm` variant to hold the request handle:

```rust
    TransferConfirm {
        request: crate::server::PendingRequest,
    },
```

(Sender alias and file list for rendering come from `request.sender()` / `request.files()`.)

  In `src/tui/app.rs`:
  - `App` field swap: replace `pending_transfer: Arc<RwLock<Option<PendingTransfer>>>` with `events_rx: Option<tokio::sync::mpsc::Receiver<ServerEvent>>` (None until the server starts). Remove the `PendingTransfer` import.
  - Server startup (around `app.rs:195-208`): construct via `LocalSendServer::builder()` with the App's alias/port/protocol, `auto_accept(false)`; store `self.events_rx = Some(events)`.
  - `check_pending_transfer` becomes `poll_server_events`:

```rust
    fn poll_server_events(&mut self) {
        let Some(rx) = self.events_rx.as_mut() else { return };
        while let Ok(ev) = rx.try_recv() {
            match ev {
                crate::server::ServerEvent::TransferRequest(request) => {
                    if self.popup.is_none() {
                        self.popup = Some(Popup::TransferConfirm { request });
                    } else {
                        request.decline(); // busy with another dialog
                    }
                }
                crate::server::ServerEvent::FileReceived { file_name, size, sender_alias, time_is_now @ .., } => { /* push into received_files list */ }
                crate::server::ServerEvent::SessionDone { .. } => {}
            }
        }
    }
```

    (The `FileReceived` arm replaces the old shared `received_files` Arc write that the server used to do — push a `ReceivedFile { file_name, size, sender: sender_alias, time: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string() }` into the existing list. The `time_is_now @ ..` placeholder above is illustrative destructuring — bind the real fields `file_name, size, sender_alias` and ignore the rest with `..`.)
  - `handle_popup_key` (around `app.rs:326-345`): the y/n arms become:

```rust
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                        if let Some(Popup::TransferConfirm { request }) = self.popup.take() {
                            request.accept();
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        if let Some(Popup::TransferConfirm { request }) = self.popup.take() {
                            request.decline();
                        }
                    }
```

  - Anywhere the popup **renders** sender/files, read them through `request.sender()` / `request.files()` (compiler errors will point at every site).

- [ ] **Step 2: Delete the legacy API.** In `src/server/state.rs` remove `PendingTransfer` and its remaining test. In `src/server/server.rs` remove `new`, `new_with_device`, `set_pending_transfer_notify` (keep private `from_parts` + builder). In `src/server/mod.rs` remove `PendingTransfer` from re-exports. `src/lib.rs` needs no change (it re-exports `server::LocalSendServer` only) — but verify with `grep -rn "PendingTransfer" src tests`.

- [ ] **Step 3: Migrate the two older tests** to the builder API. In `tests/interop_upload.rs` and `tests/interop_accept.rs`, replace all `new_with_device` + Arc plumbing with:

```rust
    let (mut server, mut events) = LocalSendServer::builder()
        .alias("Receiver")
        .port(0)
        .save_dir(save.path())
        .protocol(Protocol::Http)
        .auto_accept(true) // interop_upload; interop_accept keeps auto_accept(false) + event loop
        .build()
        .await
        .expect("build");
    let port = server.port();
```

  (`common::free_port()` is no longer needed in these two tests — ephemeral port comes from the server. `interop_accept`'s `start_receiver` helper shrinks to a builder call with `auto_accept(false)`.)

- [ ] **Step 4: Full verify**

Run: `cargo test && cargo clippy --all-targets --all-features -- -D warnings && cargo build --features tui`
Expected: all tests green; TUI feature compiles.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add -A
git commit -m "refactor(tui)!: TUI consumes ServerEvent stream; delete PendingTransfer rendezvous (R1 complete)"
```

### Task 2.6: PIN enforcement with 401 + 3-fail→429 lockout (R2)

**Files:**
- Create: `src/server/pin.rs`
- Modify: `src/server/state.rs` (hold the gate), `src/server/handlers.rs` (check it), `src/server/server.rs` + `src/server/routes.rs` (ConnectInfo for peer IP)
- Create: `tests/conformance_pin.rs`

**Interfaces:**
- Produces: `pin::PinGate::new(pin: Option<String>) -> PinGate`; `PinGate::check(&mut self, provided: Option<&str>, peer: std::net::IpAddr) -> PinVerdict` with `enum PinVerdict { Ok, Unauthorized, LockedOut }`. Constants: `MAX_FAILURES: u32 = 3`, `LOCKOUT: Duration = 5 min`.

- [ ] **Step 1: Write failing unit tests** (bottom of the new `src/server/pin.rs` — write the test module first, with a stub `PinGate` that always returns `Ok` so it compiles, then watch the asserts fail):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    const PEER: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 7));
    const OTHER: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 8));

    #[test]
    fn no_pin_configured_always_ok() {
        let mut g = PinGate::new(None);
        assert_eq!(g.check(None, PEER), PinVerdict::Ok);
        assert_eq!(g.check(Some("anything"), PEER), PinVerdict::Ok);
    }

    #[test]
    fn wrong_or_missing_pin_is_unauthorized() {
        let mut g = PinGate::new(Some("123456".to_string()));
        assert_eq!(g.check(None, PEER), PinVerdict::Unauthorized);
        assert_eq!(g.check(Some("000000"), PEER), PinVerdict::Unauthorized);
        assert_eq!(g.check(Some("123456"), PEER), PinVerdict::Ok);
    }

    #[test]
    fn three_failures_lock_out_that_peer_only() {
        let mut g = PinGate::new(Some("123456".to_string()));
        for _ in 0..3 {
            assert_eq!(g.check(Some("bad"), PEER), PinVerdict::Unauthorized);
        }
        // 4th attempt: locked, even with the right PIN
        assert_eq!(g.check(Some("123456"), PEER), PinVerdict::LockedOut);
        // a different peer is unaffected
        assert_eq!(g.check(Some("123456"), OTHER), PinVerdict::Ok);
    }

    #[test]
    fn success_resets_failure_count() {
        let mut g = PinGate::new(Some("123456".to_string()));
        g.check(Some("bad"), PEER);
        g.check(Some("bad"), PEER);
        assert_eq!(g.check(Some("123456"), PEER), PinVerdict::Ok);
        // counter reset: two more failures don't lock
        g.check(Some("bad"), PEER);
        g.check(Some("bad"), PEER);
        assert_eq!(g.check(Some("123456"), PEER), PinVerdict::Ok);
    }
}
```

- [ ] **Step 2: Run — verify FAIL.** `cargo test --lib server::pin` → assertion failures against the stub.

- [ ] **Step 3: Implement `PinGate`**

```rust
//! Receiver-side PIN enforcement: 401 on mismatch, 3 failures -> 429 + 5 min cooldown
//! (matches official LocalSend app behavior).

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

pub const MAX_FAILURES: u32 = 3;
pub const LOCKOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, PartialEq, Eq)]
pub enum PinVerdict {
    Ok,
    Unauthorized,
    LockedOut,
}

#[derive(Debug)]
pub struct PinGate {
    pin: Option<String>,
    failures: HashMap<IpAddr, (u32, Instant)>, // (count, last_failure)
}

impl PinGate {
    pub fn new(pin: Option<String>) -> Self {
        Self { pin, failures: HashMap::new() }
    }

    pub fn check(&mut self, provided: Option<&str>, peer: IpAddr) -> PinVerdict {
        let Some(expected) = self.pin.as_deref() else {
            return PinVerdict::Ok;
        };

        if let Some((count, at)) = self.failures.get(&peer) {
            if *count >= MAX_FAILURES {
                if at.elapsed() < LOCKOUT {
                    return PinVerdict::LockedOut;
                }
                self.failures.remove(&peer);
            }
        }

        if provided.is_some_and(|p| constant_time_eq(p.as_bytes(), expected.as_bytes())) {
            self.failures.remove(&peer);
            PinVerdict::Ok
        } else {
            let entry = self.failures.entry(peer).or_insert((0, Instant::now()));
            entry.0 += 1;
            entry.1 = Instant::now();
            PinVerdict::Unauthorized
        }
    }
}

/// Length-leaking-free comparison without extra deps.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
```

Add to `src/server/mod.rs`: `pub(crate) mod pin;`

- [ ] **Step 4: Run — verify PASS.** `cargo test --lib server::pin` → 4 passed.

- [ ] **Step 5: Wire into the handler.**
  - `ServerState` gains `pub pin_gate: crate::server::pin::PinGate` (constructed from the builder's `pin`).
  - Peer IP: serve with connect info. HTTP path: `axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>())`; HTTPS path: `.serve(router.into_make_service_with_connect_info::<std::net::SocketAddr>())` on the axum-server builder.
  - `handle_prepare_upload` signature gains `axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>`, and the `_pin` param is renamed `pin` and used, **before** any session/event work:

```rust
    {
        let mut state = state_ref.write().await;
        match state.pin_gate.check(params.pin.as_deref(), peer.ip()) {
            crate::server::pin::PinVerdict::Ok => {}
            crate::server::pin::PinVerdict::Unauthorized => {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            crate::server::pin::PinVerdict::LockedOut => {
                return StatusCode::TOO_MANY_REQUESTS.into_response();
            }
        }
    }
```

- [ ] **Step 6: Write the failing conformance test `tests/conformance_pin.rs`** (raw reqwest, not our client — asserts the wire):

```rust
mod common;

use localsend_rs::server::LocalSendServer;
use localsend_rs::Protocol;
use serde_json::json;

fn minimal_prepare_body() -> serde_json::Value {
    json!({
        "info": {
            "alias": "raw-sender", "version": "2.1", "deviceModel": null,
            "deviceType": "headless", "fingerprint": "raw-fp",
            "port": 53317, "protocol": "http", "download": false
        },
        "files": {
            "f1": { "id": "f1", "fileName": "a.txt", "size": 5,
                    "fileType": "text/plain", "sha256": null, "preview": null, "metadata": null }
        }
    })
}

#[tokio::test]
async fn pin_gate_returns_401_then_429_then_accepts_correct_pin() {
    let save = tempfile::tempdir().unwrap();
    let (server, _events) = LocalSendServer::builder()
        .alias("Pinned")
        .port(0)
        .save_dir(save.path())
        .protocol(Protocol::Http)
        .pin("123456")
        .auto_accept(true)
        .build()
        .await
        .unwrap();
    let port = server.port();
    common::wait_for_http_info(port).await;
    let base = format!("http://127.0.0.1:{port}/api/localsend/v2/prepare-upload");
    let http = reqwest::Client::new();

    // wrong pin -> 401, three times
    for _ in 0..3 {
        let r = http.post(format!("{base}?pin=000000")).json(&minimal_prepare_body()).send().await.unwrap();
        assert_eq!(r.status(), 401);
    }
    // locked out -> 429 even with the right pin
    let r = http.post(format!("{base}?pin=123456")).json(&minimal_prepare_body()).send().await.unwrap();
    assert_eq!(r.status(), 429);
}

#[tokio::test]
async fn correct_pin_passes() {
    let save = tempfile::tempdir().unwrap();
    let (server, _events) = LocalSendServer::builder()
        .alias("Pinned")
        .port(0)
        .save_dir(save.path())
        .protocol(Protocol::Http)
        .pin("123456")
        .auto_accept(true)
        .build()
        .await
        .unwrap();
    let port = server.port();
    common::wait_for_http_info(port).await;
    let base = format!("http://127.0.0.1:{port}/api/localsend/v2/prepare-upload");

    let r = reqwest::Client::new()
        .post(format!("{base}?pin=123456"))
        .json(&minimal_prepare_body())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: serde_json::Value = r.json().await.unwrap();
    assert!(body["sessionId"].is_string());
    assert!(body["files"]["f1"].is_string());
}
```

- [ ] **Step 7: Run — verify PASS** (Steps 3+5 already landed the behavior).

Run: `cargo test --test conformance_pin`
Expected: 2 passed. If `429` comes back as `403`, the PIN check is running after the accept flow — it must be first.

- [ ] **Step 8: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add -A
git commit -m "feat(server): enforce PIN with 401 and 3-fail 429 lockout (R2)"
```

---

## Phase 3 — Correctness sweep + container smoke

### Task 3.1: Real session store — multi-file tracking + random tokens (R5, R6)

**Files:**
- Modify: `src/protocol/types.rs` (`Token::random`, remove `Token::new(session, file)`)
- Modify: `src/core/session.rs` (random tokens; received-tracking)
- Modify: `src/server/state.rs` (use `Session`, delete `ActiveSession`), `src/server/handlers.rs`, `src/server/server.rs` (sweep task)
- Create: `tests/interop_session.rs`

**Interfaces:**
- Consumes: `ServerEvent::{FileReceived, SessionDone}` (2.1), accepted-ids filtering (2.2).
- Produces: `Token::random() -> Token`; `Session` API: `Session::new(sender_alias: String, files: HashMap<FileId, FileMetadata>) -> Self` (now with random tokens), `verify_token(&self, &FileId, &Token) -> bool` (unchanged), `mark_received(&mut self, &FileId) -> bool /* all files done */`, `touch`, `is_timed_out` (unchanged). `ServerState.current_session: Option<Session>`.

- [ ] **Step 1: Write failing tests.**

Add to `src/core/session.rs` tests:

```rust
    #[test]
    fn tokens_are_random_not_derived() {
        let files = create_test_files();
        let s1 = Session::new("A".to_string(), files.clone());
        let s2 = Session::new("A".to_string(), files.clone());
        let id = files.keys().next().unwrap();
        // Different sessions must produce different tokens for the same file id,
        // and a token must not embed the session/file ids.
        let t1 = s1.get_token(id).unwrap().as_str().to_string();
        let t2 = s2.get_token(id).unwrap().as_str().to_string();
        assert_ne!(t1, t2);
        assert!(!t1.contains(id.as_str()));
    }

    #[test]
    fn mark_received_reports_all_done() {
        let mut files = create_test_files();
        let second = FileId::new();
        files.insert(second.clone(), files.values().next().unwrap().clone());
        let ids: Vec<FileId> = files.keys().cloned().collect();
        let mut s = Session::new("A".to_string(), files);
        assert!(!s.mark_received(&ids[0]));
        assert!(s.mark_received(&ids[1]));
    }
```

Create `tests/interop_session.rs`:

```rust
mod common;

use localsend_rs::server::LocalSendServer;
use localsend_rs::{DeviceInfo, LocalSendClient, Protocol, build_file_metadata};
use std::collections::HashMap;

async fn receiver(save: &std::path::Path) -> (LocalSendServer, u16) {
    let (server, _events) = LocalSendServer::builder()
        .alias("R").port(0).save_dir(save).protocol(Protocol::Http)
        .auto_accept(true).build().await.unwrap();
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
        c.upload_file(&target, &prep.session_id, id, token, &paths[id], None).await.unwrap();
    }
    for name in ["one.bin", "two.bin", "three.bin"] {
        let got = localsend_rs::sha256_from_file(save.path().join(name)).await.unwrap();
        assert_eq!(&got, shas.get(name).unwrap());
    }

    // Session must be CLOSED now: a new prepare-upload succeeds (no 409). (R5)
    let (p2, _) = common::make_random_file(src.path(), "again.bin", 128);
    let m2 = build_file_metadata(&p2).await.unwrap();
    let mut f2 = HashMap::new();
    f2.insert(m2.id.clone(), m2);
    let second = c.prepare_upload(&target, f2, None).await;
    assert!(second.is_ok(), "expected new session after completion, got {second:?}");
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
    let err = c.prepare_upload(&target, f2, None).await.expect_err("blocked");
    assert!(matches!(err, localsend_rs::LocalSendError::SessionBlocked));
}
```

- [ ] **Step 2: Run — verify FAIL.**

Run: `cargo test --lib core::session && cargo test --test interop_session`
Expected: `tokens_are_random_not_derived` fails (token contains file id); `multi_file_session_completes_and_frees_the_slot` fails (second prepare gets 409 because a 3-file session never closes).

- [ ] **Step 3: Implement.**
  - `src/protocol/types.rs`: replace `Token::new(session_id, file_id)` with:

```rust
impl Token {
    /// Random per-file upload token (128-bit, hex).
    pub fn random() -> Self {
        Self(uuid::Uuid::new_v4().simple().to_string())
    }
    // from_string / as_str unchanged
}
```

    `grep -rn "Token::new" src tests` and fix every caller (session.rs, handlers.rs; the old deterministic derivation must be gone).
  - `src/core/session.rs`: token map now `files.keys().map(|id| (id.clone(), Token::random())).collect()`; add `received: std::collections::HashSet<FileId>` field and:

```rust
    /// Record a completed file. Returns true when every file has arrived.
    pub fn mark_received(&mut self, file_id: &FileId) -> bool {
        self.received.insert(file_id.clone());
        self.last_activity = Instant::now();
        self.received.len() == self.files.len()
    }
```

    (`is_complete(&[FileId])` is superseded — delete it and its test.)
  - `src/server/state.rs`: `current_session: Option<crate::core::Session>`; delete `ActiveSession`.
  - `handlers.rs` `handle_prepare_upload`: after the accept decision, build the session from the **accepted** files only: `let session = crate::core::Session::new(request.info.alias.clone(), accepted_files);` and respond with `session.tokens.clone()` + `session.id.clone()`.
  - `handlers.rs` `handle_upload`: verify via `session.verify_token(&params.file_id, &params.token)`; after a successful write, `let all_done = session.mark_received(&params.file_id);`; emit `ServerEvent::FileReceived { .. }` (fields from the session + saved path), and if `all_done`, emit `ServerEvent::SessionDone { session_id }` and set `state.current_session = None`. Delete the `files.len() <= 1` heuristic. Events are sent with `let _ = state.events_tx.try_send(ev);` (never block the upload path on a slow consumer; document that intent at the send site).
  - `handle_cancel`: also emit `SessionDone` when it clears a session.
  - `server.rs` `start()`: spawn the TTL sweep alongside the server task:

```rust
        let sweep_state = state.clone();
        let sweep = tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                tick.tick().await;
                let mut s = sweep_state.write().await;
                if let Some(session) = &s.current_session
                    && session.is_timed_out(300)
                {
                    tracing::info!("Sweeping timed-out session {}", session.id);
                    s.current_session = None;
                }
            }
        });
```

    Store the `sweep` handle and abort it in `stop()`.

- [ ] **Step 4: Run — verify PASS.** `cargo test` → all green (unit + both new interop tests + everything prior).

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add -A
git commit -m "fix(server)!: multi-file session tracking with random per-file tokens (R5, R6)"
```

### Task 3.2: `204` for an empty files map (R7)

**Files:**
- Create: `tests/conformance_prepare_upload.rs`
- Modify: `src/server/handlers.rs`

- [ ] **Step 1: Write the failing test**

```rust
mod common;

use localsend_rs::server::LocalSendServer;
use localsend_rs::Protocol;
use serde_json::json;

#[tokio::test]
async fn empty_files_map_returns_204() {
    let save = tempfile::tempdir().unwrap();
    let (server, _events) = LocalSendServer::builder()
        .alias("R").port(0).save_dir(save.path()).protocol(Protocol::Http)
        .auto_accept(true).build().await.unwrap();
    let port = server.port();
    common::wait_for_http_info(port).await;

    let body = json!({
        "info": { "alias": "raw", "version": "2.1", "deviceType": "headless",
                  "fingerprint": "fp", "port": 53317, "protocol": "http", "download": false },
        "files": {}
    });
    let r = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/api/localsend/v2/prepare-upload"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
}
```

- [ ] **Step 2: Run — verify FAIL** (currently 200 with an empty token map). Run: `cargo test --test conformance_prepare_upload`

- [ ] **Step 3: Implement** — first thing in `handle_prepare_upload` after PIN check:

```rust
    if request.files.is_empty() {
        return StatusCode::NO_CONTENT.into_response();
    }
```

(The existing text-message `204` heuristic — all files carry `preview` and are <1 MiB — is spec-conformant ("finished, no transfer needed") and stays; it now sits after the accept decision, unchanged.)

- [ ] **Step 4: Run — verify PASS**, then full suite.
- [ ] **Step 5: Commit:** `git add -A && git commit -m "fix(server): prepare-upload returns 204 when files map is empty (R7)"`

### Task 3.3: Filename collision handling (R8)

**Files:**
- Modify: `src/core/file.rs` (add `unique_save_path`), `src/server/handlers.rs` (use it)
- Modify: `src/core/mod.rs`, `src/lib.rs` (export)

**Interfaces:**
- Produces: `pub fn unique_save_path(save_dir: &Path, file_name: &str) -> crate::Result<PathBuf>` — traversal-safe (delegates to `path_safety::safe_join`) and non-colliding (` (1)`, ` (2)`, … before the extension).

- [ ] **Step 1: Write the failing unit test** (in `src/core/file.rs` tests module — create one if absent):

```rust
    #[test]
    fn unique_save_path_appends_counter_on_collision() {
        let dir = std::env::temp_dir().join(format!("lsrs-col-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = unique_save_path(&dir, "a.txt").unwrap();
        assert_eq!(first, dir.join("a.txt"));
        std::fs::write(&first, "x").unwrap();
        let second = unique_save_path(&dir, "a.txt").unwrap();
        assert_eq!(second, dir.join("a (1).txt"));
        std::fs::write(&second, "y").unwrap();
        let third = unique_save_path(&dir, "a.txt").unwrap();
        assert_eq!(third, dir.join("a (2).txt"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unique_save_path_still_rejects_traversal() {
        let dir = std::env::temp_dir();
        assert!(unique_save_path(&dir, "../evil.txt").is_err());
    }
```

- [ ] **Step 2: Run — verify FAIL.** `cargo test --lib core::file`

- [ ] **Step 3: Implement in `src/core/file.rs`**

```rust
use std::path::{Path, PathBuf};

/// Resolve a collision-free, traversal-safe save path inside `save_dir`.
/// Existing files are never overwritten: "a.txt" -> "a (1).txt" -> "a (2).txt".
pub fn unique_save_path(save_dir: &Path, file_name: &str) -> crate::Result<PathBuf> {
    let candidate = crate::path_safety::safe_join(save_dir, file_name)?;
    if !candidate.exists() {
        return Ok(candidate);
    }
    let stem = candidate
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = candidate
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let parent = candidate.parent().unwrap_or(save_dir).to_path_buf();
    for i in 1u32.. {
        let next = parent.join(format!("{stem} ({i}){ext}"));
        if !next.exists() {
            return Ok(next);
        }
    }
    unreachable!()
}
```

  (`path_safety::safe_join` is `pub(crate)` via `mod path_safety;` in lib.rs — check its error type; if it returns something other than `crate::Result`, map it with the existing `LocalSendError` conversion used at `handlers.rs`' current call-site.)
  Export: `pub use file::{..., unique_save_path};` in `src/core/mod.rs`, add `unique_save_path` to the `pub use core::{...}` list in `src/lib.rs`.

- [ ] **Step 4: Wire into `handle_upload`** — replace the `safe_join` call with `unique_save_path(&state.save_dir, &file_name)`, and use the **returned** (possibly renamed) path for both the write and the `FileReceived { path, file_name }` event (event's `file_name` = the final on-disk name: `path.file_name()`). Same swap at the message-save site in `handle_prepare_upload`.

- [ ] **Step 5: Integration proof** — append to `tests/interop_session.rs`:

```rust
#[tokio::test]
async fn same_filename_twice_keeps_both_copies() {
    let save = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let (_server, port) = receiver(save.path()).await;
    let c = client();
    let target = common::target_device(port);

    for round in 0..2 {
        let sub = src.path().join(format!("r{round}"));
        std::fs::create_dir_all(&sub).unwrap();
        let (p, _) = common::make_random_file(&sub, "dup.bin", 64 + round);
        let m = build_file_metadata(&p).await.unwrap();
        let id = m.id.clone();
        let mut f = HashMap::new();
        f.insert(id.clone(), m);
        let prep = c.prepare_upload(&target, f, None).await.unwrap();
        let token = prep.files.get(&id).unwrap().clone();
        c.upload_file(&target, &prep.session_id, &id, &token, &p, None).await.unwrap();
    }
    assert!(save.path().join("dup.bin").exists());
    assert!(save.path().join("dup (1).bin").exists());
}
```

- [ ] **Step 6: Run — verify PASS**, full suite. Commit:

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add -A
git commit -m "fix(server): never overwrite on filename collision (R8)"
```

### Task 3.4: Serde leniency for spec-minimal peers (R9)

**Files:**
- Create: `tests/unit_schema.rs`
- Modify: `src/protocol/types.rs`

- [ ] **Step 1: Write the failing test `tests/unit_schema.rs`**

```rust
use localsend_rs::{AnnouncementMessage, DeviceInfo};

#[test]
fn device_info_accepts_spec_minimal_payload() {
    // No download, no deviceModel, no ip — all valid per spec.
    let json = r#"{
        "alias": "Phone", "version": "2.1", "deviceType": "mobile",
        "fingerprint": "abc", "port": 53317, "protocol": "http"
    }"#;
    let d: DeviceInfo = serde_json::from_str(json).expect("minimal payload must parse");
    assert!(!d.download);
    assert_eq!(d.port, 53317);
}

#[test]
fn device_info_tolerates_missing_port_and_protocol() {
    // Some peers omit port/protocol in prepare-upload's embedded info.
    let json = r#"{ "alias": "Phone", "version": "2.1", "fingerprint": "abc" }"#;
    let d: DeviceInfo = serde_json::from_str(json).expect("must parse");
    assert_eq!(d.port, 53317);       // defaulted
    assert_eq!(d.protocol.as_str(), "http"); // defaulted
}

#[test]
fn announcement_accepts_minimal_payload() {
    let json = r#"{
        "alias": "Phone", "version": "2.1", "fingerprint": "abc",
        "port": 53317, "protocol": "http", "announce": true
    }"#;
    let a: AnnouncementMessage = serde_json::from_str(json).expect("must parse");
    assert!(a.announce);
    assert!(!a.download);
}
```

- [ ] **Step 2: Run — verify FAIL** (missing `download` fails today). Run: `cargo test --test unit_schema`

- [ ] **Step 3: Implement** in `src/protocol/types.rs` — `DeviceInfo` becomes:

```rust
fn default_port() -> u16 {
    crate::protocol::constants::DEFAULT_HTTP_PORT
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub alias: String,
    pub version: String,
    #[serde(rename = "deviceModel", default)]
    pub device_model: Option<String>,
    #[serde(rename = "deviceType", default)]
    pub device_type: Option<DeviceType>,
    pub fingerprint: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "Protocol::default")]
    pub protocol: Protocol,
    #[serde(default)]
    pub download: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
}
```

  Add `impl Default for Protocol { fn default() -> Self { Protocol::Http } }` (or `#[derive(Default)]` + `#[default]` on `Http`). Give `AnnouncementMessage` the same `#[serde(default)]` treatment on `device_model`, `device_type`, `download` (keep `port`/`protocol` required there — announcements without a port are useless; `announce`/`announcement` already default).

- [ ] **Step 4: Run — verify PASS**, full suite (serialization output is unchanged, so no interop test moves).
- [ ] **Step 5: Commit:** `git add -A && git commit -m "fix(protocol): serde defaults so spec-minimal peers deserialize (R9)"`

### Task 3.5: Client `cancel` + Content-Length + real upload progress (R10-partial, R11)

**Files:**
- Modify: `src/client/client.rs`
- Modify: `tests/interop_session.rs` (cancel + progress tests)

**Interfaces:**
- Produces: `LocalSendClient::cancel(&self, target: &DeviceInfo, session_id: &SessionId) -> Result<()>`; `upload_file` unchanged signature but now sets `Content-Length` and invokes the progress callback per chunk `(bytes_sent, total, elapsed_secs)`.

- [ ] **Step 1: Write the failing tests** (append to `tests/interop_session.rs`):

```rust
#[tokio::test]
async fn cancel_frees_the_session_slot() {
    let save = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let (_server, port) = receiver(save.path()).await;
    let c = client();
    let target = common::target_device(port);

    let (p, _) = common::make_random_file(src.path(), "c.bin", 128);
    let m = build_file_metadata(&p).await.unwrap();
    let mut f = HashMap::new();
    f.insert(m.id.clone(), m);
    let prep = c.prepare_upload(&target, f, None).await.unwrap();

    c.cancel(&target, &prep.session_id).await.expect("cancel ok");

    // Slot is free again
    let (p2, _) = common::make_random_file(src.path(), "c2.bin", 128);
    let m2 = build_file_metadata(&p2).await.unwrap();
    let mut f2 = HashMap::new();
    f2.insert(m2.id.clone(), m2);
    assert!(c.prepare_upload(&target, f2, None).await.is_ok());
}

#[tokio::test]
async fn upload_reports_monotonic_progress_up_to_total() {
    let save = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let (_server, port) = receiver(save.path()).await;
    let c = client();
    let target = common::target_device(port);

    const SIZE: usize = 4 * 1024 * 1024; // several chunks
    let (p, _) = common::make_random_file(src.path(), "p.bin", SIZE);
    let m = build_file_metadata(&p).await.unwrap();
    let id = m.id.clone();
    let mut f = HashMap::new();
    f.insert(id.clone(), m);
    let prep = c.prepare_upload(&target, f, None).await.unwrap();
    let token = prep.files.get(&id).unwrap().clone();

    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(u64, u64)>::new()));
    let seen_cb = seen.clone();
    c.upload_file(
        &target, &prep.session_id, &id, &token, &p,
        Some(Box::new(move |sent, total, _elapsed| {
            seen_cb.lock().unwrap().push((sent, total));
        })),
    )
    .await
    .unwrap();

    let seen = seen.lock().unwrap();
    assert!(seen.len() >= 2, "expected multiple progress callbacks, got {}", seen.len());
    assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0), "progress must be monotonic");
    assert_eq!(seen.last().unwrap().0, SIZE as u64);
    assert!(seen.iter().all(|(_, t)| *t == SIZE as u64));
}
```

- [ ] **Step 2: Run — verify FAIL** (`cancel` missing; progress fires once at 0). Run: `cargo test --test interop_session`

- [ ] **Step 3: Implement in `src/client/client.rs`.**

```rust
    pub async fn cancel(&self, target: &DeviceInfo, session_id: &SessionId) -> Result<()> {
        let ip = target
            .ip
            .as_ref()
            .ok_or_else(|| LocalSendError::network("Target IP not provided"))?;
        let url = format!(
            "{}://{}:{}/api/localsend/v2/cancel?sessionId={}",
            target.protocol, ip, target.port, session_id
        );
        let response = self.client.post(&url).send().await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(LocalSendError::http_failed(response.status().as_u16(), "Cancel failed"))
        }
    }
```

  Rework `upload_file`'s body construction (replacing the plain `ReaderStream` + start-only callback):

```rust
        let file = File::open(file_path).await?;
        let total_bytes = file.metadata().await?.len();
        let started = std::time::Instant::now();
        let progress = progress.map(std::sync::Arc::new);

        let counter_progress = progress.clone();
        let mut sent: u64 = 0;
        let counted = ReaderStream::new(file).inspect(move |chunk| {
            if let (Ok(c), Some(cb)) = (chunk, counter_progress.as_ref()) {
                sent += c.len() as u64;
                cb(sent, total_bytes, started.elapsed().as_secs_f64());
            }
        });
        let body = Body::wrap_stream(counted);

        let response = self
            .client
            .post(&url)
            .header(reqwest::header::CONTENT_LENGTH, total_bytes)
            .body(body)
            .send()
            .await?;
```

  `inspect` comes from `futures_util::StreamExt` — add `use futures_util::StreamExt;` (futures-util is already a dependency; if it isn't visible to the client module, add it to the imports). The old "report 0 at start" call is deleted; the callback type stays `Box<dyn Fn(u64, u64, f64) + Send + Sync>` wrapped in `Arc` internally.

- [ ] **Step 4: Run — verify PASS**, full suite.
- [ ] **Step 5: Commit:** `git add -A && git commit -m "feat(client): cancel endpoint, Content-Length, per-chunk progress (R10, R11)"`

### Task 3.6: Container e2e scaffold + `send-direct` scenario (Layer 4 smoke)

**Files:**
- Create: `e2e/docker/localsend-rs.Dockerfile`
- Create: `e2e/docker-compose.yml`
- Create: `e2e/scripts/receiver.sh`, `e2e/scripts/sender.sh`
- Create: `e2e/run.sh`
- Modify: `README.md` (short "E2E" section)

**Interfaces:**
- Consumes: working headless `receive --auto-accept` (Task 2.4) and `send <ip> <file>` CLI.
- Produces: `./e2e/run.sh` exits 0 on a byte-identical container-to-container transfer; the compose file is the base later scenarios (https/pin/discovery/oracle/ts) extend.

- [ ] **Step 1: Write `e2e/docker/localsend-rs.Dockerfile`**

```dockerfile
# Build
FROM rust:1-slim AS build
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --features cli,https

# Runtime
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/localsend-rs /usr/local/bin/localsend-rs
```

  (If the workspace has a `Cargo.lock` gitignored, drop the `Cargo.lock` COPY line and `--locked` stays out. Verify: `ls Cargo.lock`.)

- [ ] **Step 2: Write `e2e/scripts/receiver.sh`**

```sh
#!/bin/sh
set -eu
mkdir -p /shared/received
exec localsend-rs receive --directory /shared/received --port 53317 --auto-accept
```

- [ ] **Step 3: Write `e2e/scripts/sender.sh`**

```sh
#!/bin/sh
set -eu

SIZE_KB="${PAYLOAD_KB:-1024}"
mkdir -p /work
dd if=/dev/urandom of=/work/payload.bin bs=1024 count="$SIZE_KB" 2>/dev/null
WANT=$(sha256sum /work/payload.bin | cut -d' ' -f1)

echo "waiting for receiver..."
i=0
until curl -fsS "http://receiver:53317/api/localsend/v2/info" >/dev/null 2>&1; do
  i=$((i + 1))
  [ "$i" -ge 60 ] && { echo "receiver never became ready"; exit 1; }
  sleep 1
done

RECEIVER_IP=$(getent hosts receiver | awk '{print $1}' | head -n1)
echo "sending to $RECEIVER_IP"
localsend-rs send "$RECEIVER_IP" /work/payload.bin

i=0
until [ -f /shared/received/payload.bin ]; do
  i=$((i + 1))
  [ "$i" -ge 30 ] && { echo "file never arrived"; ls -la /shared/received; exit 1; }
  sleep 1
done
GOT=$(sha256sum /shared/received/payload.bin | cut -d' ' -f1)

if [ "$WANT" = "$GOT" ]; then
  echo "E2E-PASS send-direct ($SIZE_KB KB, sha256 match)"
else
  echo "E2E-FAIL sha mismatch: want=$WANT got=$GOT"
  exit 1
fi
```

- [ ] **Step 4: Write `e2e/docker-compose.yml`**

```yaml
services:
  receiver:
    image: localsend-rs-e2e
    build:
      context: ..
      dockerfile: e2e/docker/localsend-rs.Dockerfile
    volumes:
      - shared:/shared
      - ./scripts:/e2e:ro
    command: ["/bin/sh", "/e2e/receiver.sh"]
    healthcheck:
      test: ["CMD", "curl", "-fsS", "http://localhost:53317/api/localsend/v2/info"]
      interval: 2s
      timeout: 2s
      retries: 30

  sender:
    image: localsend-rs-e2e
    build:
      context: ..
      dockerfile: e2e/docker/localsend-rs.Dockerfile
    environment:
      PAYLOAD_KB: "10240"   # 10 MB
    volumes:
      - shared:/shared
      - ./scripts:/e2e:ro
    depends_on:
      receiver:
        condition: service_healthy
    command: ["/bin/sh", "/e2e/sender.sh"]

volumes:
  shared:
```

- [ ] **Step 5: Write `e2e/run.sh`**

```sh
#!/bin/sh
# Container e2e for localsend-rs. Requires Docker.
set -eu
cd "$(dirname "$0")"
docker compose down -v --remove-orphans >/dev/null 2>&1 || true
docker compose up --build --abort-on-container-exit --exit-code-from sender
status=$?
docker compose down -v --remove-orphans >/dev/null 2>&1 || true
exit $status
```

`chmod +x e2e/run.sh e2e/scripts/*.sh`

- [ ] **Step 6: Run — verify E2E-PASS**

Run: `./e2e/run.sh`
Expected: sender logs `E2E-PASS send-direct (10240 KB, sha256 match)`, exit code 0.
Debugging notes if it fails: `send <ip>` probes HTTPS first then HTTP on port **53317** (`src/cli/commands/send.rs:203-232`) — the receiver runs HTTP on 53317, so the fallback path must connect; if `getent` is missing in the runtime image, replace with `nslookup receiver | awk '/^Address: / {print $2}' | head -n1` or add `busybox-extras`.

- [ ] **Step 7: README + commit**

Add to `README.md`:

```markdown
## Testing

- `cargo test` — unit + conformance + in-process interop suites (no Docker needed).
- `./e2e/run.sh` — containerized sender→receiver e2e over the compose network (requires Docker).
```

```bash
git add e2e README.md
git commit -m "test(e2e): containerized rs->rs send-direct scenario with sha256 assertion"
```

### Task 3.7: Phase wrap-up — spec checkboxes + structure notes

**Files:**
- Modify: `docs/superpowers/specs/2026-07-12-localsend-rs-v2.1-alignment-design.md` (tick Phases 0–3)
- Modify: `README.md` (module map paragraph)

- [ ] **Step 1:** Full sweep: `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test && ./e2e/run.sh` — everything green.
- [ ] **Step 2:** Tick the Phase 0–3 checkboxes in the spec §7. Add a short "Architecture" note to `README.md` describing `src/server/{server,routes,handlers,state,events,pin}.rs` and the event-stream receive API with a 10-line usage example (copy the builder example from spec §5.2).
- [ ] **Step 3:** Commit: `git add -A && git commit -m "docs: mark v2.1 alignment phases 0-3 complete; document event-stream API"`

---

## Deferred to follow-on plans

- **Phase 4 — Download API (G1):** `prepare-download` + `download` handlers, download session store, staged files (`server.share(paths)`), minimal `GET /` page, client `prepare_download`/`download`, `download` e2e scenario, optional `POST /show` event hook (spec Q2).
- **Phase 5 — HTTPS identity & pinning (R3, R4):** cert-derived fingerprint in `MulticastDiscovery` + all remaining constructors, rustls `ServerCertVerifier` fingerprint pinning behind `TlsTrustPolicy`, `https` e2e scenario, fingerprint-format assertion (spec Q1).
- **Phase 6 — Oracle + full e2e matrix:** `tools/oracle` bin over the official `localsend` core crate (git dep, pinned rev), oracle compose service + scenarios, `discovery` (multicast) and `pin` scenarios, CI wiring (fast job / e2e job / skippable oracle job), pluggable peer service for future `localsend-ts`.
- **Phase 7 — Docs & polish:** README overhaul, manual LocalSend-1.17.0 GUI checklist, periodic re-announce (G2).

Each gets its own `docs/superpowers/plans/` file when Phase 3 lands.

---

## Self-Review Notes

- **Spec coverage (Phases 0–3):** harness (§6.1–6.3, §6.4-smoke) → Tasks 0.1/3.6 ✓; refactor + dead code (D2–D4) → 1.1–1.3 ✓ (D5 version-validation folded into Phase-4+ follow-up — serde leniency in 3.4 covers the practical need); R1 events/builder/CLI/TUI → 2.1–2.5 ✓; R2 PIN+lockout → 2.6 ✓; R5/R6 sessions/tokens → 3.1 ✓; R7 204 → 3.2 ✓; R8 collisions → 3.3 ✓; R9 schema → 3.4 ✓; R10-partial (cancel) + R11 progress → 3.5 ✓. G1/R3/R4/G2/oracle intentionally deferred (spec §7 phases 4–7).
- **Known sequencing hack:** Task 0.1's rendezvous poke and the `new_with_device` params survive only until 2.4/2.5, where they are deleted along with `PendingTransfer`.
- **Type consistency check:** `ServerEvent::{TransferRequest, FileReceived, SessionDone}` used identically in 2.1/2.2/2.4/2.5/3.1; `PendingRequest::{sender, files, accept, accept_files, decline}` consistent in 2.1/2.2/2.4/2.5; builder methods consistent in 2.3/2.4/2.5/3.x tests; `Token::random` consistent in 3.1; `unique_save_path` consistent in 3.3.
