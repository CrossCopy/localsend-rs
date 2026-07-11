# LocalSend-RS: v2.1 Alignment, Library-First API & Container Test Harness — Design

**Date:** 2026-07-12
**Status:** Draft for review
**Owner:** Huakun Shen
**Repo:** `localsend-rs` (v0.1.2, pre-1.0 — breaking changes allowed)
**Sibling spec:** `localsend-ts` — `docs/superpowers/specs/2026-07-12-localsend-v2.1-completion-and-test-harness-design.md` (same goals, TS side)

---

## 1. Purpose & Goals

`localsend-rs` is CrossCopy's Rust implementation of the LocalSend protocol, consumed as a
**library** by `apps/xc` (and, later, the mobile FFI cores). A verification pass (2026-07-12)
found a solid v2 *receive-upload* core (streaming to disk, path-traversal protection, multicast
discovery, self-signed HTTPS) but with **the library accept path broken** (only the TUI can
accept a transfer — headless/library receive always times out to 403), several security and
conformance bugs, the entire download half of the protocol missing, and no integration tests.

**Priority decision: library consumers come first.** The public library API is the product;
CLI and TUI are thin shells over it.

**Goals**

1. Reach **full LocalSend v2.1 LAN compatibility** with the official app (v1.17.0 as reference).
2. A **library-first receive API**: programmatic accept/reject/progress, no TUI required.
3. **Refactor** into a testable core; delete or wire the dead parallel abstractions.
4. Build an **automated test harness** — in-process `cargo test` layers plus a
   **containerized e2e suite** (docker-compose, mirroring CrossCopy's `e2e/` conventions).
   All tests live in **this repo**.
5. Fix all correctness/security bugs found in verification.

**Success criteria**

- All v2.1 endpoints implemented and spec-conformant: `info`, `register`, `prepare-upload`,
  `upload`, `cancel`, `prepare-download`, `download`, `GET /`.
- Works over both **HTTP and HTTPS**; HTTPS identity fingerprint = SHA-256 of the cert DER.
- A library consumer can receive files headlessly with a ~10-line event loop.
- A file sent from `localsend-rs` arrives byte-identical at the official Rust `core` crate
  (oracle) and vice versa, over HTTP and HTTPS, in containers.
- `cargo test` green locally; container e2e green in CI.

---

## 2. Non-Goals (explicitly out of scope)

- **Protocol v3** (nonce handshake, signed tokens, cert pairing) — unpublished/experimental.
- **WebRTC / internet transfer** — per the CrossCopy compatibility decision (2026-07-11), the
  WebRTC stack is ported into `apps/xc`, not into this crate.
- **v1 routes** (`/api/localsend/v1/*`) — modern clients speak v2.
- Porting the official `assets/web/` browser UI — a minimal functional share page suffices
  (matches the TS-side decision).
- CrossCopy-specific superset features (Iroh, clipboard sync, rendezvous) — live in `apps/xc`.

---

## 3. Protocol Reference (v2.1) — source of truth

Identical to the TS-side spec §3 (multicast `224.0.0.167:53317`, default port `53317`,
upload = **whole file in ONE POST**, status-code tables, data shapes, spec leniency rules).
Reference copies: `references/localsend-protocol` and `references/localsend/core` in the
CrossCopy superproject. Do not duplicate here; conformance tests encode the tables.

Key rules this repo currently violates or must honor:

- `prepare-upload`: `204` when there is **nothing to send** (empty files map); `401` invalid PIN;
  `403` rejected; `409` blocked by another session; `429` too many requests.
- Fingerprint: **HTTPS** = SHA-256 of the TLS certificate (DER); **HTTP** = random string.
- `DeviceInfo.download` and friends are optional-with-default when parsing peers.
- Official app PIN behavior: 3 failed attempts → `429` + cooldown.

---

## 4. Current State Assessment

### Works

- Multicast discovery (announce + respond, HTTP-register fallback) — `src/discovery/multicast.rs`
- `info`, `register`, `prepare-upload`, `upload`, `cancel` (axum) — `src/server/server.rs:249-560`
- Upload streams to disk (not buffered) — `server.rs:65-78`
- Path-traversal protection wired into upload — `src/path_safety.rs`, `server.rs:478`
- Self-signed HTTPS server (rcgen + axum-server); cert SHA-256 helper exists — `src/crypto/tls.rs`
- Client status-code mapping is already complete (401/403/409/429) — `src/client/client.rs:111-134`

### Missing / Broken (must fix)

| #   | Issue | Location | Severity |
|-----|-------|----------|----------|
| R1  | **Headless/library accept broken**: accept rendezvous (`PendingTransfer.response_tx`) is only drained by the TUI; CLI `receive` and library consumers block 60 s → 403. No public accept/reject API. | `server/server.rs:29-33`, `tui/app.rs:207-221`, `cli/commands/receive.rs:108` | **Critical** |
| R2  | **PIN never checked server-side** (`_pin` parsed and discarded); no `401`, no 3-fail→`429` lockout | `server/server.rs:277-285` | Security |
| R3  | **Identity fingerprint is a random UUID even in HTTPS mode** (only the CLI receive path threads the cert fp); advertised fingerprint ≠ TLS cert hash | `crypto/fingerprint.rs:2`, `server/server.rs:106`, `discovery/multicast.rs:37` | Correctness |
| R4  | **Cert pinning decorative**: `TlsTrustPolicy` collapses to `danger_accept_invalid_certs(bool)`; strict mode rejects all self-signed peers instead of pinning | `client/client.rs:33-42`, `client/trust_policy.rs` | Security |
| R5  | **Multi-file session lifecycle broken**: single `Option<ActiveSession>`, cleared after 1 file only when `files.len() <= 1`; multi-file sessions end only by 300 s timeout | `server/server.rs:56,529-535` | Correctness |
| R6  | **File tokens deterministic/guessable** (`{sessionId}_{fileId}`) | `protocol/types.rs:119-121` | Security |
| R7  | **`204` misused**: returned for a text-message heuristic; empty files map returns `200` | `server/server.rs:294-298,376-418` | Conformance |
| R8  | **No filename collision handling** — silent overwrite | `server/server.rs:66` | Correctness |
| R9  | **Schema brittle**: `DeviceInfo.download`/`port` required with no serde default → spec-minimal peers fail to deserialize | `protocol/types.rs:190-204` | Conformance |
| R10 | Client missing `cancel`, `prepare-download`, `download` | `client/client.rs` | Feature gap |
| R11 | Progress callback fires only at 0% (TODO left in code) | `client/client.rs:163-167` | Minor |
| G1  | **Download API absent**: `prepare-download`, `download`, `GET /` share page | — | Feature gap |
| G2  | No periodic re-announce; announce is a one-shot 3-burst | `discovery/multicast.rs:187-191` | Minor |

### Dead / unwired code (structure debt)

| # | Item | Disposition (decided) |
|---|------|------------------------|
| D1 | `core/session.rs` `Session` (rich token-map type, unused by server) | **Wire it** — becomes the real session store (fixes R5/R6 with it) |
| D2 | `core/transfer.rs` `TransferState` state machine (unused) | **Delete** — server events (§5.2) supersede it |
| D3 | `storage/` `FileSystem`/`TokioFileSystem` traits (server calls `tokio::fs` directly) | **Delete** — tests use real temp dirs; abstraction earns nothing |
| D4 | `discovery/http.rs` — implemented subnet scan behind `#![allow(dead_code)]`, `start()` is an idle timer | **Wire the scan** as a public discovery method; delete the idle loop |
| D5 | `protocol/validation.rs` — validators never invoked | **Wire** version check into register/prepare-upload handlers (major-only, lenient) or fold into serde |

---

## 5. Target Architecture

**Principle:** the library API is the product. Protocol logic lives in framework-thin handlers
over an explicit session store; the axum router, CLI, and TUI are shells. Every protocol behavior
is testable in-process; cross-process behavior is proven in containers.

```
src/
  protocol/
    constants.rs      # version "2.1", ports, multicast addr, API paths
    types.rs          # wire structs — serde-lenient per spec (R9)
    validation.rs     # version check, wired into handlers (D5)
  crypto/
    fingerprint.rs    # http: random; https: SHA-256(cert DER)  (R3)
    hash.rs  tls.rs   # (existing; tls stays behind `https` feature)
  core/
    session.rs        # SessionStore: multi-file token map, random tokens, TTL,
                      # 409 single-active-session, lazy+background cleanup (R5, R6)
    files.rs          # save-path resolution (path_safety), collision " (1)" renaming (R8),
                      # metadata/mime builders (existing core/file.rs content)
    device.rs  builders.rs   # (existing)
  server/
    mod.rs            # LocalSendServer: builder, start/stop, http+https
    routes.rs         # axum Router construction only
    handlers.rs       # info/register/prepare-upload/upload/cancel/
                      # prepare-download/download/index — thin, testable
    events.rs         # ServerEvent + PendingRequest (accept/decline handle)  (R1)
    pin.rs            # PIN check + 3-fail→429 lockout with cooldown  (R2)
    web.rs            # minimal GET / share page  (G1)
  client/
    client.rs         # register/prepare-upload/upload/cancel/
                      # prepare-download/download; Content-Length streaming; real progress (R10, R11)
    verifier.rs       # rustls cert verifier pinning an expected SHA-256 fingerprint (R4)
    trust_policy.rs   # kept as public config type; now actually enforced
  discovery/          # multicast.rs (+ optional periodic re-announce), http.rs (wired scan)
  path_safety.rs      # (existing, kept)
  cli/  tui/          # thin shells consuming ServerEvent (R1 fixes CLI receive for free)
tests/                # integration tests (cargo test): conformance_*.rs, interop_*.rs
  helpers/            # temp dirs, free ports, random files, sha256
e2e/                  # container harness (docker-compose; mirrors CrossCopy e2e/ conventions)
  docker-compose.yml  # sender / receiver / oracle (/ future ts) services
  docker/localsend-rs.Dockerfile
  docker/oracle.Dockerfile      # builds tools/oracle against official core crate
  scripts/*.sh        # per-scenario drivers; sha256 assertions on shared volumes
tools/
  oracle/             # tiny bin wrapping the OFFICIAL `localsend` core crate
                      # (git dep on github.com/localsend/localsend, package "localsend",
                      #  pinned rev); subcommands: serve | send | download
```

### 5.1 Deleted

`core/transfer.rs`, `storage/` (whole module), the idle-timer body of `discovery/http.rs`'s
`start()`. The old TUI-only rendezvous (`PendingTransfer`/`Arc<RwLock<Option<...>>>` in
`server.rs:29-56`) is replaced by `server/events.rs`.

### 5.2 Library receive API (the R1 fix) — core design

Event-stream + responder handle. One obvious way to receive, usable headless:

```rust
let (server, mut events) = LocalSendServer::builder()
    .alias("My Device")
    .save_dir("/downloads")
    .protocol(Protocol::Https)      // fingerprint auto-derived from cert (R3)
    .pin("123456")                  // optional (R2)
    .build()
    .await?;                        // starts listening; also returns bound port

while let Some(ev) = events.recv().await {
    match ev {
        ServerEvent::TransferRequest(req) => {
            // req.sender(): &DeviceInfo, req.files(): &BTreeMap<FileId, FileMetadata>
            req.accept().await?;                    // or req.decline(), req.accept_files(subset)
        }
        ServerEvent::FileProgress { file_id, received, total, .. } => { /* ... */ }
        ServerEvent::FileReceived { path, .. } => { /* ... */ }
        ServerEvent::SessionDone { session_id } => { /* ... */ }
    }
}
```

- `events` is a `tokio::sync::mpsc::Receiver<ServerEvent>`; `PendingRequest` wraps a oneshot
  responder (same rendezvous as today, but public, owned by the event, and documented).
- Convenience: `.auto_accept(true)` for quick-save semantics (no event round-trip; events still
  emitted for observability). If the consumer drops the receiver and auto-accept is off,
  requests are declined after the (configurable) accept timeout — never silently 403 by surprise.
- CLI `receive` becomes: subscribe, print request, `--auto-accept` or interactive y/n. TUI popup
  consumes the same events. Both shells lose their private rendezvous plumbing.

### 5.3 PIN enforcement (R2)

`server/pin.rs`: constant-time compare against configured PIN; per-IP failure counter;
3 failures → `429` for a 5-minute cooldown (matches official app). Applied to `prepare-upload`
and `prepare-download`. PIN and accept-events compose (PIN checked first, then event emitted).

### 5.4 Session store (R5, R6)

`core/session.rs`'s `Session` becomes the store's unit: random 128-bit hex tokens per file,
received-set tracking, `all_done` closes the session. Store holds one active upload session
(spec: `409` for a second concurrent sender) + download sessions (multiple). TTL enforced
lazily on access **and** by a background sweep task owned by the server.

### 5.5 HTTPS identity & pinning (R3, R4)

- Server: when `https`, `DeviceInfo.fingerprint = sha256_hex(cert DER)` **everywhere** —
  server info, register responses, multicast announcements (single source: the builder).
- Client: `TlsTrustPolicy` gains teeth via a custom rustls `ServerCertVerifier` that accepts
  exactly the pinned fingerprint(s) (TOFU or discovery-provided), installed with reqwest's
  preconfigured-TLS hook. `AcceptAll` stays available as the explicit lenient mode.
  Exact fingerprint derivation is asserted against the official core crate in the oracle layer.

### 5.6 Download API (G1)

Mirrors the TS design §6.4: staged files (`server.share(paths)` → sets `download: true` in
DeviceInfo), `prepare-download` (PIN-checked, creates download session), `download` (streams
bytes, `Content-Length`/`Content-Disposition`), minimal HTML `GET /` page listing files with
links. Client gains `prepare_download()` and `download()`.

---

## 6. Testing Strategy

All layers live in this repo. Layers 1–3 run in plain `cargo test` (fast, no Docker);
layers 4–5 run in containers (`e2e/`), mirroring CrossCopy's compose conventions
(sender/receiver services, shared volumes, sha256 assertions in scripts).

### 6.1 Layer 1 — Unit
Session store (tokens random, TTL, all-done, 409), `files.rs` collision renaming + save-path,
PIN lockout state machine, schema leniency (serde round-trips of spec-minimal payloads),
fingerprint derivation, path_safety (existing).

### 6.2 Layer 2 — Spec conformance (`tests/conformance_*.rs`)
Spin the real server on an ephemeral port; drive it with **raw reqwest** (not our client) so
we assert the wire, not ourselves:
- Status-code table: `204` empty files, `401` bad PIN, `403` declined, `409` second session,
  `429` after 3 PIN failures, `200` happy paths.
- Payload shapes for `info`/`register`/`prepare-upload` responses exact.
- Spec-minimal `DeviceInfo` (no `download`, no `port`) is accepted.
- Upload: one whole-body POST is accepted; token from a different session rejected (`403`).
- Download: `prepare-download` shape; `download` streams exact bytes.

### 6.3 Layer 3 — In-process interop (`tests/interop_*.rs`)
Real `LocalSendClient` ↔ real `LocalSendServer` over localhost:
sizes {0 B, 1 KB, 60 MB} byte-identical (sha256); multi-file sessions; PIN accept/reject;
decline via event; cancel; download direction; **HTTP and HTTPS** (pinned fingerprint).

### 6.4 Layer 4 — Container e2e (`e2e/`)
docker-compose scenarios, each a script asserting sha256 across volumes:
- `send-direct`: sender CLI → receiver CLI (headless, `--auto-accept`) by address.
- `discovery`: receiver announces; sender discovers via **multicast** on the compose network,
  then transfers. (User-defined bridge networks carry multicast between containers on Linux;
  CI runners are Linux. Kept as its own scenario so any environment flakiness doesn't block
  the direct-address scenarios.)
- `https`: same as send-direct over HTTPS with fingerprint pinning.
- `download`: receiver fetches from sender's share (reverse mode).
- `pin`: wrong PIN → 401; 3 fails → 429; right PIN succeeds.

### 6.5 Layer 5 — Official-core oracle (containerized, skippable)
`tools/oracle`: ~100-line bin over the **official** `localsend` crate
(git dep on `github.com/localsend/localsend`, package `localsend`, **pinned rev**; client side
is complete upstream). Compose service built by `docker/oracle.Dockerfile`.
Matrix: rs→oracle upload, oracle→rs upload, rs↔oracle download, × {HTTP, HTTPS}.
This is the closest automatable stand-in for "test against the official app" — the official
CLI is a 44-line Dart print-stub, and the Flutter app has no headless mode.
Separate CI job, `continue-on-error` (upstream crate is experimental).

### 6.6 Future layer — rs↔ts cross-implementation
When `localsend-ts` lands its harness (see sibling spec), add a `ts` compose service
(bun image running localsend-ts CLI) and reuse the Layer-4 scenarios. The compose file and
scripts are written so a peer service is pluggable (peer image + command are variables).

### 6.7 Manual (documented, not CI)
Spot-check checklist against installed **LocalSend 1.17.0** (Quick Save receiver; manual send
from the app; PIN dialog; browser share page from a phone). Lives in `docs/manual-testing.md`.

---

## 7. Phased Implementation Plan (summary — detailed plan doc is separate)

Each phase ends green (`cargo test`, `cargo clippy`, `cargo fmt --check`). TDD throughout.

- [ ] **Phase 0 — Test scaffolding**: `tests/helpers/`, ephemeral ports, temp dirs, random
      files, sha256; first rs↔rs smoke test against **current** code (accepting via the
      existing `pending_transfer` handle directly — the hack proves the harness, then dies).
- [ ] **Phase 1 — Structural refactor (no behavior change)**: split `server.rs` into
      `routes/handlers/state`; move file helpers into `core/files.rs`; delete D2/D3, wire D4/D5;
      CLI/TUI still compile and behave as before.
- [ ] **Phase 2 — Library receive API + PIN (R1, R2)**: `ServerEvent` stream + `PendingRequest`;
      builder API; auto-accept; CLI headless receive fixed; TUI rewired; PIN + lockout.
- [ ] **Phase 3 — Correctness sweep (R5–R9, R11) + container smoke**: session store wired
      (multi-file, random tokens), `204`, collisions, schema leniency, client `cancel`, real
      progress; then `e2e/` scaffold + `send-direct` scenario green in Docker.
- [ ] **Phase 4 — Download API (G1)**: server + client + minimal `GET /` page; `download` e2e scenario.
- [ ] **Phase 5 — HTTPS identity & pinning (R3, R4)**: cert-derived fingerprint everywhere;
      rustls pinning verifier; `https` e2e scenario.
- [ ] **Phase 6 — Oracle + full e2e matrix**: `tools/oracle`, oracle scenarios, `discovery` and
      `pin` scenarios, CI wiring (fast job = layers 1–3; e2e job = layer 4; oracle job = layer 5).
- [ ] **Phase 7 — Docs & polish**: README, manual checklist, periodic re-announce (G2),
      ts-service placeholder documented.

---

## 8. Risks & Open Questions

- **Rk1 — reqwest custom verifier**: pinning needs a rustls `ServerCertVerifier` via
  preconfigured TLS; API surface differs across reqwest versions (we're on 0.13). Verify the
  hook early in Phase 5; fallback is a small hyper+rustls client for pinned connections.
- **Rk2 — multicast inside Docker**: works on Linux bridge networks (CI), can flake on
  Docker Desktop for Mac. Mitigation: discovery is its own scenario; all other scenarios use
  direct addressing.
- **Rk3 — upstream `localsend` core crate instability**: pinned rev + skippable CI job.
- **Rk4 — breaking public API**: pre-1.0 and `apps/xc` is the only consumer; coordinate the
  version bump in the superproject when the new builder/event API lands.
- **Rk5 — TUI rewiring scope**: the TUI is 647 lines around the old rendezvous; rewiring to
  events must not balloon. Mitigation: events carry the same data the TUI already renders.
- **Q1 — fingerprint casing/format**: official app hex casing to be confirmed via oracle
  (assert exact match in Layer 5). *(Resolution owner: Phase 5.)*
- **Q2 — `POST /show`**: official-app endpoint (bring window to front), not in the protocol
  spec. Proposed: accept it with `200` + emit `ServerEvent::ShowRequested` (trivial); decide in
  Phase 4.

---

## 9. Summary

Make `localsend-rs` a **library-first**, fully **v2.1-conformant** implementation: fix the
broken headless accept path and PIN, wire the real session store, add the download half and
true HTTPS identity/pinning, delete the dead parallel abstractions — all verified by in-repo
`cargo test` layers plus a **containerized e2e suite** with the **official Rust core crate as
oracle**, designed so a future `localsend-ts` container can plug into the same matrix.
