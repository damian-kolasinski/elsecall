//! This Mac's addresses on its local networks: what other Macs there reach it at.
use elsecall_fleet::lan_address::is_lan;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_network_addresses_are_listed() {
        // Whatever this Mac has, none of it is loopback or tailnet.
        assert!(lan_addresses().into_iter().all(is_lan));
    }
}
