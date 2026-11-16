//! An optional MCP server, so an AI client can move files over LocalSend.
//!
//! `localsend-gpui mcp` speaks the Model Context Protocol on stdin/stdout. It is
//! a child process of whichever AI client started it, not a mode of the window:
//! the window may be closed, and two processes listening on one port would fight.
//! So the session brings up its own LocalSend node — own alias (`<name> (MCP)`),
//! own TLS identity, own inbox journal — and leaves the window alone.
//!
//! The feature is on by default and can be dropped from a build:
//!
//! ```text
//! cargo build --no-default-features     # no rmcp, no MCP subcommand
//! ```
//!
//! Layering, outermost last:
//!
//! * [`inbox`] — the durable journal of what arrived;
//! * [`node`] — the LocalSend node: receiver, discovery, sending, offers;
//! * [`server`] — the MCP tools and resources a client sees.
//!
//! # Why receiving is worth implementing, and why notifications alone are not
//!
//! A model can only learn about the world during a tool call, so "tell me when a
//! file arrives" is either a long poll or nothing. The long poll is here
//! ([`server::LocalSendMcp`]'s `wait_for_received`), and it is reliable because
//! the arrival is written to a journal the poll reads: a file that landed
//! between two calls is still found, and one that landed while the process was
//! dead is still found next time. `notifications/resources/updated` is sent too,
//! for clients that subscribe, but the design does not depend on it.

pub mod inbox;
pub mod node;
pub mod server;

pub use inbox::ReceivedFile;
pub use node::{McpConfig, Node};
pub use server::LocalSendMcp;

use rmcp::ServiceExt;

use crate::entry::McpArgs;

/// Turn the command line into a node configuration, over the window's settings.
///
/// The window's saved preferences are the base — same alias, same receive folder,
/// same PIN — and the flags override them per session. Nothing is written back:
/// an MCP session must not silently change what the window will do next launch.
pub fn config(args: &McpArgs) -> McpConfig {
    let mut settings = crate::service::Settings::load();
    if let Some(alias) = &args.alias {
        settings.alias = alias.clone();
    }
    if let Some(dir) = &args.save_dir {
        settings.save_dir = dir.clone();
    }
    if let Some(port) = args.port {
        settings.port = port;
    }
    settings.https = args.https;

    let mut config = McpConfig::from_settings(settings);
    config.receive = !args.no_receive;
    config.auto_accept = !args.ask;
    if let Some(dir) = &args.state_dir {
        config.state_dir = dir.clone();
    }
    config
}

/// Run the server on stdin/stdout until the client closes the pipe.
///
/// Logging goes to stderr: stdout carries the protocol and one stray line would
/// corrupt the session. That is also why the startup line is a `stderr` write.
pub async fn run_stdio(config: McpConfig) -> Result<(), String> {
    let node = Node::start(config).await?;
    let status = node.status();
    eprintln!(
        "localsend-gpui mcp: `{}` {} on port {} over {}; inbox journal {}",
        status.alias,
        if status.listening {
            "listening"
        } else {
            "send-only"
        },
        status.port,
        status.protocol,
        status.journal
    );

    let service = LocalSendMcp::new(node)
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|error| format!("the MCP handshake failed: {error}"))?;
    service
        .waiting()
        .await
        .map_err(|error| format!("the MCP server task stopped: {error}"))?;
    eprintln!("localsend-gpui mcp: client closed the connection, stopping");
    Ok(())
}
