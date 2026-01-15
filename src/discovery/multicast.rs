use crate::client::LocalSendClient;
use crate::crypto::generate_fingerprint;
use crate::device::{get_device_model, get_device_type};
use crate::discovery::Discovery;
use crate::error::LocalSendError;
use crate::protocol::{
    AnnouncementMessage, DEFAULT_MULTICAST_ADDRESS, DEFAULT_MULTICAST_PORT, DeviceInfo,
    PROTOCOL_VERSION,
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::broadcast;

pub type Result<T> = std::result::Result<T, LocalSendError>;

pub struct MulticastDiscovery {
    local_device: DeviceInfo,
    client: Option<LocalSendClient>,
    socket: Option<Arc<UdpSocket>>,
    running: Arc<AtomicBool>,
    tx: Option<broadcast::Sender<DeviceInfo>>,
}

impl MulticastDiscovery {
    pub fn new(alias: String, port: u16, protocol: String) -> Result<Self> {
        let device = DeviceInfo {
            alias,
            version: PROTOCOL_VERSION.to_string(),
            device_model: Some(get_device_model()),
            device_type: Some(get_device_type()),
            fingerprint: generate_fingerprint(),
            port,
            protocol,
            download: false,
            ip: None,
        };

        Ok(Self::new_with_device(device))
    }

    pub fn new_with_device(device: DeviceInfo) -> Self {
        let (tx, _rx) = broadcast::channel(100);
        Self {
            local_device: device.clone(),
            client: Some(LocalSendClient::new(device)),
            socket: None,
            running: Arc::new(AtomicBool::new(false)),
            tx: Some(tx),
        }
    }
}

#[async_trait::async_trait]
impl Discovery for MulticastDiscovery {
    async fn start(&mut self) -> std::result::Result<(), LocalSendError> {
        if self.running.load(Ordering::Relaxed) {
            return Err(LocalSendError::Network(
                "Discovery already running".to_string(),
            ));
        }

        let socket = UdpSocket::bind(
            format!("0.0.0.0:{}", DEFAULT_MULTICAST_PORT)
                .as_str()
                .parse::<SocketAddr>()?,
        )
        .await?;
        let multicast_addr: SocketAddr =
            format!("{}:{}", DEFAULT_MULTICAST_ADDRESS, DEFAULT_MULTICAST_PORT).parse()?;
        let multicast_ipv4 = match multicast_addr.ip() {
            std::net::IpAddr::V4(addr) => addr,
            _ => {
                return Err(LocalSendError::Network(
                    "Multicast address must be IPv4".to_string(),
                ));
            }
        };
        socket.join_multicast_v4(multicast_ipv4, std::net::Ipv4Addr::new(0, 0, 0, 0))?;

        let socket_arc = Arc::new(socket);
        self.socket = Some(socket_arc.clone());
        self.running.store(true, Ordering::Relaxed);

        let tx = self.tx.as_ref().unwrap().clone();

        let local_fingerprint = self.local_device.fingerprint.clone();
        let client = self.client.take().unwrap();
        let running = self.running.clone();
        let local_device = self.local_device.clone();

        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];

            while running.load(Ordering::Relaxed) {
                match tokio::time::timeout(Duration::from_secs(1), socket_arc.recv_from(&mut buf))
                    .await
                {
                    Ok(Ok((len, src))) => {
                        if len > 0 {
                            let msg = match String::from_utf8(buf[..len].to_vec()) {
                                Ok(s) => s,
                                Err(_) => continue,
                            };

                            if let Ok(announcement) =
                                serde_json::from_str::<AnnouncementMessage>(&msg)
                            {
                                // Ignore self-announcements
                                if announcement.fingerprint == local_fingerprint {
                                    continue;
                                }

                                let device = DeviceInfo {
                                    alias: announcement.alias.clone(),
                                    version: announcement.version.clone(),
                                    device_model: announcement.device_model.clone(),
                                    device_type: announcement.device_type,
                                    fingerprint: announcement.fingerprint.clone(),
                                    port: announcement.port,
                                    protocol: announcement.protocol.clone(),
                                    download: announcement.download,
                                    ip: Some(src.ip().to_string()),
                                };

                                let is_announcement = announcement.announce
                                    || announcement.announcement.unwrap_or(false);

                                // Notify new device
                                let _ = tx.send(device.clone());

                                // If this is an announcement from another device, respond to it
                                if is_announcement {
                                    let client = client.clone();
                                    let local_device = local_device.clone();
                                    let socket = socket_arc.clone();

                                    tokio::spawn(async move {
                                        Self::respond_to_announcement(
                                            &client,
                                            &device,
                                            &local_device,
                                            &socket,
                                        )
                                        .await;
                                    });
                                }
                            }
                        }
                    }
                    Ok(Err(_)) | Err(_) => continue, // Continue on timeout or error
                }
            }
        });

        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        self.socket = None;
        self.tx = None;
    }

    async fn announce_presence(&self) -> std::result::Result<(), LocalSendError> {
        let socket = if let Some(ref s) = self.socket {
            s
        } else {
            return Err(LocalSendError::Network("Discovery not started".to_string()));
        };

        let announcement = AnnouncementMessage {
            alias: self.local_device.alias.clone(),
            version: self.local_device.version.clone(),
            device_model: self.local_device.device_model.clone(),
            device_type: self.local_device.device_type.clone(),
            fingerprint: self.local_device.fingerprint.clone(),
            port: self.local_device.port,
            protocol: self.local_device.protocol.clone(),
            download: self.local_device.download,
            announce: true,
            announcement: Some(true),
        };

        let msg = serde_json::to_string(&announcement)?;
        let buf = msg.as_bytes();
        let multicast_addr: SocketAddr =
            format!("{}:{}", DEFAULT_MULTICAST_ADDRESS, DEFAULT_MULTICAST_PORT).parse()?;

        // Send announcement multiple times with delays to improve reliability
        let delays = [100, 500, 2000];
        for delay in delays {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            socket.send_to(buf, &multicast_addr).await?;
        }

        Ok(())
    }

    fn on_discovered<F>(&mut self, callback: F)
    where
        F: Fn(DeviceInfo) + Send + Sync + 'static,
    {
        let tx = if let Some(ref t) = self.tx {
            t.clone()
        } else {
            return;
        };

        tokio::spawn(async move {
            let mut rx = tx.subscribe();
            while let Ok(device) = rx.recv().await {
                callback(device);
            }
        });
    }

    fn get_known_devices(&self) -> Vec<DeviceInfo> {
        vec![]
    }
}

impl MulticastDiscovery {
    async fn respond_to_announcement(
        client: &LocalSendClient,
        target_device: &DeviceInfo,
        local_device: &DeviceInfo,
        socket: &UdpSocket,
    ) {
        tracing::debug!(
            "Responding to announcement from {} ({:?})",
            target_device.alias,
            target_device.ip
        );

        // Try HTTP registration first
        match client.register(target_device).await {
            Ok(_) => {
                tracing::debug!(
                    "Successfully registered with {} via HTTP",
                    target_device.alias
                );
                return;
            }
            Err(e) => {
                // If HTTP failed, we just fall back to UDP. This is common if the other device
                // has a strict firewall or if we couldn't parse their response.
                // It's not a critical error.
                tracing::debug!("HTTP registration failed ({}), falling back to UDP...", e);
            }
        }

        // Fallback: Send UDP response
        let announcement = AnnouncementMessage {
            alias: local_device.alias.clone(),
            version: local_device.version.clone(),
            device_model: local_device.device_model.clone(),
            device_type: local_device.device_type.clone(),
            fingerprint: local_device.fingerprint.clone(),
            port: local_device.port,
            protocol: local_device.protocol.clone(),
            download: local_device.download,
            announce: false,
            announcement: Some(false),
        };

        if let Ok(msg) = serde_json::to_string(&announcement) {
            let buf = msg.as_bytes();
            let multicast_addr_str =
                format!("{}:{}", DEFAULT_MULTICAST_ADDRESS, DEFAULT_MULTICAST_PORT);
            if let Ok(multicast_addr) = multicast_addr_str.parse::<SocketAddr>() {
                if let Err(e) = socket.send_to(buf, &multicast_addr).await {
                    tracing::debug!("Failed to send UDP fallback response: {}", e);
                } else {
                    tracing::debug!("Sent UDP fallback response to multicast group");
                }
            }
        }
    }
}
