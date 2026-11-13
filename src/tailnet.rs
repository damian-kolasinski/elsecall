//! Discovery of this machine's presence on the tailnet: the port and its override.
//!
//! Tailscale provides addresses, a node name and transport encryption. It never
//! provides identity: members authenticate each other with enrolled keys.
use crate::failure::{Failure, Result};
use std::{
    net::IpAddr,
    sync::atomic::{AtomicBool, Ordering},
};

/// Set only when a configured presence replaces Tailscale, so loopback can stand in
/// for the tailnet. A production member never accepts loopback endpoints.
static LOOPBACK_STANDS_IN: AtomicBool = AtomicBool::new(false);

pub fn allow_loopback_members() {
    LOOPBACK_STANDS_IN.store(true, Ordering::Relaxed);
}

/// Default TCP port of the member listener on every tailnet address.
pub const DEFAULT_PORT: u16 = 47821;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presence {
    /// The machine's tailnet node name, used as its proposed member name.
    pub name: String,
    /// Addresses the member listener binds. Never empty.
    pub addresses: Vec<IpAddr>,
}

pub trait Tailnet: Send + Sync {
    fn discover(&self) -> Result<Presence>;
}

/// Tailnet ranges: the shared address space Tailscale assigns (100.64.0.0/10) and its
/// IPv6 unique-local prefix (fd7a:115c:a1e0::/48).
pub fn is_tailnet_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 100 && (64..128).contains(&b)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

/// Addresses a member may listen on or be reached at: tailnet addresses, and loopback
/// only while a configured presence stands in for the tailnet.
pub fn is_member_address(address: IpAddr) -> bool {
    is_tailnet_address(address)
        || (address.is_loopback() && LOOPBACK_STANDS_IN.load(Ordering::Relaxed))
}

pub fn unavailable(reason: &str) -> Failure {
    Failure::new(
        "tailnet_unavailable",
        format!("This machine's tailnet presence could not be discovered: {reason}."),
        "Make sure Tailscale is running and logged in on this machine; the service retries every 30 seconds.",
    )
}

/// An explicit presence, read from `ELSECALL_TAILNET_NAME` and
/// `ELSECALL_TAILNET_ADDRESSES` (comma-separated). It replaces discovery entirely,
/// for diagnosis and for running several members on one machine.
pub struct Configured(Presence);
impl Configured {
    pub fn from_env() -> Option<Result<Self>> {
        let name = std::env::var("ELSECALL_TAILNET_NAME").ok();
        let addresses = std::env::var("ELSECALL_TAILNET_ADDRESSES").ok();
        if name.is_none() && addresses.is_none() {
            return None;
        }
        let configured = Self::parse(name.as_deref(), addresses.as_deref());
        if configured.is_ok() {
            allow_loopback_members();
        }
        Some(configured)
    }
    fn parse(name: Option<&str>, addresses: Option<&str>) -> Result<Self> {
        let invalid = || {
            Failure::new(
                "invalid_tailnet_override",
                "ELSECALL_TAILNET_NAME and ELSECALL_TAILNET_ADDRESSES must both be set; addresses must be tailnet or loopback addresses.",
                "Unset both variables to use Tailscale discovery.",
            )
        };
        let (Some(name), Some(addresses)) = (name, addresses) else {
            return Err(invalid());
        };
        let addresses = addresses
            .split(',')
            .map(|text| text.trim().parse::<IpAddr>().map_err(|_| invalid()))
            .collect::<Result<Vec<_>>>()?;
        if name.is_empty()
            || addresses.is_empty()
            || !addresses
                .iter()
                .all(|a| is_tailnet_address(*a) || a.is_loopback())
        {
            return Err(invalid());
        }
        Ok(Self(Presence {
            name: name.into(),
            addresses,
        }))
    }
}
impl Tailnet for Configured {
    fn discover(&self) -> Result<Presence> {
        Ok(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_only_tailnet_ranges() {
        for ok in [
            "100.64.0.1",
            "100.127.255.254",
            "100.64.0.10",
            "fd7a:115c:a1e0::a",
        ] {
            assert!(is_tailnet_address(ok.parse().unwrap()), "{ok}");
        }
        for no in [
            "100.63.255.255",
            "100.128.0.0",
            "192.168.1.2",
            "127.0.0.1",
            "fd7a:115c:a1e1::1",
            "::1",
        ] {
            assert!(!is_tailnet_address(no.parse().unwrap()), "{no}");
        }
        allow_loopback_members();
        assert!(is_member_address("127.0.0.1".parse().unwrap()));
        assert!(!is_member_address("0.0.0.0".parse().unwrap()));
    }

    #[test]
    fn override_requires_both_values_and_member_addresses() {
        let presence = Configured::parse(Some("a"), Some("127.0.0.1, 100.64.0.2"))
            .unwrap()
            .0;
        assert_eq!(presence.addresses.len(), 2);
        assert!(Configured::parse(Some("a"), None).is_err());
        assert!(Configured::parse(Some("a"), Some("0.0.0.0")).is_err());
        assert!(Configured::parse(Some("a"), Some("192.168.1.4")).is_err());
        assert!(Configured::parse(Some(""), Some("127.0.0.1")).is_err());
    }
}
