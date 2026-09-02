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

/// What a bounded sweep found, and whether it got to the end of its list.
///
/// **Two fields rather than a `Vec`**, because they are two facts and the second
/// one changes what the first means: an empty list from a completed sweep says
/// nobody is there, and an empty list from an abandoned one says nothing at all.
/// A caller shown only the devices cannot tell those apart, which is the whole
/// failure a deadline introduces if it is not reported.
#[derive(Debug, Clone)]
pub struct ScanOutcome {
    pub devices: Vec<DeviceInfo>,
    /// False when the deadline arrived before the last probe did. The probes
    /// still in flight were cancelled.
    pub complete: bool,
}

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

        Self::with_client_identity(device, None)
    }

    /// Builds a discovery client that presents the same identity as the local
    /// LocalSend receiver. Some iOS builds require a client certificate during
    /// the TLS handshake, including for discovery requests.
    #[cfg(feature = "https")]
    pub fn new_with_client_certificate(
        alias: String,
        port: u16,
        protocol: Protocol,
        certificate: &crate::crypto::TlsCertificate,
    ) -> Result<Self> {
        let device = DeviceInfo {
            alias,
            version: PROTOCOL_VERSION.to_string(),
            device_model: Some(get_device_model()),
            device_type: Some(get_device_type()),
            fingerprint: certificate.fingerprint.clone(),
            port,
            protocol,
            download: false,
            ip: None,
        };
        let pem = format!("{}\n{}", certificate.cert_pem, certificate.key_pem);
        let identity = reqwest::Identity::from_pem(pem.as_bytes()).map_err(LocalSendError::from)?;
        Self::with_client_identity(device, Some(identity))
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
        Self::with_client_identity(device, None)
    }

    fn with_client_identity(
        device: DeviceInfo,
        client_identity: Option<reqwest::Identity>,
    ) -> Result<Self> {
        Ok(Self {
            local_device: device,
            client: build_discovery_client(client_identity)?,
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
    /// A blind subnet scan never POSTs local device metadata to arbitrary hosts.
    pub async fn scan_subnet(&self, base_ip: &str) -> Result<Vec<DeviceInfo>> {
        Ok(self
            .scan_hosts(subnet_hosts(base_ip)?, None, false)
            .await
            .devices)
    }

    /// [`Self::scan_subnet`], abandoned after `within`.
    ///
    /// See [`ScanOutcome`] for why this returns one rather than a `Vec`, and
    /// [`Self::scan_hosts`] for what "abandoned" does to the probes still in
    /// flight.
    pub async fn scan_subnet_within(&self, base_ip: &str, within: Duration) -> Result<ScanOutcome> {
        Ok(self
            .scan_hosts(subnet_hosts(base_ip)?, Some(within), false)
            .await)
    }

    /// Probe a caller-supplied set of hosts over the normal LocalSend `/info`
    /// endpoint.  This is useful for routed networks where the caller knows a
    /// reachable address but cannot enumerate it through multicast; it still
    /// performs the same TLS/HTTP negotiation, response decoding and
    /// fingerprint-based de-duplication as a subnet scan.
    ///
    /// Because these targets were named by the caller rather than swept for,
    /// a host that reports no `/info` route falls back to `/register`, which
    /// discloses this device to it. A blind [`Self::scan_subnet`] never does.
    pub async fn scan_ips(&self, ips: Vec<String>) -> Result<Vec<DeviceInfo>> {
        Ok(self.scan_hosts(ips, None, true).await.devices)
    }

    /// [`Self::scan_ips`], abandoned after `within`.
    pub async fn scan_ips_within(&self, ips: Vec<String>, within: Duration) -> Result<ScanOutcome> {
        Ok(self.scan_hosts(ips, Some(within), true).await)
    }

    /// Probe an explicit list of hosts concurrently and return the de-duplicated set of
    /// LocalSend devices that answered (ourselves excluded). Shared core of `scan_subnet`.
    ///
    /// # What a deadline does, and what it deliberately does not do
    ///
    /// `within` bounds the **whole sweep**, not each probe — the per-probe
    /// bounds are [`CONNECT_TIMEOUT`] and [`REQUEST_TIMEOUT`] and they are
    /// unaffected. Results are taken from the stream as they arrive, so a
    /// deadline keeps everything that answered before it and reports
    /// [`ScanOutcome::complete`] as false; it never discards work already done.
    ///
    /// The probes still in flight are **cancelled**, because dropping the
    /// `buffer_unordered` stream drops the futures inside it. That is the
    /// reason this lives here rather than in a caller: a
    /// `tokio::time::timeout` wrapped around `scan_hosts` would return nothing
    /// at all while fifty sockets stayed open behind it, and a caller that
    /// wrapped the whole call would report *fewer* devices than answered with
    /// no way to tell that it had.
    async fn scan_hosts(
        &self,
        targets: Vec<String>,
        within: Option<Duration>,
        allow_register_fallback: bool,
    ) -> ScanOutcome {
        let deadline = within.map(|within| tokio::time::Instant::now() + within);
        let probes = stream::iter(targets)
            .map(|ip| async move { self.probe_info(&ip, allow_register_fallback).await })
            .buffer_unordered(SCAN_CONCURRENCY);
        futures_util::pin_mut!(probes);

        let mut discovered = Vec::new();
        let mut complete = true;
        loop {
            let next = probes.next();
            let answered = match deadline {
                Some(deadline) => match tokio::time::timeout_at(deadline, next).await {
                    Ok(answered) => answered,
                    Err(_) => {
                        complete = false;
                        break;
                    }
                },
                None => next.await,
            };
            match answered {
                Some(Some(device)) => discovered.push(device),
                // A host that did not answer. Still a completed probe.
                Some(None) => {}
                None => break,
            }
        }

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
        ScanOutcome {
            devices: result,
            complete,
        }
    }

    /// Probe a single host's `/info` endpoint. Tries the configured protocol first and,
    /// like localsend-ts, falls back to the other scheme so an HTTPS scan still finds an
    /// HTTP-only peer (and vice-versa). A host that is unreachable at the TCP level is not
    /// retried on the other scheme — it would fail there too — which keeps the scan fast
    /// over a subnet that is mostly empty.
    async fn probe_info(&self, ip: &str, allow_register_fallback: bool) -> Option<DeviceInfo> {
        for protocol in self.protocol_candidates() {
            match self
                .probe_info_with(ip, self.local_device.port, protocol)
                .await
            {
                ProbeOutcome::Found(device) => return Some(device),
                ProbeOutcome::Unreachable => return None,
                ProbeOutcome::RegisterFallback if allow_register_fallback => {
                    match self
                        .probe_register_with(ip, self.local_device.port, protocol)
                        .await
                    {
                        ProbeOutcome::Found(device) => return Some(device),
                        ProbeOutcome::Unreachable => return None,
                        ProbeOutcome::Miss | ProbeOutcome::RegisterFallback => continue,
                    }
                }
                ProbeOutcome::RegisterFallback | ProbeOutcome::Miss => continue,
            }
        }
        None
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
                // `/info` is the normal probe. An explicit peer can fall back
                // to `/register` when the endpoint reports that it is absent.
                ProbeOutcome::RegisterFallback => {
                    match self.probe_register_with(ip, port, protocol).await {
                        ProbeOutcome::Found(device) => return Some(device),
                        ProbeOutcome::Unreachable => return None,
                        ProbeOutcome::Miss | ProbeOutcome::RegisterFallback => continue,
                    }
                }
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
            return if register_fallback_status(response.status()) {
                ProbeOutcome::RegisterFallback
            } else {
                ProbeOutcome::Miss
            };
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

    /// Register with a peer when its `/info` endpoint is unavailable. This is
    /// the official LocalSend discovery fallback and is especially important
    /// for mobile receivers that only expose their authenticated registration
    /// route while foregrounded.
    async fn probe_register_with(&self, ip: &str, port: u16, protocol: Protocol) -> ProbeOutcome {
        let url = format!("{}://{}:{}/api/localsend/v2/register", protocol, ip, port);
        let response = match self.client.post(&url).json(&self.local_device).send().await {
            Ok(response) => response,
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
        device.port = port;
        device.protocol = protocol;
        tracing::info!(
            "[DISCOVER/REGISTER] {} ({}, model: {:?})",
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
    /// `/info` reported that an explicit peer should be queried through
    /// `/register` instead.
    RegisterFallback,
}

/// Whether an `/info` response means "this peer does not serve `/info`", as
/// opposed to "this peer served `/info` and said no".
///
/// Only the two answers that describe a missing route qualify. `401`/`403` are
/// deliberately excluded: they mean the route exists and refused us, and an
/// unauthenticated `/register` to the same peer would be refused for the same
/// reason — so the retry cannot succeed, and its only effect would be to post
/// this device's alias, model and fingerprint to a host that just declined to
/// talk to us.
fn register_fallback_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED
    )
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
    Ok(local_ipv4_interfaces()?
        .into_iter()
        .map(|(_, address)| address)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect())
}

/// Every non-loopback IPv4 address this machine holds, with the interface it is on.
///
/// The pair, because two parts of a LocalSend consumer speak different halves of
/// it and both are right: a sweep needs the **address** (it is what a person
/// reads off `ifconfig`, and what a `/24` is derived from), while joining a
/// multicast group needs the **interface name** (`MulticastConfig::interface_names`).
/// Deriving one from the other at the call site is how a policy expressed in one
/// vocabulary silently fails to apply in the other.
pub fn local_ipv4_interfaces() -> Result<Vec<(String, std::net::Ipv4Addr)>> {
    use if_addrs::{IfAddr, get_if_addrs};
    let mut found = get_if_addrs()
        .map_err(|error| LocalSendError::network(format!("Failed to list interfaces: {error}")))?
        .into_iter()
        .filter(|interface| !interface.is_loopback())
        .filter_map(|interface| match interface.addr {
            IfAddr::V4(address) if !address.ip.is_unspecified() => {
                Some((interface.name, address.ip))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    found.sort();
    found.dedup();
    Ok(found)
}

/// A reqwest client tuned for LAN discovery: accepts the self-signed certificates that
/// every LocalSend device presents, and bounds each probe so the scan finishes promptly.
fn build_discovery_client(client_identity: Option<reqwest::Identity>) -> Result<Client> {
    crate::crypto::ensure_crypto_provider();
    let mut builder = Client::builder()
        // **No proxy**, for the reason `LocalSendClient::new` says: the hosts
        // being probed are on this segment and a proxy is not on the way to
        // them — and asking the platform for its proxy configuration measured
        // 20.4 s on macOS on 2026-08-22, which a scan of 254 hosts pays before
        // the first probe leaves.
        .no_proxy()
        .tls_backend_rustls()
        .danger_accept_invalid_certs(true)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT);
    if let Some(identity) = client_identity {
        builder = builder.identity(identity);
    }
    builder.build().map_err(LocalSendError::from)
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

    #[test]
    fn register_fallback_is_limited_to_missing_info_routes() {
        assert!(super::register_fallback_status(
            reqwest::StatusCode::NOT_FOUND
        ));
        assert!(super::register_fallback_status(
            reqwest::StatusCode::METHOD_NOT_ALLOWED
        ));
        // A peer that answered and refused us is not a peer that is missing
        // `/info`; retrying against `/register` only discloses who we are.
        assert!(!super::register_fallback_status(
            reqwest::StatusCode::UNAUTHORIZED
        ));
        assert!(!super::register_fallback_status(
            reqwest::StatusCode::FORBIDDEN
        ));
        assert!(!super::register_fallback_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
    }

    /// A PIN-protected peer answers `/info` with 401. The scan must treat that
    /// as "not for us" and move on, rather than posting this device's identity
    /// to it.
    #[tokio::test]
    async fn a_pin_protected_peer_is_not_sent_our_identity() {
        use crate::Protocol;
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test server");
        let port = listener.local_addr().expect("server address").port();
        let server_task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept /info");
            let mut request = [0u8; 1024];
            let length = stream.read(&mut request).await.expect("read /info");
            assert!(
                String::from_utf8_lossy(&request[..length])
                    .starts_with("GET /api/localsend/v2/info")
            );
            stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write unauthorized response");

            // Anything that arrives next is the alternate-scheme probe, never a
            // registration carrying our alias and fingerprint.
            if let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(Duration::from_millis(250), listener.accept()).await
            {
                let length = stream.read(&mut request).await.unwrap_or(0);
                assert!(!String::from_utf8_lossy(&request[..length]).starts_with("POST "));
            }
        });

        let discovery =
            HttpDiscovery::new("scanner".into(), port, Protocol::Http).expect("build discovery");
        assert!(discovery.probe_peer("127.0.0.1", port).await.is_none());
        tokio::time::timeout(Duration::from_secs(1), server_task)
            .await
            .expect("server task did not finish")
            .expect("server task failed");
    }

    /// The positive half of the fallback: a peer that genuinely has no `/info`
    /// route is still found through `/register`.
    #[tokio::test]
    async fn a_peer_without_an_info_route_is_found_through_register() {
        use crate::{DeviceInfo, Protocol};
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test server");
        let port = listener.local_addr().expect("server address").port();
        let mut peer = DeviceInfo::new("register-only peer".into(), port, Protocol::Http);
        peer.fingerprint = "register-only-fingerprint".into();
        let served = peer.clone();

        let server_task = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("accept probe");
                let mut request = [0u8; 2048];
                let length = stream.read(&mut request).await.expect("read request");
                let request = String::from_utf8_lossy(&request[..length]).into_owned();
                let (status, body) = if request.starts_with("POST /api/localsend/v2/register") {
                    ("200 OK", serde_json::to_vec(&served).expect("encode peer"))
                } else {
                    ("404 Not Found", Vec::new())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
                stream.write_all(&body).await.expect("write body");
                if status == "200 OK" {
                    break;
                }
            }
        });

        let discovery =
            HttpDiscovery::new("scanner".into(), port, Protocol::Http).expect("build discovery");
        let found = discovery
            .probe_peer("127.0.0.1", port)
            .await
            .expect("a peer without /info must still be found through /register");

        assert_eq!(found.alias, "register-only peer");
        assert_eq!(found.ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(found.port, port);
        tokio::time::timeout(Duration::from_secs(1), server_task)
            .await
            .expect("server task did not finish")
            .expect("server task failed");
    }

    #[tokio::test]
    async fn server_errors_do_not_send_local_device_metadata_to_register() {
        use crate::Protocol;
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test server");
        let port = listener.local_addr().expect("server address").port();
        let server_task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept /info");
            let mut request = [0u8; 1024];
            let length = stream.read(&mut request).await.expect("read /info");
            assert!(
                String::from_utf8_lossy(&request[..length])
                    .starts_with("GET /api/localsend/v2/info")
            );
            stream
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .expect("write error response");

            // The second connection is the HTTPS fallback attempt. If every
            // non-2xx response triggered `/register`, these bytes would start
            // with an HTTP POST instead of a TLS ClientHello.
            let (mut stream, _) = listener.accept().await.expect("accept HTTPS fallback");
            let length = stream
                .read(&mut request)
                .await
                .expect("read HTTPS fallback");
            assert!(!String::from_utf8_lossy(&request[..length]).starts_with("POST "));
        });

        let discovery =
            HttpDiscovery::new("scanner".into(), port, Protocol::Http).expect("build discovery");
        assert!(discovery.probe_peer("127.0.0.1", port).await.is_none());
        tokio::time::timeout(Duration::from_secs(1), server_task)
            .await
            .expect("server task did not finish")
            .expect("server task failed");
    }

    #[tokio::test]
    async fn subnet_scans_do_not_register_with_a_generic_not_found_server() {
        use crate::Protocol;
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test server");
        let port = listener.local_addr().expect("server address").port();
        let server_task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept /info");
            let mut request = [0u8; 2048];
            let length = stream.read(&mut request).await.expect("read /info");
            assert!(
                String::from_utf8_lossy(&request[..length])
                    .starts_with("GET /api/localsend/v2/info")
            );
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write not-found response");

            let (mut stream, _) =
                tokio::time::timeout(Duration::from_millis(250), listener.accept())
                    .await
                    .expect("subnet scan may try the alternate scheme")
                    .expect("accept alternate scheme probe");
            let length = stream
                .read(&mut request)
                .await
                .expect("read alternate probe");
            assert!(!String::from_utf8_lossy(&request[..length]).starts_with("POST "));
        });

        let discovery =
            HttpDiscovery::new("scanner".into(), port, Protocol::Http).expect("build discovery");
        let outcome = discovery
            .scan_hosts(vec!["127.0.0.1".into()], None, false)
            .await;
        assert!(outcome.devices.is_empty());
        tokio::time::timeout(Duration::from_secs(1), server_task)
            .await
            .expect("server task did not finish")
            .expect("server task failed");
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
        let found = discovery
            .scan_hosts(vec!["127.0.0.1".to_string()], None, false)
            .await
            .devices;

        let target = found
            .iter()
            .find(|device| device.fingerprint == expected_fingerprint)
            .expect("the HTTP server must be discovered after the HTTPS attempt");
        assert_eq!(target.alias, "http-scan-target");
        assert_eq!(target.protocol, Protocol::Http);

        server.stop().await;
    }

    /// A mobile peer may reject an unauthenticated `/info` request during the
    /// TLS handshake but accept the protocol's authenticated `/register`
    /// fallback. This test requires both pieces: the test server requires a
    /// client certificate, and it only returns a device from `/register`.
    #[cfg(feature = "https")]
    #[tokio::test]
    async fn https_scan_presents_client_certificate_and_falls_back_to_register() {
        use crate::{DeviceInfo, Protocol, TlsCertificate, generate_tls_certificate};
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
        use rustls::server::WebPkiClientVerifier;
        use rustls::{RootCertStore, ServerConfig};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio_rustls::TlsAcceptor;

        fn private_key(certificate: &TlsCertificate) -> PrivateKeyDer<'static> {
            PrivateKeyDer::from_pem_slice(certificate.key_pem.as_bytes()).expect("read private key")
        }

        let server_certificate = generate_tls_certificate().expect("generate server certificate");
        let client_certificate = generate_tls_certificate().expect("generate client certificate");

        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(client_certificate.cert_der.clone()))
            .expect("trust the test client certificate");
        let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .expect("build client certificate verifier");
        let server_config = ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                vec![CertificateDer::from(server_certificate.cert_der.clone())],
                private_key(&server_certificate),
            )
            .expect("build mutual TLS server");
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test server");
        let port = listener.local_addr().expect("server address").port();
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let mut peer = DeviceInfo::new("mTLS register peer".into(), port, Protocol::Https);
        peer.fingerprint = server_certificate.fingerprint.clone();

        let server_task = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.expect("accept probe");
                let mut stream = acceptor.accept(stream).await.expect("client certificate");
                let mut request = [0u8; 8192];
                let length = stream.read(&mut request).await.expect("read request");
                let request = String::from_utf8_lossy(&request[..length]);
                let (status, body) = if request.starts_with("POST /api/localsend/v2/register") {
                    ("200 OK", serde_json::to_vec(&peer).expect("encode peer"))
                } else {
                    ("404 Not Found", Vec::new())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response headers");
                stream.write_all(&body).await.expect("write response body");
            }
        });

        let discovery = HttpDiscovery::new_with_client_certificate(
            "scanner".into(),
            port,
            Protocol::Https,
            &client_certificate,
        )
        .expect("build mTLS discovery client");
        let found = discovery
            .scan_ips(vec!["127.0.0.1".into()])
            .await
            .expect("scan mTLS target");

        let found_peer = found
            .iter()
            .find(|device| device.fingerprint == server_certificate.fingerprint)
            .expect("the peer must be found through /register");
        assert_eq!(found_peer.alias, "mTLS register peer");
        assert_eq!(found_peer.protocol, Protocol::Https);
        server_task.await.expect("test server completed");
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

    #[cfg(feature = "https")]
    #[tokio::test]
    #[ignore = "requires CROSSCOPY_E2E_LOCALSEND_TARGET to name a reachable LocalSend peer"]
    async fn scan_ips_finds_an_explicit_e2e_peer_with_client_certificate() {
        use crate::{Protocol, generate_tls_certificate};

        let target = std::env::var("CROSSCOPY_E2E_LOCALSEND_TARGET")
            .expect("set CROSSCOPY_E2E_LOCALSEND_TARGET to a LocalSend peer IP");
        let certificate = generate_tls_certificate().expect("generate client certificate");
        let discovery = HttpDiscovery::new_with_client_certificate(
            "e2e-scanner".into(),
            53317,
            Protocol::Https,
            &certificate,
        )
        .expect("build mTLS discovery client");

        let found = discovery
            .scan_ips(vec![target.clone()])
            .await
            .expect("probe explicit E2E target with client certificate");
        assert!(
            found
                .iter()
                .any(|peer| peer.ip.as_deref() == Some(target.as_str())),
            "the explicit LocalSend target must answer with client-certificate support"
        );
    }

    /// **A deadline keeps what answered and says it was cut short.**
    ///
    /// TEST-NET-1 is reserved for documentation and routed nowhere, so all 253
    /// hosts hang until `CONNECT_TIMEOUT` — one full second, and fifty at a
    /// time, which is roughly five seconds of wall clock. The row asserts the
    /// call comes back in well under that and reports `complete: false`.
    ///
    /// It is also the row that fails if somebody "simplifies" this into a
    /// `tokio::time::timeout` around the whole sweep: that version returns an
    /// error rather than an outcome, and the devices found before the deadline
    /// are lost.
    #[tokio::test]
    async fn a_deadline_abandons_the_sweep_and_says_so() {
        let discovery = HttpDiscovery::new(
            "a test device".to_string(),
            53317,
            crate::protocol::Protocol::Http,
        )
        .expect("a discovery client");

        let started = std::time::Instant::now();
        let outcome = discovery
            .scan_subnet_within("192.0.2.10", std::time::Duration::from_millis(300))
            .await
            .expect("a scan of a reserved range still runs");

        assert!(
            !outcome.complete,
            "253 unroutable hosts cannot all have been probed in 300ms"
        );
        assert!(outcome.devices.is_empty(), "nothing answers on TEST-NET-1");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "the deadline must abandon the sweep, not wait for its last probe: {:?}",
            started.elapsed()
        );
    }
}
