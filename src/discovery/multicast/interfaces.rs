use std::collections::BTreeSet;
use std::net::Ipv4Addr;

pub(super) fn select_interface_addresses(
    addresses: impl IntoIterator<Item = (String, Ipv4Addr, Ipv4Addr)>,
    interface_names: Option<&BTreeSet<String>>,
    primary: Option<Ipv4Addr>,
) -> Vec<Ipv4Addr> {
    addresses
        .into_iter()
        .filter(|(name, _, _)| interface_names.is_none_or(|names| names.contains(name)))
        .map(|(_, address, netmask)| (address, netmask))
        .filter(|(address, _)| !address.is_unspecified() && !address.is_loopback())
        .filter(|(address, netmask)| {
            primary.is_none_or(|primary| same_subnet(*address, primary, *netmask))
        })
        .map(|(address, _)| address)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(super) fn select_multicast_candidate_addresses(
    addresses: impl IntoIterator<Item = (String, Ipv4Addr, Ipv4Addr)>,
    interface_names: Option<&BTreeSet<String>>,
    primary: Option<Ipv4Addr>,
) -> Vec<Ipv4Addr> {
    let addresses = addresses.into_iter().collect::<Vec<_>>();
    if interface_names.is_some() {
        return select_interface_addresses(addresses, interface_names, None);
    }

    let addresses = without_container_bridges(addresses, primary);
    let mut candidates = select_interface_addresses(addresses.iter().cloned(), None, primary);
    for address in select_interface_addresses(addresses, None, None) {
        if !candidates.contains(&address) {
            candidates.push(address);
        }
    }
    candidates
}

/// Drop the host-side interfaces of container and VM runtimes.
///
/// This is not cosmetic. Each surviving address becomes a socket bound to
/// `0.0.0.0:<port>` with SO_REUSEADDR/SO_REUSEPORT, and the kernel matches
/// multicast delivery on (group, port) rather than on the joined interface — so
/// **one announcement is delivered to every one of those sockets**. A developer
/// machine with a dozen Docker networks therefore handles each announcement a
/// dozen extra times, and answers each copy.
///
/// None of that duplication can find a peer. Inside its network namespace a
/// container sees `lo` and `eth0`; the host-side `docker0`, `br-*` and `veth*`
/// exist only on the host, so a process in a container never announces from one
/// and never joins on one. Filtering them here costs exactly one case —
/// discovering a container from the *host* — and that case has an escape hatch:
/// naming interfaces explicitly returns above without consulting this at all.
///
/// The list is deliberately container/VM bridges only. Tunnels and overlays
/// (`utun`, `tun`, `wg`, `zt*`, `tailscale*`) carry real peers, macOS `bridge0`
/// is Internet Sharing, and narrowing any of those away is the failure
/// `a_tunnel_primary_does_not_hide_a_physical_lan_candidate` guards against.
///
/// Two invariants: the primary address is never dropped even if it does sit on
/// a bridge, and a host whose *only* addresses are bridges keeps all of them —
/// the filter may shrink a non-empty set, never empty one.
fn without_container_bridges(
    addresses: Vec<(String, Ipv4Addr, Ipv4Addr)>,
    primary: Option<Ipv4Addr>,
) -> Vec<(String, Ipv4Addr, Ipv4Addr)> {
    let kept = addresses
        .iter()
        .filter(|(name, address, _)| Some(*address) == primary || !is_container_bridge(name))
        .cloned()
        .collect::<Vec<_>>();

    if kept.is_empty() { addresses } else { kept }
}

fn is_container_bridge(name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "br-",     // Docker user-defined bridge networks
        "docker",  // docker0, docker_gwbridge
        "veth",    // host end of a container veth pair
        "virbr",   // libvirt
        "vboxnet", // VirtualBox host-only
        "vmnet",   // VMware
        "lxcbr", "lxdbr", // LXC / LXD
        "podman", "cni-", // Podman / CNI
    ];
    PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

fn same_subnet(address: Ipv4Addr, primary: Ipv4Addr, netmask: Ipv4Addr) -> bool {
    let netmask = u32::from_be_bytes(netmask.octets());
    u32::from_be_bytes(address.octets()) & netmask == u32::from_be_bytes(primary.octets()) & netmask
}
