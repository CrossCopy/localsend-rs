//! A real `.app`, so the window can live in the Dock, Finder and Launchpad.
//!
//! macOS identifies an application by its bundle, not by its executable: the
//! Dock tile, the icon Finder draws, the name in the menu bar and the identity
//! Launchpad indexes all come from `Contents/Info.plist` plus
//! `Contents/Resources/AppIcon.icns` and `Contents/Resources/Assets.car`. A bare
//! binary can only ever be "the thing Terminal started", which is why running it
//! from `target/debug` gets a generic icon and a process name nobody can find
//! again.
//!
//! Both icon files ship on purpose: the `.icns` is what every macOS before 26
//! reads, and the `.car` is the compiled Icon Composer catalogue that lets the
//! same bundle carry a light *and* a dark tile (`CFBundleIconName` points at it).
//!
//! This module writes that layout. It is deliberately argument-free and
//! side-effect-explicit so the layout is testable without `open`, Finder or a
//! window: given an executable and a directory, it produces a bundle whose
//! plist, icon and binary are what macOS expects.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::branding::{APP_ICON_CAR, APP_ICON_ICNS, APP_ICON_STEM};

/// The bundle's name on disk, and the name the system shows.
pub const BUNDLE_NAME: &str = "LocalSend RS.app";
pub const APP_NAME: &str = "LocalSend RS";

/// The binary inside `Contents/MacOS`. It must match `CFBundleExecutable`.
pub const EXECUTABLE: &str = "localsend-gpui";

/// The reverse-DNS identity. Stable across rebuilds: it is what macOS keys
/// Launchpad position, notification permissions and saved state on.
pub const BUNDLE_ID: &str = "app.crosscopy.localsend-gpui";

/// `Info.plist`, the bundle's manifest.
///
/// Only the keys that change behaviour for a GUI app are set; the rest default
/// sensibly. `NSHighResolutionCapable` is not optional in practice — without it
/// the window is rendered at 1× and looks blurry on every Retina display.
pub fn info_plist() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleDisplayName</key>
	<string>{APP_NAME}</string>
	<key>CFBundleExecutable</key>
	<string>{EXECUTABLE}</string>
	<key>CFBundleIconFile</key>
	<string>{APP_ICON_STEM}</string>
	<key>CFBundleIconName</key>
	<string>{APP_ICON_STEM}</string>
	<key>CFBundleIdentifier</key>
	<string>{BUNDLE_ID}</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>{APP_NAME}</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>{version}</string>
	<key>CFBundleVersion</key>
	<string>{version}</string>
	<key>LSMinimumSystemVersion</key>
	<string>12.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
"#
    )
}

/// Write `dir/LocalSend RS.app` around `exe` and return the bundle path.
///
/// Replaces an existing bundle wholesale rather than merging: a stale file from
/// a previous build is the most confusing thing a bundle can contain. The copy
/// of the binary is marked executable, because `fs::copy` only carries the
/// bytes.
pub fn bundle(dir: &Path, exe: &Path) -> io::Result<PathBuf> {
    let app = dir.join(BUNDLE_NAME);
    if app.exists() {
        fs::remove_dir_all(&app)?;
    }

    let contents = app.join("Contents");
    let macos = contents.join("MacOS");
    let resources = contents.join("Resources");
    fs::create_dir_all(&macos)?;
    fs::create_dir_all(&resources)?;

    let binary = macos.join(EXECUTABLE);
    fs::copy(exe, &binary)?;
    set_executable(&binary)?;

    fs::write(contents.join("Info.plist"), info_plist())?;
    // Baggage from the OS 9 era that Finder still reads to know this is an app.
    fs::write(contents.join("PkgInfo"), "APPL????")?;
    fs::write(
        resources.join(format!("{APP_ICON_STEM}.icns")),
        APP_ICON_ICNS,
    )?;
    // The Icon Composer catalogue: light and dark renditions, plus the tinted
    // and clear variants macOS 26 can ask for. Without it the bundle shows the
    // light tile in a dark Dock.
    fs::write(resources.join("Assets.car"), APP_ICON_CAR)?;

    Ok(app)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_exe(dir: &Path) -> PathBuf {
        let exe = dir.join("localsend-gpui-original");
        fs::write(&exe, b"#!/bin/sh\necho localsend\n").unwrap();
        exe
    }

    #[::core::prelude::v1::test]
    fn the_plist_names_the_binary_and_the_icon_that_are_actually_inside() {
        let plist = info_plist();
        let version = env!("CARGO_PKG_VERSION");
        for needle in [
            "<key>CFBundleExecutable</key>",
            EXECUTABLE,
            "<key>CFBundleIdentifier</key>",
            BUNDLE_ID,
            "<key>CFBundleIconFile</key>",
            APP_ICON_STEM,
            "<key>CFBundleIconName</key>",
            "<key>NSHighResolutionCapable</key>",
        ] {
            assert!(plist.contains(needle), "Info.plist is missing {needle}");
        }
        for key in ["CFBundleShortVersionString", "CFBundleVersion"] {
            assert!(
                plist.contains(&format!("<key>{key}</key>\n\t<string>{version}</string>")),
                "Info.plist {key} must match the Cargo package version {version}"
            );
        }
        assert!(
            plist.contains(&format!(
                "<key>CFBundleIdentifier</key>\n\t<string>{BUNDLE_ID}</string>"
            )),
            "Info.plist must use the stable release bundle identifier"
        );
        assert!(
            plist.contains("<string>APPL</string>"),
            "the package type must be an application"
        );
    }

    #[::core::prelude::v1::test]
    fn a_bundle_gets_the_binary_the_manifest_and_the_icon() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = fake_exe(tmp.path());

        let app = bundle(tmp.path(), &exe).unwrap();

        assert_eq!(app.file_name().unwrap(), BUNDLE_NAME);
        let contents = app.join("Contents");
        assert_eq!(
            fs::read_to_string(contents.join("Info.plist")).unwrap(),
            info_plist()
        );
        assert_eq!(
            fs::read(contents.join("Resources/AppIcon.icns")).unwrap(),
            APP_ICON_ICNS,
            "the bundle icon must be the drawn one, byte for byte"
        );
        assert_eq!(
            fs::read(contents.join("Resources/Assets.car")).unwrap(),
            APP_ICON_CAR,
            "the appearance-aware catalogue has to ship with the bundle"
        );

        let binary = contents.join("MacOS").join(EXECUTABLE);
        assert_eq!(
            fs::read(&binary).unwrap(),
            fs::read(&exe).unwrap(),
            "the copy must be the built binary"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&binary).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "the copy must be runnable: {mode:o}");
        }
    }

    #[::core::prelude::v1::test]
    fn rebundling_replaces_a_stale_bundle_instead_of_merging_it() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = fake_exe(tmp.path());
        let app = bundle(tmp.path(), &exe).unwrap();

        let stale = app.join("Contents/MacOS/leftover-from-an-old-build");
        fs::write(&stale, b"stale").unwrap();

        let rebuilt = bundle(tmp.path(), &exe).unwrap();
        assert_eq!(rebuilt, app);
        assert!(
            !stale.exists(),
            "a rebuild must not inherit files from the previous one"
        );
    }
}
