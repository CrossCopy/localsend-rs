//! One binary, two products.
//!
//! `localsend-gpui` with no arguments opens the window. With a word that the
//! library's CLI understands — `send`, `receive`, `discover`, `tui` — the same
//! executable runs that command instead, so a person who installed the window
//! can also script a transfer without installing a second binary. Two more
//! commands are ours: `install-cli` puts a `localsend` symlink on PATH the way
//! `code` does, and `uninstall-cli` takes it away.
//!
//! This module is deliberately argument-string-in, decision-out: what a set of
//! argv means is testable without a window, a socket or a terminal.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// What a command line asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// Open the window.
    Gui,
    /// Hand these arguments to the library CLI unchanged (program name included).
    Cli(Vec<OsString>),
    /// Print the combined help text.
    Help,
    /// Print the version.
    Version,
    /// Symlink the `localsend` command into this directory.
    InstallCli { dir: PathBuf },
    /// Remove the `localsend` command.
    UninstallCli { dir: PathBuf },
    /// Say where the `localsend` command is, or that it is not installed.
    WhereIsCli,
    /// Write a macOS `.app` bundle around this executable.
    Bundle { dir: PathBuf },
    /// Run the MCP server on stdin/stdout for an AI client.
    Mcp(McpArgs),
    /// The arguments made no sense; the string is the sentence to print.
    Invalid(String),
}

/// What `localsend-gpui mcp` was asked to do.
///
/// Parsed here rather than with clap because this half of the binary must build
/// without the MCP feature: the parse is a handful of `--flags`, and keeping it
/// unconditional means `mcp --help` is helpful even in a build that cannot run
/// the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpArgs {
    /// Hold each offer for an answer instead of accepting it.
    pub ask: bool,
    /// Do not listen at all; sending and discovery only.
    pub no_receive: bool,
    /// Where received files land. Defaults to the window's setting.
    pub save_dir: Option<PathBuf>,
    /// Where the node keeps its TLS identity and inbox journal.
    pub state_dir: Option<PathBuf>,
    /// The name peers see. Defaults to the window's alias, plus ` (MCP)`.
    pub alias: Option<String>,
    /// The port to listen on. Defaults to the window's setting.
    pub port: Option<u16>,
    /// Speak HTTPS. Defaults to the window's setting.
    pub https: bool,
    pub help: bool,
}

impl Default for McpArgs {
    fn default() -> Self {
        Self {
            ask: false,
            no_receive: false,
            save_dir: None,
            state_dir: None,
            alias: None,
            port: None,
            https: true,
            help: false,
        }
    }
}

impl McpArgs {
    /// Read the words after `mcp`.
    ///
    /// `--ask` and `--no-receive` contradict each other, and accepting the pair
    /// would mean silently ignoring one of them, so it is refused.
    pub fn parse<'a>(mut rest: impl Iterator<Item = &'a OsString>) -> Result<Self, String> {
        let mut args = Self::default();
        while let Some(arg) = rest.next() {
            let text = arg.to_string_lossy().to_string();
            let (flag, inline) = match text.split_once('=') {
                Some((flag, value)) => (flag.to_string(), Some(value.to_string())),
                None => (text.clone(), None),
            };
            let mut value = |name: &str| -> Result<String, String> {
                if let Some(value) = inline.clone() {
                    if value.is_empty() {
                        return Err(format!("{name} needs a value."));
                    }
                    return Ok(value);
                }
                match rest.next() {
                    Some(next) => Ok(next.to_string_lossy().to_string()),
                    None => Err(format!("{name} needs a value.")),
                }
            };
            match flag.as_str() {
                "--ask" => args.ask = true,
                "--no-receive" | "--send-only" => args.no_receive = true,
                "--https" => args.https = true,
                "--no-https" => args.https = false,
                "--help" | "-h" => args.help = true,
                "--alias" => args.alias = Some(value("--alias")?),
                "--dir" | "--directory" => args.save_dir = Some(PathBuf::from(value("--dir")?)),
                "--state-dir" => args.state_dir = Some(PathBuf::from(value("--state-dir")?)),
                "--port" => {
                    let raw = value("--port")?;
                    let port: u16 = raw
                        .parse()
                        .map_err(|_| format!("`{raw}` is not a port number."))?;
                    args.port = Some(port);
                }
                other if other.starts_with('-') => {
                    return Err(format!("unknown option `{other}` for `mcp`"));
                }
                other => return Err(format!("unexpected argument `{other}` for `mcp`")),
            }
        }
        if args.ask && args.no_receive {
            return Err(
                "`--ask` is about receiving, so it cannot be combined with `--no-receive`."
                    .to_string(),
            );
        }
        Ok(args)
    }

    /// Whether this session listens for transfers at all.
    pub fn receives(&self) -> bool {
        !self.no_receive
    }
}

/// The words the library CLI owns. Kept in sync with `localsend_rs::cli::Commands`
/// by a test that parses each of them.
pub const CLI_COMMANDS: &[&str] = &["discover", "receive", "send", "tui"];

/// Outside a terminal a bare launch must not open a window nobody asked for…
/// except that it is a desktop app, so a bare launch is exactly the window.
pub fn classify(args: &[OsString]) -> Entry {
    let mut rest = args.iter().skip(1);
    let Some(first) = rest.next() else {
        return Entry::Gui;
    };
    let word = first.to_string_lossy();

    match word.as_ref() {
        "--help" | "-h" | "help" => Entry::Help,
        "--version" | "-V" => Entry::Version,
        "gui" => Entry::Gui,
        "install-cli" => match flag_value(rest, "--dir") {
            Ok(Some(dir)) => Entry::InstallCli {
                dir: PathBuf::from(dir),
            },
            Ok(None) => Entry::InstallCli {
                dir: default_install_dir(),
            },
            Err(message) => Entry::Invalid(message),
        },
        "uninstall-cli" => match flag_value(rest, "--dir") {
            Ok(Some(dir)) => Entry::UninstallCli {
                dir: PathBuf::from(dir),
            },
            Ok(None) => Entry::UninstallCli {
                dir: default_install_dir(),
            },
            Err(message) => Entry::Invalid(message),
        },
        "bundle" => match flag_value(rest, "--dir") {
            Ok(Some(dir)) => Entry::Bundle {
                dir: PathBuf::from(dir),
            },
            Ok(None) => Entry::Bundle {
                dir: default_bundle_dir(),
            },
            Err(message) => Entry::Invalid(message),
        },
        "where-is-cli" => Entry::WhereIsCli,
        "mcp" => match McpArgs::parse(rest) {
            Ok(args) => Entry::Mcp(args),
            Err(message) => Entry::Invalid(message),
        },
        other if CLI_COMMANDS.contains(&other) => Entry::Cli(args.to_vec()),
        other => Entry::Invalid(format!(
            "`{other}` is not a command. Try `localsend-gpui --help`."
        )),
    }
}

/// Read `--dir <path>` (or `--dir=<path>`) out of the remaining words.
fn flag_value<'a>(
    mut rest: impl Iterator<Item = &'a OsString>,
    flag: &str,
) -> Result<Option<OsString>, String> {
    let Some(arg) = rest.next() else {
        return Ok(None);
    };
    let text = arg.to_string_lossy();
    if text == flag {
        return match rest.next() {
            Some(value) => Ok(Some(value.clone())),
            None => Err(format!("{flag} needs a directory after it.")),
        };
    }
    if let Some(value) = text.strip_prefix(&format!("{flag}=")) {
        if value.is_empty() {
            return Err(format!("{flag} needs a directory after it."));
        }
        return Ok(Some(OsString::from(value)));
    }
    if text.starts_with('-') {
        return Err(format!("unknown option `{text}`"));
    }
    Err(format!("unexpected argument `{text}`"))
}

/// Where a `code`-style command goes when nobody says otherwise.
///
/// `~/.local/bin` is the one directory a user can write to that every modern
/// shell puts on PATH; `/usr/local/bin` would need sudo, and asking for a
/// password to install a symlink is not a good first experience.
pub fn default_install_dir() -> PathBuf {
    home()
        .map(|home| home.join(".local").join("bin"))
        .unwrap_or_else(|| PathBuf::from(".local/bin"))
}

/// Where `bundle` puts the app when nobody says otherwise: the per-user
/// Applications folder, which needs no administrator password.
pub fn default_bundle_dir() -> PathBuf {
    home()
        .map(|home| home.join("Applications"))
        .unwrap_or_else(|| PathBuf::from("Applications"))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// The name the command gets on PATH.
pub const COMMAND_NAME: &str = "localsend";

/// What `install-cli` did, so it can be reported (and asserted in tests).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub link: PathBuf,
    /// It was already there and pointed somewhere else.
    pub replaced: bool,
    /// The directory is on this process's PATH.
    pub on_path: bool,
}

/// Put a `localsend` symlink in `dir` pointing at `exe`.
///
/// Idempotent: a link that already points at the same file is left alone, a link
/// that points elsewhere is replaced (that is an upgrade, not a conflict), and a
/// *regular file* named `localsend` is refused rather than clobbered — it might be
/// somebody's script.
pub fn install_cli(dir: &Path, exe: &Path) -> std::io::Result<Installed> {
    std::fs::create_dir_all(dir)?;
    let link = dir.join(COMMAND_NAME);

    let mut replaced = false;
    match std::fs::symlink_metadata(&link) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let current = std::fs::read_link(&link).unwrap_or_default();
            if current == exe {
                return Ok(Installed {
                    on_path: on_path(dir),
                    link,
                    replaced,
                });
            }
            std::fs::remove_file(&link)?;
            replaced = true;
        }
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "{} already exists and is not a link. Move it away first.",
                    link.display()
                ),
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    symlink(exe, &link)?;
    Ok(Installed {
        on_path: on_path(dir),
        link,
        replaced,
    })
}

/// Remove the `localsend` symlink from `dir`. `Ok(false)` means there was none.
pub fn uninstall_cli(dir: &Path) -> std::io::Result<bool> {
    let link = dir.join(COMMAND_NAME);
    match std::fs::symlink_metadata(&link) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            std::fs::remove_file(&link)?;
            Ok(true)
        }
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "{} is not a link, so it was not created by this app. Not removing it.",
                link.display()
            ),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// The `localsend` command as this process can see it, if it can.
pub fn find_cli() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(COMMAND_NAME))
        .find(|candidate| candidate.exists())
}

fn on_path(dir: &Path) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|entry| entry == dir))
        .unwrap_or(false)
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    // Copying is the only thing that works without developer mode; the file name
    // picks up `.exe` automatically because `target` has it.
    std::fs::copy(target, link.with_extension("exe")).map(|_| ())
}

/// The text `--help` prints: both halves of the product in one screen.
pub fn help_text() -> String {
    format!(
        "\
localsend-gpui — LocalSend for this machine, window and command line in one binary

USAGE
  localsend-gpui                              open the window
  localsend-gpui gui                          open the window (explicit)
  localsend-gpui send <TARGET> <FILES>… [--pin <PIN>]
  localsend-gpui receive [--directory <DIR>] [--port <PORT>] [--pin <PIN>]
                         [--auto-accept] [--no-https]
  localsend-gpui discover [--timeout <SECONDS>] [--json]
  localsend-gpui tui                          the terminal interface

  localsend-gpui install-cli [--dir <DIR>]    link `{cmd}` onto PATH (default {default})
  localsend-gpui uninstall-cli [--dir <DIR>]  remove that link
  localsend-gpui where-is-cli                 say where `{cmd}` is

  localsend-gpui bundle [--dir <DIR>]         write `{app}` (default {bundle_dir})

  localsend-gpui mcp [--ask | --no-receive] [--dir <DIR>] [--port <PORT>]
                     [--no-https] [--alias <NAME>] [--state-dir <DIR>]
                                              MCP server for an AI client (stdio)

  localsend-gpui --help | --version

Every command is the same code the window runs: `send` and `receive` speak the
same protocol with the same settings and the same TLS identity. Run
`localsend-gpui send --help` for the flags of one command.
",
        cmd = COMMAND_NAME,
        app = crate::bundle::BUNDLE_NAME,
        default = default_install_dir().display(),
        bundle_dir = default_bundle_dir().display()
    )
}

/// The text `localsend-gpui mcp --help` prints.
pub fn mcp_help_text() -> String {
    "\
localsend-gpui mcp — a LocalSend server for AI clients, over MCP on stdin/stdout

USAGE
  localsend-gpui mcp [OPTIONS]

OPTIONS
  --ask               show each incoming transfer to the model instead of accepting it;
                      it is answered with accept_offer / decline_offer, and expires in 5 min
  --no-receive        send and discover only: do not listen for transfers at all
  --dir <DIR>         where received files land (default: the window's setting)
  --port <PORT>       listen here (default: the window's port, 53317)
  --no-https          speak plain HTTP instead of HTTPS
  --alias <NAME>      the name peers see (default: the window's alias + \" (MCP)\")
  --state-dir <DIR>   where this node keeps its TLS identity and inbox journal
                      (default: alongside the window's settings, in `mcp/`)
  --help              this text

WHAT A MODEL CAN DO
  list_devices, send_files, list_received, wait_for_received, list_offers,
  accept_offer, decline_offer, server_status — plus the resources
  localsend://inbox and localsend://devices.

RECEIVING
  Files addressed to this node are accepted automatically and written to the
  receive folder, because an AI session has nowhere to show an offer. Every
  arrival is appended to a journal, so `list_received` still answers after a
  restart, and `wait_for_received` blocks until something lands. MCP has no push
  notification to an idle model, so the long poll is the reliable path.

  This node is its own device: it does not share the window's port, identity or
  inbox. It can run beside the window and beside the official LocalSend app.

EXAMPLE CLIENT CONFIGURATION
  {
    \"mcpServers\": {
      \"localsend\": { \"command\": \"localsend-gpui\", \"args\": [\"mcp\"] }
    }
  }
"
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn a_bare_launch_opens_the_window_and_so_does_gui() {
        assert_eq!(classify(&args(&["localsend-gpui"])), Entry::Gui);
        assert_eq!(classify(&args(&["localsend-gpui", "gui"])), Entry::Gui);
    }

    #[test]
    fn library_commands_are_forwarded_untouched() {
        let argv = args(&["localsend-gpui", "send", "a.txt", "--to", "10.0.0.2:53317"]);
        match classify(&argv) {
            Entry::Cli(forwarded) => assert_eq!(forwarded, argv),
            other => panic!("expected a CLI forward, got {other:?}"),
        }
        for command in CLI_COMMANDS {
            assert!(
                matches!(classify(&args(&["localsend-gpui", command])), Entry::Cli(_)),
                "{command} must reach the library CLI"
            );
        }
    }

    #[test]
    fn help_and_version_are_ours() {
        assert_eq!(classify(&args(&["localsend-gpui", "--help"])), Entry::Help);
        assert_eq!(classify(&args(&["localsend-gpui", "-h"])), Entry::Help);
        assert_eq!(classify(&args(&["localsend-gpui", "help"])), Entry::Help);
        assert_eq!(
            classify(&args(&["localsend-gpui", "--version"])),
            Entry::Version
        );
    }

    #[test]
    fn install_cli_takes_an_optional_directory() {
        assert_eq!(
            classify(&args(&["localsend-gpui", "install-cli"])),
            Entry::InstallCli {
                dir: default_install_dir()
            }
        );
        assert_eq!(
            classify(&args(&[
                "localsend-gpui",
                "install-cli",
                "--dir",
                "/tmp/bin"
            ])),
            Entry::InstallCli {
                dir: PathBuf::from("/tmp/bin")
            }
        );
        assert_eq!(
            classify(&args(&["localsend-gpui", "install-cli", "--dir=/tmp/bin"])),
            Entry::InstallCli {
                dir: PathBuf::from("/tmp/bin")
            }
        );
        assert!(matches!(
            classify(&args(&["localsend-gpui", "install-cli", "--dir"])),
            Entry::Invalid(_)
        ));
    }

    #[test]
    fn an_unknown_word_is_refused_with_a_hint() {
        match classify(&args(&["localsend-gpui", "transmit"])) {
            Entry::Invalid(message) => assert!(message.contains("--help"), "{message}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn bundle_takes_an_optional_directory() {
        assert_eq!(
            classify(&args(&["localsend-gpui", "bundle"])),
            Entry::Bundle {
                dir: default_bundle_dir()
            }
        );
        assert_eq!(
            classify(&args(&["localsend-gpui", "bundle", "--dir", "/tmp/apps"])),
            Entry::Bundle {
                dir: PathBuf::from("/tmp/apps")
            }
        );
        assert!(matches!(
            classify(&args(&["localsend-gpui", "bundle", "--dir"])),
            Entry::Invalid(_)
        ));
    }

    #[test]
    fn help_text_names_every_entry_point() {
        let text = help_text();
        for needle in [
            "send",
            "receive",
            "discover",
            "tui",
            "install-cli",
            "bundle",
            "gui",
            "mcp",
        ] {
            assert!(text.contains(needle), "help is missing {needle}:\n{text}");
        }
    }

    #[test]
    fn mcp_defaults_to_receiving_and_accepting() {
        let mcp = match classify(&args(&["localsend-gpui", "mcp"])) {
            Entry::Mcp(mcp) => mcp,
            other => panic!("expected the MCP server, got {other:?}"),
        };
        assert!(mcp.receives());
        assert!(
            !mcp.ask,
            "auto-accept is the default, so files land unasked"
        );
        assert!(mcp.https);
        assert_eq!(mcp.port, None, "the window's port is the default");
    }

    #[test]
    fn mcp_flags_of_every_shape_are_read() {
        let mcp = match classify(&args(&[
            "localsend-gpui",
            "mcp",
            "--ask",
            "--dir",
            "/tmp/inbox",
            "--port=53318",
            "--no-https",
            "--alias",
            "Builder",
            "--state-dir=/tmp/mcp-state",
        ])) {
            Entry::Mcp(mcp) => mcp,
            other => panic!("expected the MCP server, got {other:?}"),
        };
        assert!(mcp.ask);
        assert!(mcp.receives());
        assert!(!mcp.https);
        assert_eq!(mcp.save_dir.as_deref(), Some(Path::new("/tmp/inbox")));
        assert_eq!(mcp.state_dir.as_deref(), Some(Path::new("/tmp/mcp-state")));
        assert_eq!(mcp.alias.as_deref(), Some("Builder"));
        assert_eq!(mcp.port, Some(53318));
    }

    #[test]
    fn a_send_only_node_cannot_also_ask_about_offers() {
        match classify(&args(&["localsend-gpui", "mcp", "--ask", "--no-receive"])) {
            Entry::Invalid(message) => assert!(message.contains("--no-receive"), "{message}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(matches!(
            classify(&args(&["localsend-gpui", "mcp", "--no-receive"])),
            Entry::Mcp(McpArgs {
                no_receive: true,
                ..
            })
        ));
    }

    #[test]
    fn a_bad_mcp_argument_is_refused_rather_than_ignored() {
        for argv in [
            vec!["localsend-gpui", "mcp", "--port"],
            vec!["localsend-gpui", "mcp", "--port", "not-a-port"],
            vec!["localsend-gpui", "mcp", "--dir"],
            vec!["localsend-gpui", "mcp", "--turbo"],
            vec!["localsend-gpui", "mcp", "extra"],
        ] {
            assert!(
                matches!(classify(&args(&argv)), Entry::Invalid(_)),
                "{argv:?} should be refused"
            );
        }
    }

    #[test]
    fn mcp_help_is_its_own_screen_and_explains_receiving() {
        let mcp = match classify(&args(&["localsend-gpui", "mcp", "--help"])) {
            Entry::Mcp(mcp) => mcp,
            other => panic!("expected mcp --help, got {other:?}"),
        };
        assert!(mcp.help);
        let text = mcp_help_text();
        for needle in [
            "list_devices",
            "send_files",
            "wait_for_received",
            "localsend://inbox",
            "--ask",
            "mcpServers",
        ] {
            assert!(text.contains(needle), "mcp help is missing {needle}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn install_cli_is_idempotent_and_never_clobbers_a_real_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let bin = dir.path().join("localsend-gpui");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();

        let first = install_cli(&dir.path().join("bin"), &bin).unwrap();
        assert!(!first.replaced);
        assert!(first.link.exists());
        assert_eq!(std::fs::read_link(&first.link).unwrap(), bin);

        // Again: no change, no error.
        let second = install_cli(&dir.path().join("bin"), &bin).unwrap();
        assert!(!second.replaced);

        // Pointing elsewhere: replaced, because that is an upgrade.
        let other = dir.path().join("other-gpui");
        std::fs::write(&other, b"#!/bin/sh\n").unwrap();
        let third = install_cli(&dir.path().join("bin"), &other).unwrap();
        assert!(third.replaced);
        assert_eq!(std::fs::read_link(&third.link).unwrap(), other);

        // A regular file with the command's name is somebody else's; refuse.
        let guarded = dir.path().join("guarded");
        std::fs::create_dir_all(&guarded).unwrap();
        std::fs::write(guarded.join(COMMAND_NAME), b"#!/bin/sh\n").unwrap();
        let error = install_cli(&guarded, &bin).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);

        // Uninstall removes our link, and reports when there is nothing to remove.
        assert!(uninstall_cli(&dir.path().join("bin")).unwrap());
        assert!(!uninstall_cli(&dir.path().join("bin")).unwrap());
        assert!(uninstall_cli(&guarded).is_err());
    }
}
