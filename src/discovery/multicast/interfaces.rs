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

    let mut candidates = select_interface_addresses(addresses.iter().cloned(), None, primary);
    for address in select_interface_addresses(addresses, None, None) {
        if !candidates.contains(&address) {
            candidates.push(address);
        }
    }
    candidates
}

fn same_subnet(address: Ipv4Addr, primary: Ipv4Addr, netmask: Ipv4Addr) -> bool {
    let netmask = u32::from_be_bytes(netmask.octets());
    u32::from_be_bytes(address.octets()) & netmask == u32::from_be_bytes(primary.octets()) & netmask
}
