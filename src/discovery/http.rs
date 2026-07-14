use crate::core::device::{get_device_model, get_device_type};
use crate::crypto::generate_fingerprint;
use crate::discovery::Discovery;
use crate::error::LocalSendError;
use crate::protocol::{DeviceInfo, PROTOCOL_VERSION, Protocol};
use futures_util::{StreamExt, stream};
use reqwest::Client;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::broadcast;

pub type Result<T> = std::result::Result<T, LocalSendError>;

/// Concurrent `/info` probes in flight during a subnet scan. Matches the official
/// LocalSend client and localsend-ts (`concurrency: 50`).
const SCAN_CONCURRENCY: usize = 50;

/// How long to wait for a host's TCP connect. Live LAN devices answer well within this;
/// unreachable hosts (most of a `/24`) are abandoned after it, so it bounds the scan's
/// wall-clock. A host that fails to connect is not retried on the other scheme.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1000);

/// Overall per-probe timeout (connect + response).
const REQUEST_TIMEOUT: Duration = Duration::from_millis(2000);

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
            client: build_discovery_client()?,
            running: Arc::new(AtomicBool::new(false)),
            tx: None,
        })
    }

    /// Sweeps every host `x.y.z.1..=254` in the `/24` subnet of `base_ip` (excluding
    /// our own address), asking each one `GET /api/localsend/v2/info`, and returns the
    /// LocalSend devices that answered. This is the protocol's "legacy" HTTP discovery
    /// (spec §2.2): it finds any device whose HTTP server is reachable, even one that is
    /// missing multicast (lossy Wi-Fi, or a mobile app suspended in the background).
    ///
    /// Probes run concurrently ([`SCAN_CONCURRENCY`] at a time). LocalSend devices use
    /// self-signed certificates, so the client accepts them — the peer's real fingerprint
    /// is read from the response, so nothing is trusted blindly. Mirrors localsend-ts
    /// `HttpDiscovery` and the official `HttpScanDiscoveryService`.
    pub async fn scan_subnet(&self, base_ip: &str) -> Result<Vec<DeviceInfo>> {
        Ok(self.scan_hosts(subnet_hosts(base_ip)?).await)
    }

    /// Probe an explicit list of hosts concurrently and return the de-duplicated set of
    /// LocalSend devices that answered (ourselves excluded). Shared core of `scan_subnet`.
    async fn scan_hosts(&self, targets: Vec<String>) -> Vec<DeviceInfo> {
        let discovered = stream::iter(targets)
            .map(|ip| async move { self.probe_info(&ip).await })
            .buffer_unordered(SCAN_CONCURRENCY)
            .filter_map(|device| async move { device })
            .collect::<Vec<_>>()
            .await;

        // Skip ourselves and de-duplicate by fingerprint (a peer may answer on more than
        // one address). Fingerprint is a peer's identity; a device without one is ignored.
        let mut seen = HashSet::new();
        let mut result = Vec::new();
        for device in discovered {
            if device.fingerprint.is_empty() || device.fingerprint == self.local_device.fingerprint
            {
                continue;
            }
            if seen.insert(device.fingerprint.clone()) {
                result.push(device);
            }
        }
        result
    }

    /// Probe a single host's `/info` endpoint. Tries the configured protocol first and,
    /// like localsend-ts, falls back to the other scheme so an HTTPS scan still finds an
    /// HTTP-only peer (and vice-versa). A host that is unreachable at the TCP level is not
    /// retried on the other scheme — it would fail there too — which keeps the scan fast
    /// over a subnet that is mostly empty.
    async fn probe_info(&self, ip: &str) -> Option<DeviceInfo> {
        for protocol in self.protocol_candidates() {
            match self.probe_info_with(ip, protocol).await {
                ProbeOutcome::Found(device) => return Some(device),
                ProbeOutcome::Unreachable => return None,
                ProbeOutcome::Miss => continue,
            }
        }
        None
    }

    fn protocol_candidates(&self) -> [Protocol; 2] {
        match self.local_device.protocol {
            Protocol::Https => [Protocol::Https, Protocol::Http],
            Protocol::Http => [Protocol::Http, Protocol::Https],
        }
    }

    /// `ip`/`port`/`protocol` on the returned device are taken from the connection we
    /// actually made, because the official app omits `port`/`protocol` from `/info`.
    async fn probe_info_with(&self, ip: &str, protocol: Protocol) -> ProbeOutcome {
        let url = format!(
            "{}://{}:{}/api/localsend/v2/info",
            protocol, ip, self.local_device.port
        );
        let response = match self.client.get(&url).send().await {
            Ok(response) => response,
            // Connect failures/timeouts mean nothing is listening on this host — the other
            // scheme won't fare better, so signal the caller to stop probing this host.
            Err(e) if e.is_connect() || e.is_timeout() => return ProbeOutcome::Unreachable,
            Err(_) => return ProbeOutcome::Miss,
        };
        if !response.status().is_success() {
            return ProbeOutcome::Miss;
        }
        let mut device: DeviceInfo = match response.json().await {
            Ok(device) => device,
            Err(_) => return ProbeOutcome::Miss,
        };
        device.ip = Some(ip.to_string());
        device.port = self.local_device.port;
        device.protocol = protocol;
        tracing::info!(
            "[DISCOVER/TCP] {} ({}, model: {:?})",
            device.alias,
            ip,
            device.device_model
        );
        ProbeOutcome::Found(device)
    }
}

/// Result of probing one host on one scheme.
enum ProbeOutcome {
    /// A LocalSend device answered.
    Found(DeviceInfo),
    /// Nothing is listening (connect failed/timed out) — don't try the other scheme.
    Unreachable,
    /// The host answered but not as a LocalSend peer on this scheme — try the next one.
    Miss,
}

/// Host addresses `x.y.z.1..=254` in the `/24` of `base_ip`, excluding `base_ip` itself.
fn subnet_hosts(base_ip: &str) -> Result<Vec<String>> {
    let octets: Vec<&str> = base_ip.split('.').collect();
    if octets.len() != 4 {
        return Err(LocalSendError::network(format!(
            "Invalid base IP for subnet scan: {base_ip}"
        )));
    }
    let prefix = format!("{}.{}.{}", octets[0], octets[1], octets[2]);
    Ok((1u8..=254)
        .map(|host| format!("{prefix}.{host}"))
        .filter(|ip| ip != base_ip)
        .collect())
}

/// A reqwest client tuned for LAN discovery: accepts the self-signed certificates that
/// every LocalSend device presents, and bounds each probe so the scan finishes promptly.
fn build_discovery_client() -> Result<Client> {
    Client::builder()
        .danger_accept_invalid_certs(true)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(LocalSendError::from)
}

#[async_trait::async_trait]
impl Discovery for HttpDiscovery {
    async fn start(&mut self) -> std::result::Result<(), LocalSendError> {
        if self.running.load(Ordering::Relaxed) {
            return Err(LocalSendError::network("Discovery already running"));
        }

        self.running.store(true, Ordering::Relaxed);

        let (tx, _rx) = broadcast::channel(100);
        self.tx = Some(tx);

        tracing::debug!("HttpDiscovery: passive; call scan_subnet() explicitly");

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

#[cfg(test)]
mod tests {
    use super::{HttpDiscovery, subnet_hosts};

    #[test]
    fn subnet_hosts_covers_1_to_254_excluding_self() {
        // `subnet_hosts` is a pure function; the address is an arbitrary input, not a real
        // host. Uses 192.0.2.0/24 (RFC 5737 TEST-NET-1, reserved for documentation) so the
        // test is obviously independent of whatever network the machine is on.
        let hosts = subnet_hosts("192.0.2.10").expect("valid base ip");

        // .1..=254 minus our own address.
        assert_eq!(hosts.len(), 253);
        assert!(!hosts.contains(&"192.0.2.10".to_string()));
        assert!(hosts.contains(&"192.0.2.1".to_string()));
        assert!(hosts.contains(&"192.0.2.254".to_string()));
        // Never the network/broadcast-ish .0 / .255.
        assert!(!hosts.contains(&"192.0.2.0".to_string()));
        assert!(!hosts.contains(&"192.0.2.255".to_string()));
    }

    #[test]
    fn subnet_hosts_rejects_a_malformed_base_ip() {
        assert!(subnet_hosts("not.an.ip").is_err());
        assert!(subnet_hosts("192.0.2").is_err());
    }

    #[cfg(feature = "https")]
    #[tokio::test]
    async fn scan_subnet_finds_a_self_signed_https_server() {
        use crate::{LocalSendServer, Protocol};

        let output = tempfile::tempdir().expect("output directory");
        let (mut server, _events) = LocalSendServer::builder()
            .alias("scan-target")
            .port(0)
            .save_dir(output.path())
            .protocol(Protocol::Https)
            .build()
            .await
            .expect("start HTTPS receiver");
        let expected_fingerprint = server.device().fingerprint.clone();

        // Probe just the loopback host the server is on, over the (self-signed) TLS the
        // real devices use. `scan_hosts` is the shared core of `scan_subnet`.
        let discovery = HttpDiscovery::new("scanner".into(), server.port(), Protocol::Https)
            .expect("build discovery");
        let found = discovery.scan_hosts(vec!["127.0.0.1".to_string()]).await;

        let target = found
            .iter()
            .find(|d| d.fingerprint == expected_fingerprint)
            .expect("the self-signed HTTPS server must be discovered over TLS");
        assert_eq!(target.alias, "scan-target");
        assert_eq!(target.ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(target.port, server.port());
        assert_eq!(target.protocol, Protocol::Https);

        server.stop();
    }
}
