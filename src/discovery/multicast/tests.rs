use super::{
    AnnouncementSendSummary, MulticastConfig, MulticastDiscovery, select_interface_addresses,
    select_multicast_candidate_addresses,
};
use crate::LocalSendError;
use std::collections::BTreeSet;
use std::net::Ipv4Addr;

#[derive(Clone)]
struct TestInterface {
    name: String,
    address: Ipv4Addr,
    netmask: Ipv4Addr,
}

impl TestInterface {
    fn ipv4(name: &str, address: &str) -> Self {
        Self {
            name: name.into(),
            address: address.parse().unwrap(),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
        }
    }
}

#[test]
fn multicast_config_rejects_non_multicast_address() {
    let result = MulticastConfig::new("192.168.1.1".parse().unwrap(), 53317, None);
    assert!(matches!(
        result,
        Err(LocalSendError::InvalidMulticastAddress(_))
    ));
}

#[test]
fn one_unroutable_interface_does_not_fail_a_successful_announcement() {
    let mut summary = AnnouncementSendSummary::default();
    summary.record(Ok(128));
    summary.record(Err(std::io::Error::from_raw_os_error(65)));

    assert!(
        summary.finish().is_ok(),
        "a working LAN socket must survive an unrelated TUN with no route"
    );
}

#[test]
fn all_unroutable_interfaces_fail_the_announcement() {
    let mut summary = AnnouncementSendSummary::default();
    summary.record(Err(std::io::Error::from_raw_os_error(65)));
    summary.record(Err(std::io::Error::from_raw_os_error(51)));

    let error = summary
        .finish()
        .expect_err("an announcement with no successful interface must fail");
    assert!(error.to_string().contains("every interface"));
}

#[test]
fn an_all_failed_round_needs_recovery_but_a_later_success_does_not() {
    let mut summary = AnnouncementSendSummary::default();
    summary.record(Err(std::io::Error::from_raw_os_error(65)));
    assert!(summary.needs_recovery_retry());

    summary.record(Ok(128));
    assert!(!summary.needs_recovery_retry());
    assert!(summary.finish().is_ok());
}

#[test]
fn live_discovery_identity_can_toggle_browser_download_advertising() {
    let mut original = crate::DeviceInfo::new("CrossCopy".into(), 53317, crate::Protocol::Http);
    original.download = false;
    let mut discovery = MulticastDiscovery::new_with_device(original.clone());
    original.download = true;

    discovery.set_local_device(original.clone());

    assert_eq!(discovery.local_device, original);
}

#[test]
fn interface_filter_keeps_only_named_ipv4_interfaces() {
    let interfaces = vec![
        TestInterface::ipv4("en0", "192.168.1.10"),
        TestInterface::ipv4("utun3", "10.0.0.2"),
    ];
    let selected = select_interface_addresses(
        interfaces
            .into_iter()
            .map(|interface| (interface.name, interface.address, interface.netmask)),
        Some(&BTreeSet::from(["en0".into()])),
        None,
    );
    assert_eq!(selected, vec!["192.168.1.10".parse::<Ipv4Addr>().unwrap()]);
}

#[test]
fn a_tunnel_primary_does_not_hide_a_physical_lan_candidate() {
    let selected = select_multicast_candidate_addresses(
        [
            (
                "utun5".into(),
                Ipv4Addr::new(198, 18, 0, 1),
                Ipv4Addr::new(255, 0, 0, 0),
            ),
            (
                "en0".into(),
                Ipv4Addr::new(192, 168, 6, 50),
                Ipv4Addr::new(255, 255, 255, 0),
            ),
        ],
        None,
        Some(Ipv4Addr::new(198, 18, 0, 1)),
    );

    assert_eq!(
        selected,
        vec![Ipv4Addr::new(198, 18, 0, 1), Ipv4Addr::new(192, 168, 6, 50),]
    );
}

#[cfg(feature = "https")]
use crate::{DeviceInfo, LocalSendServer, Protocol};

#[cfg(feature = "https")]
#[tokio::test]
async fn announcement_client_pins_the_discovered_https_certificate() {
    let output = tempfile::tempdir().expect("output directory");
    let (mut server, _events) = LocalSendServer::builder()
        .alias("discovered HTTPS peer")
        .port(0)
        .save_dir(output.path())
        .protocol(Protocol::Https)
        .build()
        .await
        .expect("start HTTPS receiver");

    let mut peer = server.device().clone();
    peer.ip = Some("127.0.0.1".into());
    let local = DeviceInfo::new("discovery client".into(), 0, Protocol::Https);
    let client = MulticastDiscovery::client_for_announcement(local, &peer)
        .expect("build a client for the announced peer");

    client
        .register(&peer)
        .await
        .expect("the announced certificate fingerprint should be pinned");

    server.stop().await;
}

#[test]
fn multicast_uses_each_interface_on_the_primary_lan_only() {
    assert_eq!(
        select_interface_addresses(
            [
                (
                    "unspecified".into(),
                    Ipv4Addr::UNSPECIFIED,
                    Ipv4Addr::new(255, 255, 255, 0),
                ),
                (
                    "loopback".into(),
                    Ipv4Addr::LOCALHOST,
                    Ipv4Addr::new(255, 0, 0, 0),
                ),
                (
                    "en0".into(),
                    Ipv4Addr::new(192, 168, 6, 10),
                    Ipv4Addr::new(255, 255, 255, 0),
                ),
                (
                    "en1".into(),
                    Ipv4Addr::new(192, 168, 6, 101),
                    Ipv4Addr::new(255, 255, 255, 0),
                ),
                (
                    "bridge0".into(),
                    Ipv4Addr::new(192, 168, 139, 3),
                    Ipv4Addr::new(255, 255, 254, 0),
                ),
            ],
            None,
            Some(Ipv4Addr::new(192, 168, 6, 101))
        ),
        vec![
            Ipv4Addr::new(192, 168, 6, 10),
            Ipv4Addr::new(192, 168, 6, 101),
        ]
    );
}
