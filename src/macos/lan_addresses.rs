//! This Mac's addresses on its local networks: what other Macs there reach it at.
use crate::tailnet::is_tailnet_address;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Every IPv4 and routable IPv6 address on an interface that is up, without
/// loopback, link-local and tailnet addresses.
pub fn lan_addresses() -> Vec<IpAddr> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list`, which is freed below.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return Vec::new();
    }
    let mut addresses = Vec::new();
    let mut entry = list;
    while !entry.is_null() {
        // SAFETY: entries form a valid list until freeifaddrs.
        let interface = unsafe { &*entry };
        entry = interface.ifa_next;
        let up = interface.ifa_flags & libc::IFF_UP as u32 != 0;
        if !up || interface.ifa_addr.is_null() {
            continue;
        }
        // SAFETY: ifa_addr points to a socket address of the family it names.
        let address = unsafe {
            match i32::from((*interface.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let v4 = &*(interface.ifa_addr as *const libc::sockaddr_in);
                    IpAddr::V4(Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr)))
                }
                libc::AF_INET6 => {
                    let v6 = &*(interface.ifa_addr as *const libc::sockaddr_in6);
                    IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.s6_addr))
                }
                _ => continue,
            }
        };
        if is_lan(address) {
            addresses.push(address);
        }
    }
    // SAFETY: `list` came from getifaddrs.
    unsafe { libc::freeifaddrs(list) };
    addresses.sort();
    addresses.dedup();
    addresses
}

fn is_lan(address: IpAddr) -> bool {
    if address.is_loopback() || address.is_unspecified() || is_tailnet_address(address) {
        return false;
    }
    match address {
        IpAddr::V4(v4) => !v4.is_link_local(),
        IpAddr::V6(v6) => v6.segments()[0] & 0xffc0 != 0xfe80,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_network_addresses_count() {
        for lan in ["192.168.1.20", "10.0.0.5", "fdf1:54a:8baf::1"] {
            assert!(is_lan(lan.parse().unwrap()), "{lan}");
        }
        for not in [
            "127.0.0.1",
            "::1",
            "169.254.1.1",
            "fe80::1",
            "100.64.0.10",
            "fd7a:115c:a1e0::a",
        ] {
            assert!(!is_lan(not.parse().unwrap()), "{not}");
        }
        // Whatever this Mac has, none of it is loopback or tailnet.
        assert!(lan_addresses().into_iter().all(is_lan));
    }
}
