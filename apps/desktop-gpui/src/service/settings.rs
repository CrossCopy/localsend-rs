//! The settings the window edits and the service obeys, and where they persist.
//!
//! Saved as JSON under the platform's application-support directory, beside the
//! TLS identity the library keeps, so one machine has one LocalSend name and one
//! certificate across runs. Loading is tolerant by design: a missing file, a
//! half-written file or a field a newer build added falls back to the defaults
//! instead of refusing to start — an unreadable settings file must never be the
//! reason a file transfer app will not open.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The LocalSend port every implementation uses. Kept here rather than imported
/// because a settings file may legitimately name a non-default one.
pub const DEFAULT_PORT: u16 = 53317;
pub const DEFAULT_SURFACE_OPACITY: u8 = 78;
pub const MAX_SURFACE_OPACITY: u8 = 100;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The name peers see in their device lists.
    pub alias: String,
    /// Where received files land.
    pub save_dir: PathBuf,
    /// TCP port the receiver listens on. 0 asks the OS for one (used by tests).
    pub port: u16,
    /// The receiver's PIN, or `None` for an open receiver.
    pub pin: Option<String>,
    /// Accept every offer without showing it first.
    pub auto_accept: bool,
    /// Accept offers from `.favorites` without showing them, whatever
    /// `auto_accept` says. This is the narrow version of quick save: a phone you
    /// own can drop files on you, a stranger cannot.
    pub auto_accept_favorites: bool,
    /// Fingerprints of the devices this machine trusts. Identity, not address —
    /// a phone that changes networks keeps its fingerprint.
    pub favorites: Vec<String>,
    /// Speak HTTPS (the official app's default) rather than plain HTTP.
    pub https: bool,
    /// Use the native blurred window material behind app surfaces.
    pub glass_enabled: bool,
    /// Opacity of app-drawn surfaces over the native material, from 0 to 100.
    pub surface_opacity: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            alias: default_alias(),
            save_dir: default_save_dir(),
            port: DEFAULT_PORT,
            pin: None,
            auto_accept: false,
            auto_accept_favorites: false,
            favorites: Vec::new(),
            https: true,
            glass_enabled: true,
            surface_opacity: DEFAULT_SURFACE_OPACITY,
        }
    }
}

impl Settings {
    /// Whether applying `next` changes anything consumed by the receiver.
    /// Appearance-only edits are deliberately excluded so the listener keeps
    /// transfers in flight while the window style changes.
    pub fn receiver_config_changed(&self, next: &Self) -> bool {
        self.alias != next.alias
            || self.save_dir != next.save_dir
            || self.port != next.port
            || self.pin != next.pin
            || self.auto_accept != next.auto_accept
            || self.auto_accept_favorites != next.auto_accept_favorites
            || self.favorites != next.favorites
            || self.https != next.https
    }

    /// Trim what the UI may have let trailing, and drop an empty PIN rather than
    /// storing `Some("")` — the library treats a PIN of `""` as a required PIN
    /// that nobody can type.
    pub fn normalized(mut self) -> Self {
        self.normalize_in_place();
        self
    }

    /// The same cleanup, on a value somebody already owns. Used by the controller
    /// when the window hands it a new favorites list.
    pub fn normalize_in_place(&mut self) {
        self.alias = self.alias.trim().to_string();
        self.surface_opacity = self.surface_opacity.min(MAX_SURFACE_OPACITY);
        // A favorite list is a set: duplicates would show up as two rows for one
        // device in every view that iterates it.
        self.favorites.sort();
        self.favorites.dedup();
        self.favorites.retain(|id| !id.trim().is_empty());
        self.pin = self
            .pin
            .take()
            .map(|pin| pin.trim().to_string())
            .filter(|pin| !pin.is_empty());
    }

    /// What is wrong with these settings, in the order a person would fix it.
    pub fn validate(&self) -> Result<(), String> {
        let settings = self.clone().normalized();
        if settings.alias.is_empty() {
            return Err("The device name cannot be empty.".to_string());
        }
        if settings.alias.chars().count() > 32 {
            return Err("The device name can be at most 32 characters.".to_string());
        }
        if settings.port == 0 {
            return Err("The port must be between 1 and 65535.".to_string());
        }
        if let Some(pin) = &settings.pin {
            let numeric = pin.chars().all(|c| c.is_ascii_digit());
            if !numeric || !(4..=12).contains(&pin.len()) {
                return Err("A PIN is 4 to 12 digits, or empty for no PIN.".to_string());
            }
        }
        if settings.save_dir.as_os_str().is_empty() {
            return Err("Choose a folder for received files.".to_string());
        }
        Ok(())
    }

    /// Is this device trusted for a silent accept?
    pub fn is_favorite(&self, fingerprint: &str) -> bool {
        self.favorites.iter().any(|id| id == fingerprint)
    }

    /// Add or remove one device, keeping the list sorted and unique.
    pub fn toggle_favorite(&mut self, fingerprint: &str) -> bool {
        let now_favorite = !self.is_favorite(fingerprint);
        if now_favorite {
            self.favorites.push(fingerprint.to_string());
            self.favorites.sort();
            self.favorites.dedup();
        } else {
            self.favorites.retain(|id| id != fingerprint);
        }
        now_favorite
    }

    /// Would an offer from this device be taken without asking?
    pub fn auto_accepts_from(&self, fingerprint: &str) -> bool {
        self.auto_accept || (self.auto_accept_favorites && self.is_favorite(fingerprint))
    }

    /// The settings file this build reads and writes.
    pub fn config_path() -> Option<PathBuf> {
        config_dir().map(|dir| dir.join("settings.json"))
    }

    /// Read the stored settings, or the defaults when there is nothing usable.
    pub fn load() -> Self {
        match Self::config_path() {
            Some(path) => Self::load_from(&path),
            None => Self::default(),
        }
    }

    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_json::from_str::<Settings>(&text) {
            Ok(settings) => settings.normalized(),
            Err(_) => Self::default(),
        }
    }

    /// Persist to the standard location. Failure is reported, never fatal.
    pub fn save(&self) -> std::io::Result<()> {
        match Self::config_path() {
            Some(path) => self.save_to(&path),
            None => Ok(()),
        }
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(&self.clone().normalized())
            .map_err(std::io::Error::other)?;
        // Write beside the target and rename: a crash mid-write leaves the old
        // file intact rather than a truncated one that reads back as defaults.
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, text)?;
        std::fs::rename(&temporary, path)
    }
}

fn config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library").join("Application Support"));
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("LOCALAPPDATA"))
        .map(PathBuf::from);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));

    base.map(|base| base.join("LocalSendGpui"))
}

fn default_save_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home {
        Some(home) => home.join("Downloads").join("LocalSend"),
        None => PathBuf::from("downloads"),
    }
}

/// A name a peer list can show without configuration. `LOCALSEND_ALIAS` wins
/// (scripts and tests set it); otherwise the machine's own short host name, which
/// is what a person recognises in another device's list.
fn default_alias() -> String {
    if let Ok(alias) = std::env::var("LOCALSEND_ALIAS") {
        let alias = alias.trim();
        if !alias.is_empty() {
            return alias.to_string();
        }
    }
    let host = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().trim_end_matches(".local").to_string())
        .filter(|name| !name.is_empty());
    match host {
        Some(host) => format!("{host} (LocalSend)"),
        None => "LocalSend".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Settings {
        Settings {
            alias: "Huakun's Mac".to_string(),
            save_dir: PathBuf::from("/tmp/localsend-test"),
            port: 53317,
            pin: None,
            auto_accept: false,
            auto_accept_favorites: false,
            favorites: Vec::new(),
            https: true,
            glass_enabled: true,
            surface_opacity: DEFAULT_SURFACE_OPACITY,
        }
    }

    #[test]
    fn defaults_are_a_valid_open_https_receiver() {
        let settings = Settings::default();
        assert!(!settings.alias.trim().is_empty());
        assert_eq!(settings.port, DEFAULT_PORT);
        assert!(settings.https);
        assert!(settings.glass_enabled);
        assert_eq!(settings.surface_opacity, 78);
        assert!(!settings.auto_accept);
        assert_eq!(settings.pin, None);
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn surface_opacity_is_clamped_to_the_supported_range() {
        assert_eq!(
            Settings {
                surface_opacity: 0,
                ..sample()
            }
            .normalized()
            .surface_opacity,
            0
        );
        assert_eq!(
            Settings {
                surface_opacity: 255,
                ..sample()
            }
            .normalized()
            .surface_opacity,
            100
        );
    }

    #[test]
    fn visual_only_changes_do_not_require_a_receiver_restart() {
        let current = sample();
        let appearance = Settings {
            glass_enabled: false,
            surface_opacity: 43,
            ..current.clone()
        };
        assert!(!current.receiver_config_changed(&appearance));

        let receiver = Settings {
            port: 51234,
            ..appearance
        };
        assert!(current.receiver_config_changed(&receiver));
    }

    #[test]
    fn a_blank_alias_is_refused() {
        let settings = Settings {
            alias: "   ".into(),
            ..sample()
        };
        assert!(settings.validate().unwrap_err().contains("name"));
    }

    #[test]
    fn pins_are_four_to_twelve_digits_or_absent() {
        let valid = |pin: &str| {
            Settings {
                pin: Some(pin.to_string()),
                ..sample()
            }
            .validate()
            .is_ok()
        };
        assert!(valid("1234"));
        assert!(valid("123456"));
        assert!(!valid("123"));
        assert!(!valid("1234567890123"));
        assert!(!valid("12ab"));
        let empty = Settings {
            pin: Some("   ".into()),
            ..sample()
        };
        assert!(empty.validate().is_ok(), "a blank PIN means no PIN");
        assert_eq!(empty.normalized().pin, None);
    }

    #[test]
    fn port_zero_is_refused() {
        let settings = Settings {
            port: 0,
            ..sample()
        };
        assert!(settings.validate().is_err());
    }

    #[test]
    fn a_favorite_is_a_set_of_fingerprints_and_can_be_toggled() {
        let mut settings = sample();
        assert!(!settings.is_favorite("phone-a"));
        assert!(settings.toggle_favorite("phone-a"));
        assert!(settings.is_favorite("phone-a"));
        // Toggling twice is how the star button works.
        assert!(!settings.toggle_favorite("phone-a"));
        assert!(!settings.is_favorite("phone-a"));

        // Duplicates collapse, and an edited list is sorted on save.
        settings.favorites = vec![
            "b".to_string(),
            "a".to_string(),
            "a".to_string(),
            "  ".to_string(),
        ];
        assert_eq!(settings.clone().normalized().favorites, vec!["a", "b"]);
    }

    #[test]
    fn quick_save_for_favorites_is_narrower_than_quick_save_for_everyone() {
        let mut settings = Settings {
            favorites: vec!["trusted".to_string()],
            ..sample()
        };
        assert!(!settings.auto_accepts_from("trusted"), "off by default");
        assert!(!settings.auto_accepts_from("stranger"));

        settings.auto_accept_favorites = true;
        assert!(settings.auto_accepts_from("trusted"));
        assert!(!settings.auto_accepts_from("stranger"), "favorites only");

        settings.auto_accept = true;
        assert!(settings.auto_accepts_from("stranger"), "everyone, then");
    }

    #[test]
    fn a_favorites_list_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let settings = Settings {
            auto_accept_favorites: true,
            favorites: vec!["phone-a".to_string(), "laptop-b".to_string()],
            ..sample()
        };
        let mut expected = settings.favorites.clone();
        expected.sort();
        settings.save_to(&path).unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.favorites, expected);
        assert!(loaded.auto_accept_favorites);
    }

    #[test]
    fn settings_round_trip_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("settings.json");
        let settings = Settings {
            pin: Some("4321".into()),
            port: 51234,
            ..sample()
        };
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), settings);
    }

    #[test]
    fn a_missing_or_corrupt_file_reads_as_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.json");
        assert_eq!(Settings::load_from(&missing), Settings::default());

        let corrupt = dir.path().join("corrupt.json");
        std::fs::write(&corrupt, "{ not json").unwrap();
        assert_eq!(Settings::load_from(&corrupt), Settings::default());
    }

    #[test]
    fn a_file_from_an_older_build_fills_the_new_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(path.as_path(), r#"{"alias":"Old Mac","https":false}"#).unwrap();
        let settings = Settings::load_from(&path);
        assert_eq!(settings.alias, "Old Mac");
        assert!(!settings.https);
        assert_eq!(settings.port, DEFAULT_PORT);
        assert!(!settings.auto_accept);
        assert!(settings.glass_enabled);
        assert_eq!(settings.surface_opacity, 78);
    }
}
