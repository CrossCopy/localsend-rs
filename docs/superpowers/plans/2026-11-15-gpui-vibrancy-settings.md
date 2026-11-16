# GPUI Vibrancy Settings Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Add persisted, immediately applicable native window vibrancy and app-surface opacity settings to the GPUI desktop app.

**Architecture:** Extend the existing desktop settings contract and form. Keep GPUI's native blurred window material, change its appearance on Apply, and feed a process-wide opacity value into app-owned background surfaces; do not add a custom platform blur layer.

**Tech Stack:** Rust, GPUI Kit 0.7, serde, GPUI component Slider.

**Spec:** `docs/superpowers/specs/2026-11-15-gpui-vibrancy-settings-design.md`

## Global Constraints

- `glass_enabled` defaults to `true`; `surface_opacity` defaults to `78` and is clamped to `0..=100`.
- Blur radius is not adjustable through the supported GPUI API; do not add private AppKit calls.
- Visual settings apply on Apply and do not restart the receiver.
- Every pane length uses `theme::s()`.

## Review Focus

- Older settings files omit the new keys: test defaults are filled in on load.
- Out-of-range opacity is loaded or applied: test it clamps to `0..=100`.
- Turning Glass off must select an opaque window; turning it on must select GPUI's blurred appearance.
- Changing opacity must repaint the app surfaces without requiring a receiver restart.
- Revert must restore both visual settings along with the other settings.

---

### Task 1: Persist and normalize appearance settings

**Files:**
- Modify: `apps/desktop-gpui/src/service/settings.rs`

**Interfaces:**
- Produces `Settings.glass_enabled: bool` and `Settings.surface_opacity: u8`.
- `Settings::normalized` clamps opacity to `0..=100`.
- `Settings::receiver_config_changed(next: &Self) -> bool` excludes visual-only settings.

- [x] **Step 1: Write failing settings tests**

Add tests asserting defaults (`true`, `78`), legacy JSON fills those defaults, values normalize to `0`/`100`, and visual-only changes do not require a receiver restart.

- [x] **Step 2: Run the targeted tests and verify they fail**

Run: `cargo test --manifest-path apps/desktop-gpui/Cargo.toml service::settings`

Expected: compilation/test failures because the fields/defaults/normalization are absent.

- [x] **Step 3: Add the settings fields and normalization**

Update `Settings`, `Default`, test fixtures, `normalize_in_place`, and `receiver_config_changed`; rely on `#[serde(default)]` for backward-compatible files.

- [x] **Step 4: Run the targeted tests**

Run: `cargo test --manifest-path apps/desktop-gpui/Cargo.toml service::settings`

Expected: PASS, including legacy-file compatibility and clamping.

### Task 2: Wire native glass and opacity into app surfaces

**Files:**
- Modify: `apps/desktop-gpui/src/theme.rs`
- Modify: `apps/desktop-gpui/src/main.rs`
- Modify: `apps/desktop-gpui/src/app_state.rs`
- Test: `apps/desktop-gpui/src/theme.rs`

**Interfaces:**
- `theme::set_surface_opacity(percent: u8) -> u8` stores and returns a clamped process-wide value; callers refresh windows after updating it.
- `theme::surface_opacity() -> f32` returns the alpha factor used by app-owned surfaces.
- `DesktopApp::apply_settings` receives the active `Window` so Apply can call `Window::set_background_appearance`.

- [x] **Step 1: Write a failing opacity behavior test**

Assert opacity values `0`, `78`, and `100` map to alpha `0.0`, `0.78`, and `1.0`, with out-of-range settings clamped before use.

- [x] **Step 2: Run the targeted test and verify it fails**

Run: `cargo test --manifest-path apps/desktop-gpui/Cargo.toml theme::tests`

Expected: compile failure because the opacity API does not exist.

- [x] **Step 3: Apply translucency to app backgrounds and the native window**

Use an atomic opacity value alongside the existing zoom value. Apply it to the root background and the shared themed surfaces. On Apply, choose `WindowBackgroundAppearance::Blurred` when enabled and `Opaque` otherwise; disabled Glass forces surface opacity to 100. Update opacity and refresh windows. Initialize opacity from loaded settings when constructing the app.

- [x] **Step 4: Remove prior temporary `DBG` instrumentation**

Remove only `DBG` logging added during visual troubleshooting; preserve normal diagnostics and the store observer's required `cx.notify()`.

- [x] **Step 5: Run the targeted tests**

Run: `cargo test --manifest-path apps/desktop-gpui/Cargo.toml theme::tests`

Expected: PASS and no debug output hooks remain.

### Task 3: Add appearance controls to Settings

**Files:**
- Modify: `apps/desktop-gpui/src/views/settings.rs`
- Test: existing desktop GPUI settings/window tests under `apps/desktop-gpui/tests/`

**Interfaces:**
- `SettingsForm` owns the Glass toggle and `SliderState` for opacity.
- `draft()` and `reload()` round-trip both settings.
- The Apply button passes the active `Window` through to `DesktopApp::apply_settings`.

- [x] **Step 1: Add failing form coverage**

Add tests showing appearance values survive draft/reload, an edited opacity is included in the applied `Settings`, visual-only Apply does not restart the receiver, and a detached app reports that settings were not persisted.

- [x] **Step 2: Run the relevant window tests and verify they fail**

Run: `cargo test --manifest-path apps/desktop-gpui/Cargo.toml --test ui settings`

Expected: failures because the form has no visual-appearance fields/control contract yet.

- [x] **Step 3: Implement the settings controls**

Add a Glass switch and 0–100 opacity slider with a live percentage label. State that blur strength is controlled by macOS/GPUI and has no radius control. Keep Apply/Revert behavior consistent with the rest of the form.

- [x] **Step 4: Run the focused tests**

Run: `cargo test --manifest-path apps/desktop-gpui/Cargo.toml --test ui settings`

Expected: PASS, including the new form behavior coverage.

### Task 4: Verify the desktop application

**Files:** No additional production files.

- [x] **Step 1: Format and run all desktop app tests**

Run from `apps/desktop-gpui`: `cargo fmt --check && cargo test -- --test-threads=1 && python3 assets/test_icon_grid.py`

Expected: formatting clean and all tests pass.

- [x] **Step 2: Run clippy**

Run from `apps/desktop-gpui`: `cargo clippy --all-targets -- -D warnings`

Expected: no warnings or errors.

- [x] **Step 3: Build and visually verify**

Run from `apps/desktop-gpui`: `cargo build`; launch the app, confirm Glass on/off and opacity changes are visible without restarting the receiver, and capture a screenshot as evidence.
