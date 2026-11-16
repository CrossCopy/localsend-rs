//! The app's face: the icon the Dock shows, and the icon bytes a bundle copies.
//!
//! GPUI's window has no icon API, and the window title bar is the only place a
//! running app would otherwise hint at its identity. Both assets are drawn by
//! `assets/make-icon.py` and compiled in, so an app started straight out of
//! `target/debug` looks like an app and not like a blank white square.
//!
//! Two appearances, because macOS switches app icons with the system theme: the
//! dark tile is the product's native deep-navy/mint, the light tile flips both so
//! the mark keeps its contrast on a white tile. A bundled `.app` gets this for
//! free from `Assets.car` (see [`crate::bundle`]); a bare binary has to be told,
//! which is what [`install_dock_icon`] is for.

use std::sync::{Arc, OnceLock};

use gpui_kit::{Image, ImageFormat};

/// The 1024² dark tile — the native appearance.
pub const APP_ICON_DARK_PNG: &[u8] = include_bytes!("../assets/AppIcon-Dark.png");

/// The 1024² light tile, for a light-mode Dock.
pub const APP_ICON_LIGHT_PNG: &[u8] = include_bytes!("../assets/AppIcon-Light.png");

/// The transparent dark radar mark used in the app's sidebar.
pub const MARK_DARK_PNG: &[u8] = include_bytes!("../assets/Mark-Dark.png");

/// The transparent light radar mark used in the app's sidebar.
pub const MARK_LIGHT_PNG: &[u8] = include_bytes!("../assets/Mark-Light.png");

/// The multi-resolution tile Finder, the Dock and Launchpad read from a bundle.
pub const APP_ICON_ICNS: &[u8] = include_bytes!("../assets/AppIcon.icns");

/// The compiled asset catalogue carrying both appearances, written by `actool`
/// from the Icon Composer document. macOS 26 prefers this over the `.icns`.
pub const APP_ICON_CAR: &[u8] = include_bytes!("../assets/Assets.car");

/// Name macOS gives the icon inside `Contents/Resources`.
pub const APP_ICON_STEM: &str = "AppIcon";

/// A cached theme-specific radar image for the in-app wordmark.
pub fn sidebar_mark(dark: bool) -> Arc<Image> {
    static DARK: OnceLock<Arc<Image>> = OnceLock::new();
    static LIGHT: OnceLock<Arc<Image>> = OnceLock::new();
    let (cache, bytes) = if dark {
        (&DARK, MARK_DARK_PNG)
    } else {
        (&LIGHT, MARK_LIGHT_PNG)
    };
    Arc::clone(cache.get_or_init(|| Arc::new(Image::from_bytes(ImageFormat::Png, bytes.to_vec()))))
}

#[cfg(any(target_os = "macos", test))]
fn should_install_runtime_icon(exe: &std::path::Path) -> bool {
    let bundle = exe
        .parent()
        .filter(|path| path.file_name().is_some_and(|name| name == "MacOS"))
        .and_then(|path| path.parent())
        .filter(|path| path.file_name().is_some_and(|name| name == "Contents"))
        .and_then(|path| path.parent());
    !bundle.is_some_and(|path| path.extension().is_some_and(|ext| ext == "app"))
}

/// Put the brand icon in the Dock for an unbundled executable.
///
/// Bundled apps leave icon sizing and appearance selection to macOS. Safe to
/// call more than once; it replaces the image, not the process identity.
///
/// `dark` picks the tile that matches the window's appearance. Only meaningful
/// on macOS — elsewhere an executable's icon comes from the desktop's launcher
/// entry, not from within the process.
#[cfg(target_os = "macos")]
pub fn install_dock_icon(dark: bool) {
    use objc2::{AnyThread as _, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    // Bundled apps already have an appearance-aware system icon. Overriding it
    // with the full-canvas PNG bypasses the system's standard icon sizing.
    if !std::env::current_exe().is_ok_and(|exe| should_install_runtime_icon(&exe)) {
        return;
    }

    // The call is main-thread-only, and the window bootstrap is on the main
    // thread; if this ever runs elsewhere there is nothing to do but leave the
    // default icon.
    let Some(marker) = MainThreadMarker::new() else {
        return;
    };

    let bytes = if dark {
        APP_ICON_DARK_PNG
    } else {
        APP_ICON_LIGHT_PNG
    };
    let data = NSData::with_bytes(bytes);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        return;
    };

    // SAFETY: main thread (checked above), and `image` outlives the call —
    // AppKit retains the image it is handed.
    unsafe {
        NSApplication::sharedApplication(marker).setApplicationIconImage(Some(&image));
    }
}

#[cfg(not(target_os = "macos"))]
pub fn install_dock_icon(_dark: bool) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn bundle_uses_system_icon_while_standalone_binary_uses_runtime_icon() {
        use std::path::Path;

        assert!(!should_install_runtime_icon(Path::new(
            "/Applications/LocalSend RS.app/Contents/MacOS/localsend-gpui"
        )));
        assert!(should_install_runtime_icon(Path::new(
            "/tmp/build/localsend-gpui"
        )));
    }

    /// PNG: 8-byte signature, then the IHDR chunk with width and height.
    fn png_size(bytes: &[u8]) -> (u32, u32) {
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
        assert_eq!(&bytes[12..16], b"IHDR", "the first chunk must be IHDR");
        let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
        (width, height)
    }

    #[::core::prelude::v1::test]
    fn both_runtime_icons_are_square_1024_tiles() {
        assert_eq!(png_size(APP_ICON_DARK_PNG), (1024, 1024));
        assert_eq!(png_size(APP_ICON_LIGHT_PNG), (1024, 1024));
        assert_eq!(png_size(MARK_DARK_PNG), (1024, 1024));
        assert_eq!(png_size(MARK_LIGHT_PNG), (1024, 1024));
    }

    #[::core::prelude::v1::test]
    fn the_two_appearances_are_actually_different_pictures() {
        assert_ne!(
            APP_ICON_DARK_PNG, APP_ICON_LIGHT_PNG,
            "a light icon that is a copy of the dark one is not a light icon"
        );
    }

    #[::core::prelude::v1::test]
    fn the_bundle_icon_is_a_real_icns() {
        let renditions = icns_renditions(APP_ICON_ICNS)
            .expect("the bundle icon must have a structurally valid ICNS file");
        assert!(
            renditions.len() >= 3,
            "the bundle icon must contain several distinct image renditions"
        );
    }

    #[::core::prelude::v1::test]
    fn icns_validation_rejects_malformed_chunks_and_counts_distinct_renditions() {
        let valid = test_icns_with_chunks(&[*b"ic04", *b"ic07", *b"ic13"]);
        assert_eq!(icns_renditions(&valid).unwrap().len(), 3);

        let mut truncated = valid.clone();
        truncated.pop();
        assert!(icns_renditions(&truncated).is_none());

        let mut wrong_total_length = valid.clone();
        wrong_total_length[4..8].copy_from_slice(&((valid.len() - 1) as u32).to_be_bytes());
        assert!(icns_renditions(&wrong_total_length).is_none());

        let mut out_of_bounds_chunk = valid;
        out_of_bounds_chunk[12..16].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(icns_renditions(&out_of_bounds_chunk).is_none());

        let mut undersized_chunk = test_icns_with_chunks(&[*b"ic04", *b"ic07", *b"ic13"]);
        undersized_chunk[12..16].copy_from_slice(&7_u32.to_be_bytes());
        assert!(icns_renditions(&undersized_chunk).is_none());

        let repeated_rendition_types = test_icns_with_chunks(&[*b"ic04", *b"ic04", *b"ic07"]);
        assert_eq!(icns_renditions(&repeated_rendition_types).unwrap().len(), 2);
    }

    fn test_icns_with_chunks(types: &[[u8; 4]]) -> Vec<u8> {
        let total_length = 8 + types.len() * 9;
        let mut bytes = b"icns".to_vec();
        bytes.extend_from_slice(&(total_length as u32).to_be_bytes());
        for kind in types {
            bytes.extend_from_slice(kind);
            bytes.extend_from_slice(&9_u32.to_be_bytes());
            bytes.push(0);
        }
        bytes
    }

    fn icns_renditions(bytes: &[u8]) -> Option<Vec<[u8; 4]>> {
        const SUPPORTED_RENDITIONS: [[u8; 4]; 14] = [
            *b"ic04", *b"ic05", *b"ic06", *b"ic07", *b"ic08", *b"ic09", *b"ic10", *b"ic11",
            *b"ic12", *b"ic13", *b"ic14", *b"icp4", *b"icp5", *b"icp6",
        ];

        if bytes.get(..4)? != b"icns" {
            return None;
        }
        let declared_length = u32::from_be_bytes(bytes.get(4..8)?.try_into().ok()?) as usize;
        if declared_length != bytes.len() {
            return None;
        }

        let mut offset = 8;
        let mut renditions = Vec::new();
        while offset < declared_length {
            let header_end = offset.checked_add(8)?;
            let chunk_header = bytes.get(offset..header_end)?;
            let kind: [u8; 4] = chunk_header.get(..4)?.try_into().ok()?;
            let chunk_length =
                u32::from_be_bytes(chunk_header.get(4..8)?.try_into().ok()?) as usize;
            if chunk_length < 9 {
                return None;
            }
            let chunk_end = offset.checked_add(chunk_length)?;
            if chunk_end > declared_length {
                return None;
            }
            if SUPPORTED_RENDITIONS.contains(&kind) && !renditions.contains(&kind) {
                renditions.push(kind);
            }
            offset = chunk_end;
        }

        Some(renditions)
    }

    #[::core::prelude::v1::test]
    fn the_compiled_catalogue_carries_the_dark_appearance() {
        // `Assets.car` is a binary BOM; the appearance names are stored as plain
        // strings inside it, which is enough to prove actool kept both variants.
        let needle = b"NSAppearanceNameDarkAqua";
        assert!(
            APP_ICON_CAR
                .windows(needle.len())
                .any(|window| window == needle),
            "actool must be run over AppIcon.icon, or the dark tile never ships"
        );
    }
}
