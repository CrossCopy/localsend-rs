# localsend-rs — Agent Guide

Rust implementation of the **LocalSend v2.1** protocol, consumed as a **library** by CrossCopy
(`apps/xc`, and later the mobile FFI cores). CLI and TUI are thin shells over the library.

> `CLAUDE.md` is a symlink to this file. Keep this the single source of truth.

## Status (what works / what doesn't)

**Implemented — the receive/upload half + library API:**
- v2 endpoints: `GET /info`, `POST /register`, `POST /prepare-upload`, `POST /upload`, `POST /cancel`.
- **Library-first receive**: `LocalSendServer::builder()` returns `(server, mpsc::Receiver<ServerEvent>)`;
  consumers accept/decline programmatically — no TUI required (see API below).
- PIN enforcement (401 + 3-fail→429 lockout), multi-file sessions with random per-file tokens + TTL sweep,
  path-traversal-safe + collision-safe saves, spec-lenient deserialization, whole-file streaming to disk.
- Client: `register`, `prepare_upload`, `upload_file` (one POST, `Content-Length`, per-chunk progress), `cancel`.
- Discovery: UDP multicast (`224.0.0.167:53317`) announce/respond + HTTP-register fallback; subnet scan (`HttpDiscovery::scan_subnet`).
- HTTPS server via self-signed cert; `DeviceInfo.fingerprint = SHA-256(cert DER)` when HTTPS.

**Not yet implemented (deferred — see the plan doc):**
- Download / reverse mode: `prepare-download`, `download`, `GET /` browser share page (**G1**).
- True client-side cert **pinning** (`TlsTrustPolicy` currently only toggles `danger_accept_invalid_certs`) (**R3/R4**).
- Official-core **oracle** cross-impl tests, and https/pin/download/discovery e2e container scenarios.
- WebRTC / internet transfer (lives in `apps/xc`, not this crate).

## Build & Test

Everything runs from this crate's root. This crate has **no `[workspace]` table** — it builds standalone.

```bash
# Fast suite: unit + conformance + in-process interop (no Docker). This is the primary gate.
cargo test                     # 48 tests: src/ unit tests + tests/*.rs integration binaries

# Lint & format — MUST be clean on every commit.
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check              # run `cargo fmt` before committing

# Feature-matrix sanity (https on/off; the binary needs the cli feature).
cargo build --features tui
cargo build --features https
cargo build --no-default-features --lib

# Containerized end-to-end (requires Docker): rs→rs send-direct, sha256-asserted.
./e2e/run.sh                   # prints "E2E-PASS send-direct (…, sha256 match)", exit 0
```

**Test layers (all in this repo):**
- `src/**/*.rs` `#[cfg(test)]` — unit tests (session store, PIN gate, path safety, files, schema).
- `tests/conformance_*.rs` — drive the real server with **raw reqwest** to assert the wire (status codes, payload shapes) against the spec, not against our own client.
- `tests/interop_*.rs` — real `LocalSendClient` ↔ real `LocalSendServer` over localhost (byte-identical sha256, multi-file, accept/decline, cancel, progress, message path).
- `e2e/` — docker-compose sender+receiver containers over a bridge network; `e2e/scripts/*.sh` assert sha256 across a shared volume. Base for future scenarios (https/pin/download/discovery/oracle).

**TDD is the workflow.** Write the failing test first; every behavior change ships with a test.

## Library receive API (the important pattern)

```rust
let (mut server, mut events) = LocalSendServer::builder()
    .alias("My Device")
    .port(0)                       // 0 = ephemeral; server.port() reports the real one
    .save_dir("/downloads")
    .protocol(Protocol::Http)      // Https auto-generates cert + cert-derived fingerprint
    .pin("123456")                 // optional
    .auto_accept(false)            // true = quick-save; false = answer each request
    .build().await?;               // server is LISTENING on return

while let Some(ev) = events.recv().await {
    match ev {
        ServerEvent::TransferRequest(req) => { req.accept(); /* or req.decline() / req.accept_files(ids) */ }
        ServerEvent::FileReceived { path, .. } => { /* a file (or text message) landed */ }
        ServerEvent::SessionDone { .. } => {}
    }
}
```
Dropping the `PendingRequest` or letting the accept-timeout elapse declines the transfer (403). The CLI
(`src/cli/commands/receive.rs`) and TUI (`src/tui/app.rs`) are both just consumers of this event stream.

## Architecture / where to look

| Concern | Location |
|---|---|
| Binary entry / CLI dispatch | `src/main.rs`, `src/cli/` (clap) |
| Server type + builder + start/stop + TTL sweep | `src/server/server.rs` |
| Axum router | `src/server/routes.rs` |
| Request handlers (info/register/prepare-upload/upload/cancel) | `src/server/handlers.rs` |
| Shared server state (`ServerState`, session, save-to-disk) | `src/server/state.rs` |
| Event stream + accept/decline handle | `src/server/events.rs` |
| PIN gate (401/429 lockout) | `src/server/pin.rs` |
| Session store (random tokens, multi-file tracking) | `src/core/session.rs` |
| Save-path resolution (traversal-safe, collision-safe) | `src/core/file.rs` (`unique_save_path`) |
| HTTP client | `src/client/client.rs`, `src/client/trust_policy.rs` |
| Protocol DTOs / newtypes / serde | `src/protocol/types.rs` |
| Constants (version, ports, multicast, API paths) | `src/protocol/constants.rs` |
| Fingerprint / sha256 / TLS cert | `src/crypto/{fingerprint,hash,tls}.rs` |
| Discovery | `src/discovery/{multicast,http,traits}.rs` |
| Path-traversal guard | `src/path_safety.rs` |

## Conventions

- **Protocol**: v2.1, routes under `/api/localsend/v2/`. Upload = the **entire file in ONE POST** — never invent `Content-Range`/chunking headers. Multicast `224.0.0.167:53317`, default port `53317`.
- **Transport default is HTTPS**, matching the official LocalSend app. Both `tui` and `receive` default to HTTPS with a `--no-https` opt-out (the e2e keeps `--no-https` so the http healthcheck works). `send` probes HTTPS-then-HTTP and adapts to the peer. Targets accept `host:port` (see `split_host_port` in `send.rs`); a bare host uses `DEFAULT_HTTP_PORT`.
- **Constants only**: never hardcode ports/addresses — use `src/protocol/constants.rs`.
- **Feature gates**: `default = ["cli","https"]`; guard https-only code with `#[cfg(feature = "https")]` so `--no-default-features --lib` still builds.
- **Errors**: `thiserror` (`LocalSendError`) in the library; `anyhow` in CLI/bin.
- **Concurrency rule**: `ServerState` is `Arc<RwLock<…>>`. NEVER hold the write guard across an `.await` that waits on the network or the accept-decision channel — clone what you need out of a short lock scope, drop the guard, then await. Emit events with `try_send` (non-blocking) so a slow consumer can't stall an upload.
- **Serde is the wire contract**: `src/protocol/types.rs` is the source of truth. Deserialization is lenient (defaults for optional fields) but serialized OUTPUT must stay stable — don't add `skip_serializing_if` beyond `ip`.
- **Pre-1.0**: breaking the public API is fine; the only consumer is `apps/xc` (compiled separately in the superproject). When the API changes, the superproject's `vendors/localsend-rs` gitlink must be bumped.

## Anti-patterns

- **Don't hold the state lock across an await** (see concurrency rule) — it deadlocks/serializes uploads.
- **Don't derive upload tokens from ids** — `Token::random()` only (guessable tokens were a security bug).
- **Don't overwrite on save** — always go through `unique_save_path` (traversal-safe + collision-numbered).
- **Don't reintroduce a "clear session after 1 file" heuristic** — multi-file sessions close on all-files-received or TTL only.
- **Don't add reviewer-facing dead code** — the crate is warning-clean under `-D warnings`.

## Reference docs

- Design spec (rationale, issue IDs R1–R11 / D1–D5 / G1–G2): `docs/superpowers/specs/2026-07-12-localsend-rs-v2.1-alignment-design.md`
- Implementation plan (Phases 0–3, done): `docs/superpowers/plans/2026-07-12-localsend-rs-v2.1-foundation.md`
- Protocol spec & official impls: `../../references/localsend-protocol`, `../../references/localsend/core` (official Rust core — client side is complete; useful as an interop oracle).
- Sibling TS impl: `../localsend-ts`.
