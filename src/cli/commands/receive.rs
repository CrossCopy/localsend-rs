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

    #[cfg(feature = "https")]
    #[arg(long)]
    https: bool,
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
    let https_enabled = command.https;
    #[cfg(not(feature = "https"))]
    let https_enabled = false;

    if https_enabled {
        println!("HTTPS mode ENABLED");
    }

    #[cfg(feature = "https")]
    let tls_cert = if https_enabled {
        Some(crate::crypto::generate_tls_certificate()?)
    } else {
        None
    };

    let fingerprint = if https_enabled {
        #[cfg(feature = "https")]
        {
            tls_cert.as_ref().unwrap().fingerprint.clone()
        }
        #[cfg(not(feature = "https"))]
        {
            crate::crypto::generate_fingerprint()
        }
    } else {
        crate::crypto::generate_fingerprint()
    };

    let protocol_enum = if https_enabled {
        crate::protocol::Protocol::Https
    } else {
        crate::protocol::Protocol::Http
    };

    let device = crate::protocol::DeviceInfo {
        alias: "LocalSend-Rust".to_string(),
        version: crate::protocol::PROTOCOL_VERSION.to_string(),
        device_model: Some(crate::device::get_device_model()),
        device_type: Some(crate::device::get_device_type()),
        fingerprint,
        port: command.port,
        protocol: protocol_enum,
        download: false,
        ip: None,
    };

    // Start multicast discovery
    let mut discovery = crate::discovery::MulticastDiscovery::new_with_device(device.clone());

    println!("Starting multicast discovery...");
    discovery.start().await?;
    discovery.on_discovered(|device| {
        println!(
            "Device discovered: {} (port: {})",
            device.alias, device.port
        );
    });

    // Announce our presence
    println!("Announcing presence to network...");
    discovery.announce_presence().await?;

    let pending_transfer = std::sync::Arc::new(std::sync::RwLock::new(None));
    let received_files = std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
    let mut server = crate::server::LocalSendServer::new_with_device(
        device,
        command.directory,
        https_enabled,
        pending_transfer,
        received_files,
    )?;

    #[cfg(feature = "https")]
    if let Some(cert) = tls_cert {
        server.set_tls_certificate(cert);
    }

    server.start(None).await?;

    tokio::signal::ctrl_c().await?;

    println!("\nShutting down server...");
    server.stop();
    discovery.stop();

    Ok(())
}
