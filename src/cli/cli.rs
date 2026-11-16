#[cfg(feature = "tui")]
use crate::cli::commands::TuiCommand;
use crate::cli::commands::{DiscoverCommand, ReceiveCommand, SendCommand};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(about = "LocalSend client and CLI for Rust", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    Discover(DiscoverCommand),
    Receive(ReceiveCommand),
    Send(SendCommand),
    #[cfg(feature = "tui")]
    Tui(TuiCommand),
}

impl Cli {
    /// Run the command this parser produced.
    ///
    /// Both binaries call this: the library's own `localsend-rs` and the desktop
    /// app, which forwards any argument that is not its own flag. Keeping the
    /// dispatch here means the two entry points cannot drift apart in which
    /// commands exist or what they do.
    pub async fn run(self) -> anyhow::Result<()> {
        #[cfg(feature = "tui")]
        use crate::cli::run_tui;
        use crate::cli::{run_discover, run_receive, run_send};

        match self.command {
            Commands::Discover(cmd) => run_discover(cmd).await,
            Commands::Receive(cmd) => run_receive(cmd).await,
            Commands::Send(cmd) => run_send(cmd).await,
            #[cfg(feature = "tui")]
            Commands::Tui(cmd) => run_tui(cmd).await,
        }
    }
}
