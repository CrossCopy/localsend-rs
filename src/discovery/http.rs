#![allow(dead_code)]
use crate::core::device::{get_device_model, get_device_type};
use crate::crypto::generate_fingerprint;
use crate::discovery::Discovery;
use crate::error::LocalSendError;
use crate::protocol::{DEFAULT_HTTP_PORT, DeviceInfo, PROTOCOL_VERSION, Protocol};
use reqwest::Client;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::broadcast;

pub type Result<T> = std::result::Result<T, LocalSendError>;

pub struct HttpDiscovery {
    local_device: DeviceInfo,
    client: Client,
    running: Arc<AtomicBool>,
    tx: Option<broadcast::Sender<DeviceInfo>>,
}

impl HttpDiscovery {
    pub fn new(alias: String, port: u16, protocol: Protocol) -> Result<Self> {
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

        Ok(Self {
            local_device: device,
            client: Client::new(),
            running: Arc::new(AtomicBool::new(false)),
            tx: None,
        })
    }

    async fn scan_subnet(&self, base_ip: &str) -> Result<Vec<DeviceInfo>> {
        let base: Vec<u8> = base_ip
            .split('.')
            .map(|s| s.parse::<u8>().unwrap_or(0))
            .collect();

        let mut devices = Vec::new();

        for i in 1u8..=255 {
            let ip = format!("{}.{}.{}.{}", base[0], base[1], base[2], i);
            if let Ok(device) = self.try_register(&ip).await {
                devices.push(device);
            }
        }

        Ok(devices)
    }

    async fn try_register(&self, ip: &str) -> Result<DeviceInfo> {
        let url = format!(
            "{}://{}:{}/api/localsend/v2/register",
            self.local_device.protocol, ip, DEFAULT_HTTP_PORT
        );

        let response = self
            .client
            .post(&url)
            .json(&self.local_device)
            .send()
            .await?;

        if response.status().is_success() {
            let mut device: DeviceInfo = response.json().await?;
            device.ip = Some(ip.to_string());
            Ok(device)
        } else {
            Err(LocalSendError::network(format!(
                "Failed to register with {}: {}",
                ip,
                response.status()
            )))
        }
    }
}

#[async_trait::async_trait]
impl Discovery for HttpDiscovery {
    async fn start(&mut self) -> std::result::Result<(), LocalSendError> {
        if self.running.load(Ordering::Relaxed) {
            return Err(LocalSendError::network("Discovery already running"));
        }

        self.running.store(true, Ordering::Relaxed);

        let (tx, mut _rx) = broadcast::channel(100);
        self.tx = Some(tx.clone());

        let running = self.running.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            interval.tick().await;

            while running.load(Ordering::Relaxed) {
                interval.tick().await;
            }
        });

        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        self.tx = None;
    }

    async fn announce_presence(&self) -> std::result::Result<(), LocalSendError> {
        Err(LocalSendError::network(
            "HTTP discovery doesn't support announce",
        ))
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
