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

    /// The same, for a consumer that already **is** a LocalSend device.
    ///
    /// [`Self::new`] mints a fresh fingerprint, which is right for a scanner
    /// that is nothing else. It is wrong for a running receiver: the scan skips
    /// `local_device.fingerprint` so that a device does not discover itself,
    /// and a minted one never matches the value this process actually
    /// announces — so the caller's own machine comes back as a peer, gets a row
    /// in its own device list, and can be "sent to".
    pub fn for_device(device: DeviceInfo) -> Result<Self> {
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

    /// Probe a caller-supplied set of hosts over the normal LocalSend `/info`
    /// endpoint.  This is useful for routed networks where the caller knows a
    /// reachable address but cannot enumerate it through multicast; it still
    /// performs the same TLS/HTTP negotiation, response decoding and
    /// fingerprint-based de-duplication as a subnet scan.
    pub async fn scan_ips(&self, ips: Vec<String>) -> Result<Vec<DeviceInfo>> {
        Ok(self.scan_hosts(ips).await)
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
        self.probe_peer(ip, self.local_device.port).await
    }

    /// Ask ONE known peer, at ITS port, whether it is still there.
    ///
    /// The difference from a scan is which port is used. A sweep is looking for
    /// strangers and can only assume the well-known port, so it probes with its
    /// own. A peer we have already met answered somewhere specific, and that is
    /// not necessarily where we listen — a caller checking whether a known peer
    /// is still alive has to ask where the peer actually is.
    ///
    /// This is what a daemon's liveness check runs on. It is unicast, and every
    /// LocalSend client is required to serve it, which makes it evidence about
    /// a peer that does not depend on the peer volunteering anything.
    pub async fn probe_peer(&self, ip: &str, port: u16) -> Option<DeviceInfo> {
        for protocol in self.protocol_candidates() {
            match self.probe_info_with(ip, port, protocol).await {
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
    async fn probe_info_with(&self, ip: &str, port: u16, protocol: Protocol) -> ProbeOutcome {
        let url = format!("{}://{}:{}/api/localsend/v2/info", protocol, ip, port);
        let response = match self.client.get(&url).send().await {
            Ok(response) => response,
            // Connect failures/timeouts mean nothing is listening on this host — the other
            // scheme won't fare better, so signal the caller to stop probing this host.
            // A TLS handshake against an HTTP-only LocalSend server is reported by
            // reqwest as a connect error too. Only a timeout proves the host is
            // unreachable for both schemes; connection errors must try the fallback.
            Err(e) if e.is_timeout() => return ProbeOutcome::Unreachable,
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
        // The port we actually reached it on, not ours. The official app omits
        // `port` from `/info`, so the connection is the only evidence of where
        // this device answers — and stamping our own port here would hand the
        // caller an address that only works while both sides happen to use the
        // same one.
        device.port = port;
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

/// Every non-loopback IPv4 address this machine holds.
///
/// What a subnet sweep needs and what a caller must not be allowed to invent: a
/// scan is named by where **this** device is, so that nothing holding the
/// capability can point the probe at a network the operator is not on. A
/// multi-homed host has several, and each is a different segment.
pub fn local_ipv4_addresses() -> Result<Vec<std::net::Ipv4Addr>> {
    use if_addrs::{IfAddr, get_if_addrs};
    Ok(get_if_addrs()
        .map_err(|error| LocalSendError::network(format!("Failed to list interfaces: {error}")))?
        .into_iter()
        .filter(|interface| !interface.is_loopback())
        .filter_map(|interface| match interface.addr {
            IfAddr::V4(address) if !address.ip.is_unspecified() => Some(address.ip),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect())
}

/// A reqwest client tuned for LAN discovery: accepts the self-signed certificates that
/// every LocalSend device presents, and bounds each probe so the scan finishes promptly.
fn build_discovery_client() -> Result<Client> {
    crate::crypto::ensure_crypto_provider();
    Client::builder()
        // **No proxy**, for the reason `LocalSendClient::new` says: the hosts
        // being probed are on this segment and a proxy is not on the way to
        // them — and asking the platform for its proxy configuration measured
        // 20.4 s on macOS on 2026-08-22, which a scan of 254 hosts pays before
        // the first probe leaves.
        .no_proxy()
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
        // real devices use. This exercises the public explicit-target path used by
        // routed E2E environments.
        let discovery = HttpDiscovery::new("scanner".into(), server.port(), Protocol::Https)
            .expect("build discovery");
        let found = discovery
            .scan_ips(vec!["127.0.0.1".to_string()])
            .await
            .expect("scan explicit loopback target");

        let target = found
            .iter()
            .find(|d| d.fingerprint == expected_fingerprint)
            .expect("the self-signed HTTPS server must be discovered over TLS");
        assert_eq!(target.alias, "scan-target");
        assert_eq!(target.ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(target.port, server.port());
        assert_eq!(target.protocol, Protocol::Https);

        server.stop().await;
    }

    #[tokio::test]
    async fn https_scanner_falls_back_to_an_http_server() {
        use crate::{LocalSendServer, Protocol};

        let output = tempfile::tempdir().expect("output directory");
        let (mut server, _events) = LocalSendServer::builder()
            .alias("http-scan-target")
            .port(0)
            .save_dir(output.path())
            .protocol(Protocol::Http)
            .build()
            .await
            .expect("start HTTP receiver");
        let expected_fingerprint = server.device().fingerprint.clone();

        // CrossCopy advertises HTTPS, so its scanner tries TLS first. The TLS
        // handshake against this HTTP endpoint fails with reqwest's connect flag;
        // discovery must still retry the same port over HTTP.
        let discovery = HttpDiscovery::new("scanner".into(), server.port(), Protocol::Https)
            .expect("build discovery");
        let found = discovery.scan_hosts(vec!["127.0.0.1".to_string()]).await;

        let target = found
            .iter()
            .find(|device| device.fingerprint == expected_fingerprint)
            .expect("the HTTP server must be discovered after the HTTPS attempt");
        assert_eq!(target.alias, "http-scan-target");
        assert_eq!(target.protocol, Protocol::Http);

        server.stop().await;
    }

    /// A liveness check asks ONE known peer, at ITS port.
    ///
    /// The scan path takes the port from the scanner's own device because it is
    /// sweeping a subnet of strangers, all of which are assumed to be on the
    /// well-known port. A peer we have already met is different: we know where
    /// it answered, and that is not necessarily where we listen. The scanner
    /// here is deliberately configured with the wrong port, so a probe that
    /// used its own would find nothing.
    #[tokio::test]
    async fn probe_peer_asks_at_the_peers_own_port() {
        use crate::{LocalSendServer, Protocol};

        let output = tempfile::tempdir().expect("output directory");
        let (mut server, _events) = LocalSendServer::builder()
            .alias("probe-target")
            .port(0)
            .save_dir(output.path())
            .protocol(Protocol::Http)
            .build()
            .await
            .expect("start HTTP receiver");
        let expected_fingerprint = server.device().fingerprint.clone();
        let peer_port = server.port();

        // A port the peer is definitely NOT on.
        let wrong_port = peer_port.checked_add(1).expect("a spare port number");
        let discovery = HttpDiscovery::new("prober".into(), wrong_port, Protocol::Http)
            .expect("build discovery");

        let found = discovery
            .probe_peer("127.0.0.1", peer_port)
            .await
            .expect("the peer must answer at its own port");
        assert_eq!(found.fingerprint, expected_fingerprint);
        assert_eq!(found.port, peer_port);

        server.stop().await;
    }

    /// A probe of somewhere nothing is listening must answer "no", not hang or
    /// report a device: it is what tells liveness a peer has gone.
    #[tokio::test]
    async fn probe_peer_reports_nothing_when_the_peer_is_gone() {
        use crate::{LocalSendServer, Protocol};

        let output = tempfile::tempdir().expect("output directory");
        let (mut server, _events) = LocalSendServer::builder()
            .alias("departing")
            .port(0)
            .save_dir(output.path())
            .protocol(Protocol::Http)
            .build()
            .await
            .expect("start HTTP receiver");
        let peer_port = server.port();
        let discovery = HttpDiscovery::new("prober".into(), peer_port, Protocol::Http)
            .expect("build discovery");
        server.stop().await;

        assert!(
            discovery.probe_peer("127.0.0.1", peer_port).await.is_none(),
            "a stopped peer must not still be reported as answering"
        );
    }

    #[tokio::test]
    #[ignore = "requires CROSSCOPY_E2E_LOCALSEND_TARGET to name a reachable LocalSend peer"]
    async fn scan_ips_finds_an_explicit_e2e_peer() {
        use crate::Protocol;

        let target = std::env::var("CROSSCOPY_E2E_LOCALSEND_TARGET")
            .expect("set CROSSCOPY_E2E_LOCALSEND_TARGET to a LocalSend peer IP");
        let discovery = HttpDiscovery::new("e2e-scanner".into(), 53317, Protocol::Https)
            .expect("build discovery client");

        let found = discovery
            .scan_ips(vec![target.clone()])
            .await
            .expect("probe explicit E2E target");
        assert!(
            found
                .iter()
                .any(|peer| peer.ip.as_deref() == Some(target.as_str())),
            "the explicit LocalSend target must answer /info"
        );
    }
}
