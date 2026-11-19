//! Which addresses are a Mac's addresses on a local network: what members report
//! about themselves and what `nearby` compares them with.
use crate::tailnet::is_tailnet_address;
use std::net::IpAddr;

/// Whether `address` is one other Macs on a local network reach a Mac at: not
/// loopback, unspecified, link-local or tailnet.
pub fn is_lan(address: IpAddr) -> bool {
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
            "0.0.0.0",
            "169.254.1.1",
            "fe80::1",
            "100.64.0.10",
            "fd7a:115c:a1e0::a",
        ] {
            assert!(!is_lan(not.parse().unwrap()), "{not}");
        }
    }
}
