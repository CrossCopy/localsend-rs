//! TUI command for interactive terminal interface.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "tui", about = "Launch interactive TUI mode")]
pub struct TuiCommand {
    /// Port to listen on
    #[arg(short, long, default_value = "53317")]
    pub port: u16,

    /// Device alias name
    #[arg(short, long)]
    pub alias: Option<String>,

    /// Enable HTTPS
    #[cfg(feature = "https")]
    #[arg(long, default_value = "true")]
    pub https: bool,
}

pub async fn execute(command: TuiCommand) -> anyhow::Result<()> {
    #[cfg(feature = "https")]
    let https = command.https;
    #[cfg(not(feature = "https"))]
    let https = false;

    crate::tui::run_tui(command.port, command.alias, https)
        .await
        .map_err(|e| anyhow::anyhow!("TUI error: {}", e))
}
