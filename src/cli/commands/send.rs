use crate::DeviceInfo;
use crate::client::LocalSendClient;
use crate::crypto::generate_fingerprint;
use crate::discovery::{Discovery, MulticastDiscovery};
use crate::file::build_file_metadata;
use crate::protocol::types::FileMetadataDetails;
use crate::protocol::{DeviceType, FileMetadata};
use clap::Parser;
use reqwest::Client;
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "send", about = "Send files to a LocalSend device")]
pub struct SendCommand {
    target: String,

    #[arg(required = true)]
    files: Vec<String>, // Changed from PathBuf to String to support text

    #[arg(short, long)]
    pin: Option<String>,
}

pub async fn execute(command: SendCommand) -> anyhow::Result<()> {
    let target = resolve_target(&command.target).await?;
    println!("Sending to: {} ({:?})", target.alias, target.ip);

    let client = LocalSendClient::new(DeviceInfo {
        alias: "LocalSend-Rust".to_string(),
        version: "2.1".to_string(),
        device_model: Some(std::env::consts::OS.to_string()),
        device_type: Some(DeviceType::Desktop),
        fingerprint: generate_fingerprint(),
        port: 53318,
        protocol: "https".to_string(), // Default to HTTPS
        download: false,
        ip: None,
    });

    // Register first to ensure connection
    let _ = client.register(&target).await;

    let mut file_metadata_map: HashMap<String, FileSource> = HashMap::new();
    let mut files_metadata = HashMap::new();

    enum FileSource {
        Path(PathBuf),
        Text(String),
    }

    for input in &command.files {
        let path = PathBuf::from(input);
        if path.exists() {
            let file_meta = build_file_metadata(&path).await?;
            file_metadata_map.insert(file_meta.id.clone(), FileSource::Path(path));
            files_metadata.insert(file_meta.id.clone(), file_meta);
        } else {
            // Treat as text
            let text = input.clone();
            let id = uuid::Uuid::new_v4().to_string();
            let file_meta = FileMetadata {
                id: id.clone(),
                file_name: format!("{}.txt", id), // Random name or "message.txt"
                size: text.len() as u64,
                file_type: "text/plain".to_string(),
                sha256: None,
                preview: Some(text.clone()), // Preview is the text itself
                metadata: Some(FileMetadataDetails {
                    modified: None,
                    accessed: None,
                }),
            };
            file_metadata_map.insert(id.clone(), FileSource::Text(text));
            files_metadata.insert(id.clone(), file_meta);
        }
    }

    let upload_response = client
        .prepare_upload(&target, files_metadata, command.pin.as_deref())
        .await?;

    println!("Session ID: {}", upload_response.session_id);

    if upload_response.session_id.is_empty() {
        // 204 No Content - likely text message sent successfully
        println!("Transfer completed successfully (No files needed to be transferred).");
        return Ok(());
    }

    for (file_id, token) in &upload_response.files {
        let source = file_metadata_map
            .get(file_id)
            .ok_or_else(|| anyhow::anyhow!("File not found for ID: {}", file_id))?;

        match source {
            FileSource::Path(path) => {
                let file_size = tokio::fs::metadata(path).await?.len();
                println!("Uploading: {} ({} bytes)", path.display(), file_size);

                client
                    .upload_file(
                        &target,
                        &upload_response.session_id,
                        file_id,
                        token,
                        path,
                        None,
                    )
                    .await?;

                println!(
                    "Success: {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
            }
            FileSource::Text(text) => {
                println!("Sending text message: \"{}\"", text);
                // We need to write text to a temp file or modify client to accept bytes.
                // For now, let's just write to a temp file.
                let temp_dir = std::env::temp_dir();
                let temp_file = temp_dir.join(format!("localsend_text_{}.txt", file_id));
                tokio::fs::write(&temp_file, text.as_bytes()).await?;

                client
                    .upload_file(
                        &target,
                        &upload_response.session_id,
                        file_id,
                        token,
                        &temp_file,
                        None,
                    )
                    .await?;

                let _ = tokio::fs::remove_file(temp_file).await;
                println!("Success: Text message sent");
            }
        }
    }

    Ok(())
}

async fn resolve_target(target: &str) -> anyhow::Result<DeviceInfo> {
    // 1. Try if target is an IP address
    if let Ok(ip) = target.parse::<IpAddr>() {
        println!("Target is an IP address, probing directly...");
        if let Ok(device) = probe_device(ip.to_string()).await {
            return Ok(device);
        }
        println!("Direct probe failed, falling back to discovery...");
    }

    // 2. Use Multicast Discovery
    let mut discovery = MulticastDiscovery::new(
        "LocalSend-Rust-Finder".to_string(),
        53317,
        "https".to_string(),
    )?;

    let found_device = std::sync::Arc::new(std::sync::Mutex::new(None as Option<DeviceInfo>));
    let found_device_clone = found_device.clone();
    let target_owned = target.to_string();

    discovery.on_discovered(move |device: DeviceInfo| {
        let matches_alias = device.alias == target_owned;
        let matches_ip = device.ip.as_deref() == Some(&target_owned);

        if matches_alias || matches_ip {
            let mut found = found_device_clone.lock().unwrap();
            if found.is_none() {
                *found = Some(device);
            }
        }
    });

    discovery.start().await?;
    // Send announcement to trigger responses
    discovery.announce_presence().await?;

    println!("Searching for device '{}'...", target);
    for _ in 0..50 {
        // Wait up to 5 seconds
        if found_device.lock().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    discovery.stop();

    let found = found_device.lock().unwrap();
    if let Some(device) = found.clone() {
        Ok(device)
    } else {
        anyhow::bail!("Could not resolve target: {}", target);
    }
}

async fn probe_device(ip: String) -> anyhow::Result<DeviceInfo> {
    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(2))
        .build()?;

    // Try HTTPS first
    let url = format!("https://{}:53317/api/localsend/v2/info", ip);
    if let Ok(resp) = client.get(&url).send().await {
        if resp.status().is_success() {
            let mut device: DeviceInfo = resp.json().await?;
            device.ip = Some(ip.clone());
            device.protocol = "https".to_string(); // Ensure protocol is set matches what we used
            return Ok(device);
        }
    }

    // Try HTTP
    let url = format!("http://{}:53317/api/localsend/v2/info", ip);
    if let Ok(resp) = client.get(&url).send().await {
        if resp.status().is_success() {
            let mut device: DeviceInfo = resp.json().await?;
            device.ip = Some(ip.clone());
            device.protocol = "http".to_string();
            return Ok(device);
        }
    }

    anyhow::bail!("Failed to probe device at {}", ip)
}
