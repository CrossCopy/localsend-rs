use crate::discovery::traits::Discovery;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "receive", about = "Start LocalSend server to receive files")]
pub struct ReceiveCommand {
    #[arg(short, long, default_value = "./downloads")]
    directory: PathBuf,

    #[arg(short, long, default_value = "53317")]
    port: u16,

    #[arg(long)]
    pin: Option<String>,

    #[arg(long)]
    auto_accept: bool,

    /// Use plain HTTP instead of HTTPS. LocalSend uses HTTPS by default (matching
    /// the official app); pass this for easy interop/testing with HTTP-only peers.
    #[cfg(feature = "https")]
    #[arg(long)]
    no_https: bool,
}

pub async fn execute(command: ReceiveCommand) -> anyhow::Result<()> {
    if !command.directory.exists() {
        tokio::fs::create_dir_all(&command.directory).await?;
        println!(
            "Created download directory: {}",
            command.directory.display()
        );
    }

    println!("Starting LocalSend server on port {}", command.port);
    println!("Save directory: {}", command.directory.display());

    if let Some(ref pin) = command.pin {
        println!("PIN required: {}", pin);
    }

    if command.auto_accept {
        println!("Auto-accept mode ENABLED - files will be accepted without confirmation!");
    }

    #[cfg(feature = "https")]
    let https_enabled = !command.no_https;
    #[cfg(not(feature = "https"))]
    let https_enabled = false;

    println!(
        "Transport: {}",
        if https_enabled { "HTTPS" } else { "HTTP" }
    );

    let protocol_enum = if https_enabled {
        crate::protocol::Protocol::Https
    } else {
        crate::protocol::Protocol::Http
    };

    let mut builder = crate::server::LocalSendServer::builder()
        .alias("LocalSend-Rust".to_string())
        .port(command.port)
        .save_dir(&command.directory)
        .protocol(protocol_enum)
        .auto_accept(command.auto_accept);
    if let Some(ref pin) = command.pin {
        builder = builder.pin(pin.clone());
    }
    let (mut server, mut events) = builder.build().await?;

    // Discovery must announce the SAME device identity the server uses.
    let mut discovery =
        crate::discovery::MulticastDiscovery::new_with_device(server.device().clone());
    println!("Starting multicast discovery...");
    discovery.start().await?;
    println!("Announcing presence to network...");
    discovery.announce_presence().await?;

    let auto_accept = command.auto_accept;
    let event_loop = tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            match ev {
                crate::server::ServerEvent::TransferRequest(req) => {
                    println!(
                        "Incoming transfer from '{}' ({} file(s))",
                        req.sender().alias,
                        req.files().len()
                    );
                    if auto_accept {
                        req.accept();
                    } else {
                        // Headless interactive: y/n on stdin.
                        let accept = inquire::Confirm::new("Accept this transfer?")
                            .with_default(false)
                            .prompt()
                            .unwrap_or(false);
                        if accept { req.accept() } else { req.decline() }
                    }
                }
                crate::server::ServerEvent::FileReceived {
                    file_name,
                    path,
                    size,
                    sender_alias,
                    message_text,
                    ..
                } => {
                    if let Some(text) = message_text {
                        // A text message: show the body inline instead of just
                        // pointing at the .txt on disk.
                        println!("Message from {}: {}", sender_alias, text);
                        println!("  (saved to {})", path.display());
                    } else {
                        println!(
                            "Received '{}' ({} bytes) from {} -> {}",
                            file_name,
                            size,
                            sender_alias,
                            path.display()
                        );
                    }
                }
                crate::server::ServerEvent::SessionDone { session_id } => {
                    println!("Session {} complete", session_id);
                }
            }
        }
    });

    tokio::signal::ctrl_c().await?;

    println!("\nShutting down server...");
    event_loop.abort();
    server.stop();
    discovery.stop();

    Ok(())
}
