# localsend-gpui

A native desktop window around [`localsend-rs`](../..), plus the same app's CLI.
This is the GPUI shell: the library does the protocol, this app does the pixels.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
  <img src="assets/logo-light.svg" width="112" alt="LocalSend RS radar logo">
</picture>

```bash
cd apps/desktop-gpui
cargo run --bin localsend-gpui          # the window
cargo run --bin localsend-gpui -- --help   # the CLI, same binary
```

## One command, two faces

`localsend-gpui` is a single binary that behaves like `code`: a bare launch opens
the window, and a word it knows turns it into the library's CLI.

| argv | what runs |
|---|---|
| *(nothing)* | the window |
| `receive`, `send`, `tui`, `discover`, … | `localsend-rs`'s CLI, argv forwarded untouched |
| `install-cli [--dir DIR]` | symlink `localsend` into `~/.local/bin` (no sudo) |
| `uninstall-cli` | remove that symlink, refusing to touch a regular file |
| `where-is-cli` | print the path of the installed link, exit 1 if absent |
| `bundle [--dir DIR]` | write `LocalSend RS.app` (default `~/Applications`) |
| `--help`, `--version`, `help` | the app's own help |

`install-cli` is idempotent and refuses to clobber a real file, so a second run
just replaces its own stale link. The classification lives in `src/entry.rs` and
is unit-tested against the argv shapes above: the point is that a person never has
to remember which of two binaries is which.

## What it does

- **Receive** — a hero status card (tinted by state, `ONLINE` chip, the port, the
  protocol, the certificate fingerprint and the folder, with a reveal button), an
  idle state built around a beacon and the three steps of a transfer, offers as
  cards that list each file with a kind tile and a total, live transfers with
  percentage/bytes/rate/ETA, and a "Recently received" list where each row leads
  with the file's kind icon and reveals in Finder.
- **Arrivals** — a sender's request is not just a badge: it posts a notification
  card at the top right (and a Notification Center banner when the window is in the
  background, on macOS, for a bundled `.app`) naming the sender and the first file,
  with **Accept**/**Decline** in the card. Answering anywhere retracts it; so does
  the offer expiring. Clicking the card body brings the Receive pane forward.
  The sidebar badge counts pending offers and paints dark ink on its amber fill —
  the theme's own `warning.foreground` is white, which is unreadable on amber.
- **Icon actions** — a folder, a star, a refresh, the trash and the appearance
  toggle are real buttons from one helper (`theme::icon_button`: a 28 pt square,
  quiet at rest, accent surface under the pointer), not bare glyphs. `s()` scales
  the square, and `with_size` — not `size` — is what makes the glyph grow with it.
- **Send** — nearby devices (UDP multicast announce, answered by `/register`; we
  re-announce), each with a device tile and a **star**. Picking one opens a stage
  panel: a kind tile per staged file, an optional PIN, Send, progress, cancel.
- **History** — every finished transfer, one row per file, grouped by day
  (`Today`, `Yesterday`, `N days ago`), each with a `RECEIVED`/`SENT` chip and the
  failure reason when there is one.
- **Settings** — device name, port, receiving folder, PIN, HTTPS, "accept without
  asking", "accept without asking from favorites", and the macOS glass appearance.
  Glass uses GPUI's blurred window material with adjustable app-surface opacity;
  blur strength stays under macOS control because GPUI does not expose a radius.
  LocalSend does not capture the screen or request screen-recording permission.
  Appearance edits preview immediately; **Apply** saves them and **Revert** restores
  the saved appearance. Network changes restart the receiver; appearance changes do not.
- **Favorites** — starring a device stores its *fingerprint*, not its address, so a
  phone that changes networks stays trusted. With "accept without asking from
  favorites" on, a starred device is taken straight into the folder (the window
  says `Accepted 1 file from X (favorite).`); anything unstarred still gets a
  prompt. Starring takes effect immediately — no listener restart.
- **Text messages** — a "file" that is really clipboard text is recorded in
  history with no path, and never written to disk.
- **Port conflicts** — the official LocalSend app can hold 53317 at the same time
  as this one. On `AddrInUse` the receiver rebinds on an OS-assigned port, says so
  in a warning banner naming the new port, and keeps announcing that real port, so
  the window stays usable instead of dying at startup.
- **Zoom** — `Cmd +` / `Cmd -` / `Cmd 0` on macOS, `Ctrl +` / `Ctrl -` / `Ctrl 0`
  elsewhere, plus a **View** menu. Every length in the panes goes through
  `theme::s()`, which multiplies by the live factor (70–200 %); gpui-kit's own
  rem-sized widgets follow through `Theme::font_size`, so buttons and inputs grow
  with the text instead of staying small.
- **Icon** — `assets/make-icon.py` centers the supplied transparent bronze-mark
  PNG on matching light/dark tiles. It emits PNG-backed SVG wrappers, sidebar marks, runtime Dock PNGs,
  `AppIcon.icns`, and the appearance-aware `Assets.car`. Bundled apps let macOS
  choose and size the icon; only an unbundled binary installs a runtime Dock icon.
- **UI** — GPUI Kit supplies the default light/dark theme, sidebar, title bar,
  and standard controls. LocalSend supplies its own branding and transfer layouts.
- **Updates** — Settings can check releases and explicitly download and verify a
  newer bundle. Verified installation requires a build-time publisher Team ID,
  a valid Developer ID signature, and Gatekeeper assessment. Unsigned builds
  cannot install updates. See [`docs/release/macos.md`](../../docs/release/macos.md)
  for distribution and signing requirements.

### Zoom shortcuts

| shortcut (macOS / Windows, Linux) | action |
|---|---|
| `Cmd +` or `Cmd =` / `Ctrl +`, `Ctrl =` | one step in (+10 %) |
| `Cmd -` / `Ctrl -` | one step out (−10 %) |
| `Cmd 0` / `Ctrl 0` | back to 100 % |

The bundle is not code-signed (no Apple Developer identity in this workspace), so a
bundle copied from another machine needs `xattr -dr com.apple.quarantine "LocalSend
RS.app"` before macOS will open it. Running it from the build directory needs
nothing.

## Architecture

Three layers, so that the part worth testing can be tested without a window:

```
src/entry.rs     argv -> window | library CLI | install/uninstall the `localsend` link
src/bundle.rs    writes `LocalSend RS.app` (Info.plist + icon + binary)
src/branding.rs  the compiled-in icon bytes and the runtime Dock icon
src/theme.rs     theme integration, surface opacity and the zoom factor every length uses
src/service/     tokio tasks: LocalSendServer, MulticastDiscovery, client sends.
                 Owns every network resource. Emits ServiceEvent, takes Command.
src/store.rs     GPUI entity fed by a pump task (smol::channel -> cx.spawn).
                 Keeps the Session; records commands when it has no service.
src/session.rs   the UI state machine: offers, peers, transfers, history.
                 Pure data + pure transitions — unit-tested with no GPUI at all.
src/app_state.rs window shell: wordmark sidebar, device footer, one visible pane.
src/views/       the four panes as pure functions of a Session snapshot.
```

The service is one controller task owning the server, the discovery socket, the
pending offers and the in-flight sends. The UI never owns a `PendingRequest`: it
answers an offer by id (`Command::Accept { id, .. }`), which is what makes a
declined offer impossible to leak. Server events and send results are funnelled
through one internal channel so the controller's `select!` never has to borrow
two things at once. Live settings that the library can flip on a running server
(`SetAutoAccept`, `SetFavorites`) are separate commands from `Restart`, so they do
not interrupt a transfer.

HTTPS reuses the library's persisted identity
(`load_or_generate_default_tls_certificate`), so restarting the app keeps the same
fingerprint — peers do not have to re-pin it. When sending, the peer's announced
fingerprint is pinned as the *only* trust anchor
(`TlsTrustPolicy::new([peer.node_id])`).

`WindowHandle`-less testing: `TransferStore::detached()` records the commands a
real store would have sent, so window tests assert "clicking Accept asked the
service to accept id 7" without a socket.

## Tests

```bash
  cargo test            # 173 tests: 128 unit/bin + 45 integration (window, network, screenshot)
  cargo clippy --all-targets -- -D warnings
  cargo fmt --check
  python3 assets/test_icon_grid.py
```

- `src/**` unit/bin tests (128) — the session state machine, formatting, settings
  (including favorites round-trip, normalization and glass defaults), argv classification, the
  install/uninstall link logic, the bundle layout and its plist, the icon assets,
  and the zoom arithmetic (scaling, clamping, reset).
- `tests/ui.rs` (15) — headless window tests: the sidebar switches panes, a
  listening status names its port/fingerprint/folder, an offer renders and
  Accept/Decline produce the right command, progress becomes history, staging a
  real temp file and pressing Send produces `Command::Send`, auto-accept flips
  live, Apply refuses a blank name, notices dismiss, stale peers are pruned, and
  **starring a peer emits `SetFavorites` and flips the button** both ways,
  **`Cmd +` really scales the layout** (the status card grows, `Cmd 0` restores it
  to the pixel), **an arriving offer posts a notification that disappears when the
  offer closes**, **the toast answers the request without leaving the pane**, and
  **the icon actions are buttons of at least a pointer-sized target**.
- `tests/service_e2e.rs` (5) — real-network tests over localhost: a
  `LocalSendClient` offers a file to our receiver (accept → bytes on disk, peer
  registered, an identity with no `ip` still works), our controller sends to a
  standalone `LocalSendServer` (progress, completion, bytes on the far side), a
  declined offer leaves the folder empty, **a taken port falls back to a free one
  and says which**, and **a favorite is accepted automatically while a stranger
  still gets a prompt** (both halves asserted, so plain auto-accept cannot pass it).
- `assets/test_icon_grid.py` — both transparent radar marks stay centered inside the
  same padded foreground grid.

Note for the window tests: `gpui_kit::open_window` wraps content in a `Root`, so
they keep the `AnyWindowHandle` and the `Entity<DesktopApp>` separately. Elements
under test opt in with `.test_support()`; buttons get it from `gpui_base`.

## Settings file

`~/Library/Application Support/LocalSendGpui/settings.json` (on macOS; the
platform's config directory elsewhere). Written atomically (temp file + rename),
and a missing or corrupt file reads as defaults rather than failing to start.

```json
{
  "alias": "Huakun's Mac",
  "port": 53317,
  "save_dir": "/Users/you/Downloads/LocalSend",
  "pin": null,
  "https": true,
  "glass_enabled": true,
  "surface_opacity": 78,
  "auto_accept": false,
  "auto_accept_favorites": false,
  "favorites": ["<sender fingerprint>"]
}
```

`favorites` is sorted, deduped and stripped of blanks on every save: it is a set
of fingerprints, so a duplicate would show as two rows for one device.

Environment overrides: `LOCALSEND_ALIAS` for the default device name, and
`LOCALSEND_GPUI_THEME=light|dark` to pin the appearance (otherwise the window
follows the system).

## Why a separate workspace

This directory is its own cargo workspace. A shell that links the Metal/winit GUI
stack must never sit in the library's dependency graph — the library's `cargo
test` gate stays GUI-free. The crate depends on `localsend-rs` with
`default-features = false, features = ["https", "cli", "tui"]`: `https` for the
default transport, `cli`/`tui` so this binary can hand `receive`/`send`/… to the
library's own dispatcher instead of re-implementing them.
