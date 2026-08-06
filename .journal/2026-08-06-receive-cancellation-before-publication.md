# Receive cancellation before publication

## Core decision/topic

Make cancellation, timeout replacement and file publication share one
Session-owned lifecycle rather than allowing a live HTTP handler to outlast the
session that authorized it.

## Options considered

1. Clear the current session immediately when it is cancelled or times out.
   Rejected: an already-reading handler can still atomically publish and emit a
   successful receive after ownership has been cleared.
2. Check for cancellation only after publish.  Rejected: that cannot prevent a
   file from landing and does not linearize the race.
3. Claim a per-file receive lease, atomically exchange it for a publication
   permit, and retain that permit until commit plus session event accounting.
   Chosen: the publication transition is the single race winner.

## Final decision and rationale

`Session` now owns each file's phase and cancellation token.  Only one body can
hold a lease.  Cancellation can transition `Ready`/`Receiving` slots to
`Cancelled` and interrupt streaming; it refuses to clear a session containing a
`Publishing` slot.  A handler that wins `Receiving -> Publishing` must finish
the atomic file commit and session accounting before releasing its permit.

## Key changes made

- Added explicit receive phases, `StandardReceiveLease`, and
  `StandardPublicationPermit` to `Session`.
- Raced the streaming body against the session cancellation token and preserve
  rollback behavior for partial output/progress.
- Made cancellation, replacement, shutdown and session sweep respect an active
  publication; concurrent/replayed uploads now return conflict rather than
  writing twice.
- Added paused-body integration tests for explicit cancel and timeout-triggered
  replacement, both proving no file and no success event are published.
- Updated the File-v3 Ready test fixture with the frozen legacy LocalSend data
  plane fields required by the current shared proto.

## Verification

- `CARGO_TARGET_DIR=/Users/hk/Dev/CrossCopy/target cargo test --test receive_path_safety -- --nocapture` — 6 passed.
- `CARGO_TARGET_DIR=/Users/hk/Dev/CrossCopy/target cargo test` — 72 unit and
  45 integration tests passed; one external-peer-only discovery test stayed
  explicitly ignored.
- `cargo fmt --check` and
  `CARGO_TARGET_DIR=/Users/hk/Dev/CrossCopy/target cargo clippy --all-targets --all-features -- -D warnings` passed.

## Future considerations

This proves server-side cancellation before publication on loopback HTTP.  UI
confirmation, desktop runtime behaviour and physical-device evidence remain
separate validation layers.
