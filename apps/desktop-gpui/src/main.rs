//! The window bootstrap.
//!
//! `open_window` wraps the content view in the kit's `Root` overlay layer, so the
//! build closure returns the content view itself. `init` must run before any
//! window or component exists: it registers the component theme and the bundled
//! icons the widgets use.

use std::sync::OnceLock;

use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode, TitleBar};
use gpui_kit::*;
use localsend_gpui::app_state::DesktopApp;
use localsend_gpui::branding;
use localsend_gpui::entry::Entry;
use localsend_gpui::theme::{self, apply_macos_theme, ZoomIn, ZoomOut, ZoomReset};

actions!(localsend, [Quit]);

/// `LOCALSEND_GPUI_THEME=light|dark` pins the appearance. Without it the window
/// follows the system, including when the system changes while the app is open.
static THEME_OVERRIDE: OnceLock<ThemeMode> = OnceLock::new();

fn startup_theme_override() -> Option<ThemeMode> {
    match std::env::var(localsend_gpui::THEME_ENV).as_deref() {
        Ok("dark") => Some(ThemeMode::Dark),
        Ok("light") => Some(ThemeMode::Light),
        _ => None,
    }
}

/// Whether *the system* is dark, which is not the same question as whether the
/// window is dark: `LOCALSEND_GPUI_THEME` pins the window for screenshots, while
/// the Dock icon belongs to the Mac and macOS picks the tile that matches the
/// system. Reading the window's appearance (AppKit's, untouched by the theme
/// override) keeps the two from disagreeing.
fn system_is_dark(window: &Window) -> bool {
    appearance_is_dark(window.appearance())
}

/// The four AppKit appearances collapse to one bit: the vibrant variants are
/// still dark or light.
fn appearance_is_dark(appearance: WindowAppearance) -> bool {
    match appearance {
        WindowAppearance::Dark | WindowAppearance::VibrantDark => true,
        WindowAppearance::Light | WindowAppearance::VibrantLight => false,
    }
}

fn main() {
    // One binary, two products: argument words the library CLI owns run that
    // command, everything else opens the window. See `entry.rs`.
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    match localsend_gpui::entry::classify(&argv) {
        Entry::Gui => run_window(),
        Entry::Help => print!("{}", localsend_gpui::entry::help_text()),
        Entry::Version => println!("localsend-gpui {}", env!("CARGO_PKG_VERSION")),
        Entry::InstallCli { dir } => install_cli(&dir),
        Entry::UninstallCli { dir } => uninstall_cli(&dir),
        Entry::WhereIsCli => where_is_cli(),
        Entry::Bundle { dir } => make_bundle(&dir),
        Entry::Mcp(args) => run_mcp(args),
        Entry::Cli(_) => run_cli(argv),
        Entry::Invalid(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    }
}

/// Hand the whole command line to the library CLI, unchanged, on a runtime.
fn run_cli(argv: Vec<std::ffi::OsString>) {
    use clap::Parser;
    localsend_rs::crypto::ensure_crypto_provider();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime for the CLI");
    let result =
        runtime.block_on(async move { localsend_rs::cli::Cli::parse_from(argv).run().await });
    if let Err(error) = result {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

/// Run the MCP server for an AI client on stdin/stdout.
///
/// Same shape as `run_cli`: the library and rmcp are tokio-native, so one
/// runtime is built here and the window's GPUI executor is never involved.
#[cfg(feature = "mcp")]
fn run_mcp(args: localsend_gpui::entry::McpArgs) {
    if args.help {
        print!("{}", localsend_gpui::entry::mcp_help_text());
        return;
    }
    localsend_rs::crypto::ensure_crypto_provider();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime for the MCP server");

    let entry = localsend_gpui::entry::McpArgs {
        help: false,
        ..args
    };
    let config = localsend_gpui::mcp::config(&entry);
    if let Err(error) = runtime.block_on(localsend_gpui::mcp::run_stdio(config)) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

/// A build without the `mcp` feature still answers the subcommand, so the
/// person who typed it learns why it did nothing.
#[cfg(not(feature = "mcp"))]
fn run_mcp(args: localsend_gpui::entry::McpArgs) {
    if args.help {
        print!("{}", localsend_gpui::entry::mcp_help_text());
        return;
    }
    eprintln!(
        "this build of localsend-gpui was compiled without the MCP server \
         (cargo feature `mcp`); rebuild without `--no-default-features`."
    );
    std::process::exit(1);
}

fn install_cli(dir: &std::path::Path) {
    let exe = std::env::current_exe().expect("the running executable's path");
    match localsend_gpui::entry::install_cli(dir, &exe) {
        Ok(done) => {
            println!(
                "{} `{}` -> {}",
                if done.replaced { "Replaced" } else { "Linked" },
                localsend_gpui::entry::COMMAND_NAME,
                done.link.display()
            );
            println!("The window is unchanged: `localsend` runs this same binary.");
            if !done.on_path {
                println!(
                    "\n{} is not on PATH yet. Add this to ~/.zshrc:\n  export PATH=\"{}:$PATH\"",
                    dir.display(),
                    dir.display()
                );
            }
        }
        Err(error) => {
            eprintln!("Could not install the command: {error}");
            std::process::exit(1);
        }
    }
}

fn uninstall_cli(dir: &std::path::Path) {
    match localsend_gpui::entry::uninstall_cli(dir) {
        Ok(true) => println!("Removed {}/localsend", dir.display()),
        Ok(false) => println!(
            "Nothing to remove: {}/localsend does not exist",
            dir.display()
        ),
        Err(error) => {
            eprintln!("Could not remove the command: {error}");
            std::process::exit(1);
        }
    }
}

fn where_is_cli() {
    match localsend_gpui::entry::find_cli() {
        Some(path) => {
            println!("{}", path.display());
            // A link to a deleted build is worse than no link: say so.
            if let Ok(target) = std::fs::read_link(&path) {
                if !target.exists() {
                    eprintln!(
                        "warning: it points at {}, which no longer exists",
                        target.display()
                    );
                }
            }
        }
        None => {
            println!("`localsend` is not installed. Run `localsend-gpui install-cli`.");
            std::process::exit(1);
        }
    }
}

/// Write a `.app` around this executable and say how to open it.
fn make_bundle(dir: &std::path::Path) {
    use localsend_gpui::bundle;

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("cannot locate this executable: {error}");
            std::process::exit(1);
        }
    };
    match bundle::bundle(dir, &exe) {
        Ok(app) => {
            println!("Wrote {}", app.display());
            println!("Open it with: open \"{}\"", app.display());
        }
        Err(error) => {
            eprintln!("could not write {}: {error}", dir.display());
            std::process::exit(1);
        }
    }
}

fn run_window() {
    application().with_assets(app_assets()).run(|cx| {
        init(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        theme::bind_zoom(cx);
        cx.set_menus([
            Menu::new("LocalSend").items([MenuItem::action("Quit LocalSend", Quit)]),
            Menu::new("View").items([
                MenuItem::action("Zoom In", ZoomIn),
                MenuItem::action("Zoom Out", ZoomOut),
                MenuItem::Separator,
                MenuItem::action("Actual Size", ZoomReset),
            ]),
        ]);

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1180.), px(760.)),
                cx,
            ))),
            window_min_size: Some(size(px(900.), px(600.))),
            titlebar: Some(window_titlebar_options()),
            ..TitleBar::window_options()
        };

        open_window(options, cx, |window, cx| {
            window.set_rem_size(px(14.));
            Theme::sync_system_appearance(Some(window), cx);
            // Unbundled builds need an explicit Dock icon. Bundled builds keep
            // the system-sized, appearance-aware icon from their asset catalog.
            branding::install_dock_icon(system_is_dark(window));
            if let Some(mode) = startup_theme_override() {
                let _ = THEME_OVERRIDE.set(mode);
            }
            apply_macos_theme(
                THEME_OVERRIDE
                    .get()
                    .copied()
                    .unwrap_or_else(|| cx.theme().mode),
                cx,
            );
            window
                .observe_window_appearance(|window, cx| {
                    Theme::sync_system_appearance(Some(window), cx);
                    apply_macos_theme(
                        THEME_OVERRIDE
                            .get()
                            .copied()
                            .unwrap_or_else(|| cx.theme().mode),
                        cx,
                    );
                    // The Dock tile is part of "the app looks like this in this
                    // appearance", so it switches with the system, not only at
                    // launch. A bundle gets the same swap from `Assets.car`.
                    branding::install_dock_icon(system_is_dark(window));
                })
                .detach();
            cx.new(|cx| DesktopApp::new(window, cx))
        })
        .expect("failed to open the LocalSend window");
    });
}

fn window_titlebar_options() -> TitlebarOptions {
    let mut options = TitleBar::title_bar_options();
    options.title = Some("LocalSend".into());

    // These are native AppKit chrome coordinates, so keep them in points and
    // independent of the app's content zoom.
    #[cfg(target_os = "macos")]
    {
        options.traffic_light_position = Some(point(px(18.), px(21.)));
    }

    options
}

fn app_assets() -> assets::AllAssets {
    assets::AllAssets
}

#[cfg(test)]
mod tests {
    use super::appearance_is_dark;
    use gpui_kit::WindowAppearance;

    #[cfg(target_os = "macos")]
    use gpui_kit::{point, px};

    #[test]
    fn the_dock_icon_follows_the_system_not_the_window_override() {
        // Both dark appearances get the dark tile, both light ones the light
        // tile; the vibrant variants are the ones easiest to forget.
        assert!(appearance_is_dark(WindowAppearance::Dark));
        assert!(appearance_is_dark(WindowAppearance::VibrantDark));
        assert!(!appearance_is_dark(WindowAppearance::Light));
        assert!(!appearance_is_dark(WindowAppearance::VibrantLight));
    }

    #[test]
    fn macos_traffic_lights_have_top_and_left_padding() {
        let options = super::window_titlebar_options();

        #[cfg(target_os = "macos")]
        assert_eq!(
            options.traffic_light_position,
            Some(point(px(18.), px(21.)))
        );

        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            options.traffic_light_position,
            super::TitleBar::title_bar_options().traffic_light_position
        );
    }
}
