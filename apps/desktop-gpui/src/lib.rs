//! A native LocalSend window around the `localsend-rs` receive/upload half.
//!
//! The crate is a library plus a thin `main`, for one reason: the parts that can
//! be wrong in ways a screenshot will not show — the state machine, the settings,
//! the network controller — are testable without a window, and the test binaries
//! in `tests/` need something to link against.
//!
//! Layers, outermost first:
//!
//! * [`views`] — pure drawing, no state of its own.
//! * [`store`] — the GPUI entity the views read; forwards events to [`session`].
//! * [`session`] — the whole window state and its transitions, no GPUI.
//! * [`service`] — the tokio controller, no GPUI.
//!
//! Style and shell structure follow the two GPUI apps this was modelled on
//! (`refyard/apps/desktop-gpui` and `space-lens/apps/gpui`): one workspace per GUI
//! crate, `gpui-kit` components, a macOS-flavoured palette, a sidebar plus a
//! toolbar plus a content pane.

pub mod app_state;
pub mod branding;
pub mod bridge;
pub mod bundle;
pub mod entry;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod service;
pub mod session;
pub mod store;
pub mod theme;
pub mod update;
pub mod views;

pub use app_state::DesktopApp;
pub use store::TransferStore;

/// The palette env var, shared with the reference apps: `light` or `dark` pins the
/// window, anything else follows the system.
pub const THEME_ENV: &str = "LOCALSEND_GPUI_THEME";
