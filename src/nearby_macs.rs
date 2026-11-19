//! Macs on the local network that could join the fleet: `elsecall fleet nearby`.
//!
//! What the network advertises comes through the `LocalNetwork` port. This module
//! decides which advertisements are Macs, whether each can be reached over Remote
//! Login, and which are already fleet members.
use crate::lan_address::is_lan;
use elsecall_failure::Result;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, net::IpAddr, time::Duration};

/// What an advertisement offers the master.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    /// Remote Login is on: SSH answers at `port`.
    RemoteLogin { port: u16 },
    /// Continuity, which most Apple devices advertise while it is on, so a Mac with
    /// Remote Login off is still seen.
    Continuity,
}

/// One advertised service instance, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advertisement {
    pub offer: Offer,
    /// The instance name, which macOS sets to the computer's name.
    pub name: String,
    /// The host the service runs on, such as `macbook-pro.local.`.
    pub host: String,
    /// The addresses the host answered with. Only its local network ones count,
    /// the kind members report about themselves.
    pub addresses: Vec<IpAddr>,
    /// The hardware model from the device's `_device-info._tcp` record.
    pub model: Option<String>,
}

/// What the local network advertises.
pub trait LocalNetwork {
    /// Browses for `browse`, then resolves what it found within a short bound.
    /// Fails when the system denies this process the local network.
    fn advertisements(&self, browse: Duration) -> Result<Vec<Advertisement>>;
}

/// A fleet member as `nearby` matches it to a host.
#[derive(Debug, Clone, Default)]
pub struct KnownMember {
    pub name: String,
    /// The host names the member reports, when it is reachable: its Bonjour name
    /// and the name the system gives itself, which may differ.
    pub host_names: Vec<String>,
    /// Its addresses on its local networks; `None` when it is unreachable or its
    /// build predates reporting them.
    pub lan_addresses: Option<Vec<IpAddr>>,
}

/// Whether the master can reach a Mac over SSH.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "remote_login", rename_all = "snake_case")]
pub enum RemoteLogin {
    On {
        ssh_port: u16,
    },
    /// Seen only through Continuity: Remote Login must be turned on first.
    Off,
}

/// Whether a Mac is already a fleet member. Default Mac names repeat across
/// networks, so a name alone only suggests it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "membership", rename_all = "snake_case")]
pub enum Membership {
    NotMember,
    /// Its host name and a local address match what the member reports.
    Member {
        member: String,
    },
    /// Only a name matches: the member is unreachable or reports no addresses, or
    /// the Mac has no local network address to compare.
    PossibleMember {
        member: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NearbyMac {
    /// The Mac's computer name.
    pub name: String,
    /// The host name to add it by, such as `macbook-pro.local`, in lowercase.
    pub host: String,
    /// IPv4 first.
    pub addresses: Vec<IpAddr>,
    /// `None` for an SSH host that reports no model, which may not be a Mac.
    pub model: Option<String>,
    #[serde(flatten)]
    pub remote_login: RemoteLogin,
    #[serde(flatten)]
    pub membership: Membership,
}

impl NearbyMac {
    /// Whether this is a Mac that Add Mac can reach and that is not a member.
    pub fn addable(&self) -> bool {
        self.model.is_some()
            && matches!(self.remote_login, RemoteLogin::On { .. })
            && self.membership == Membership::NotMember
    }
}

/// The Macs among `advertisements`, one per host: those that can be added first,
/// then those with Remote Login off, SSH hosts that report no model, and
/// members, each by name.
pub fn nearby(advertisements: Vec<Advertisement>, members: &[KnownMember]) -> Vec<NearbyMac> {
    let mut hosts: BTreeMap<String, Vec<Advertisement>> = BTreeMap::new();
    for advertisement in advertisements {
        hosts
            .entry(host_key(&advertisement.host))
            .or_default()
            .push(advertisement);
    }
    let mut macs: Vec<NearbyMac> = hosts
        .into_iter()
        .filter_map(|(host, found)| {
            let model = found.iter().find_map(|a| a.model.clone());
            let ssh_port = found.iter().find_map(|a| match a.offer {
                Offer::RemoteLogin { port } => Some(port),
                Offer::Continuity => None,
            });
            // Without a model, only an SSH host may be a Mac; other devices that
            // advertise Continuity are phones, tablets and TVs.
            match &model {
                Some(model) if !is_mac(model) => return None,
                None if ssh_port.is_none() => return None,
                _ => (),
            }
            let first = found
                .iter()
                .find(|a| matches!(a.offer, Offer::RemoteLogin { .. }))
                .unwrap_or(&found[0]);
            let mut addresses: Vec<IpAddr> = found
                .iter()
                .flat_map(|a| a.addresses.iter().copied())
                .filter(|address| is_lan(*address))
                .collect();
            addresses.sort_by_key(|address| (address.is_ipv6(), *address));
            addresses.dedup();
            Some(NearbyMac {
                name: first.name.clone(),
                host: first.host.trim_end_matches('.').to_ascii_lowercase(),
                membership: membership(members, &host, &addresses),
                addresses,
                model,
                remote_login: ssh_port
                    .map_or(RemoteLogin::Off, |ssh_port| RemoteLogin::On { ssh_port }),
            })
        })
        .collect();
    macs.sort_by_key(|mac| {
        (
            match mac.membership {
                Membership::NotMember => 0,
                Membership::PossibleMember { .. } => 1,
                Membership::Member { .. } => 2,
            },
            mac.model.is_none(),
            mac.remote_login == RemoteLogin::Off,
            mac.name.to_lowercase(),
        )
    });
    macs
}

/// Mac hardware model identifiers, such as `Mac16,10`, `MacBookPro18,4`,
/// `iMac21,1` or `VirtualMac2,1`: a Mac prefix, letters, then `major,minor`.
/// Other Apple devices report `iPhone17,1` or `AppleTV14,1`, and Samba servers
/// pretend to be `MacSamba`.
fn is_mac(model: &str) -> bool {
    let Some(rest) = ["VirtualMac", "iMac", "Mac"]
        .iter()
        .find_map(|prefix| model.strip_prefix(prefix))
    else {
        return false;
    };
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    let Some((major, minor)) = rest.split_once(',') else {
        return false;
    };
    let number = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    number(major) && number(minor)
}

/// A host name without case, trailing dot or `.local`, so `MacBook-Pro.local.`
/// and `macbook-pro` compare equal.
fn host_key(host: &str) -> String {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host.strip_suffix(".local").unwrap_or(&host).to_owned()
}

fn membership(members: &[KnownMember], host: &str, addresses: &[IpAddr]) -> Membership {
    let named = |member: &&KnownMember| {
        member.name == host || member.host_names.iter().any(|name| host_key(name) == host)
    };
    let confirmed = members.iter().filter(named).find(|member| {
        member
            .lan_addresses
            .as_ref()
            .is_some_and(|own| own.iter().any(|a| addresses.contains(a)))
    });
    if let Some(member) = confirmed {
        return Membership::Member {
            member: member.name.clone(),
        };
    }
    // A member that reports its addresses and has none of these is another Mac
    // with the same name, on another network. A Mac with no local network address
    // gives nothing to compare, so its name is all there is.
    match members
        .iter()
        .filter(named)
        .find(|member| member.lan_addresses.is_none() || addresses.is_empty())
    {
        Some(member) => Membership::PossibleMember {
            member: member.name.clone(),
        },
        None => Membership::NotMember,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(offer: Offer, name: &str, host: &str, model: Option<&str>) -> Advertisement {
        Advertisement {
            offer,
            name: name.into(),
            host: host.into(),
            addresses: vec!["192.168.64.30".parse().unwrap()],
            model: model.map(Into::into),
        }
    }
    const SSH: Offer = Offer::RemoteLogin { port: 22 };
    const CONTINUITY: Offer = Offer::Continuity;

    #[test]
    fn mac_models_are_told_from_other_devices() {
        for mac in [
            "Mac16,10",
            "MacBookPro18,4",
            "MacBookAir10,1",
            "Macmini9,1",
            "MacPro7,1",
            "iMac21,1",
            "iMacPro1,1",
            "VirtualMac2,1",
        ] {
            assert!(is_mac(mac), "{mac}");
        }
        for other in [
            "MacSamba",
            "MacPro",
            "Xserve3,1",
            "iPhone17,1",
            "AppleTV14,1",
            "AudioAccessory6,1",
            "Mac16,",
            "Mac,1",
            "",
        ] {
            assert!(!is_mac(other), "{other}");
        }
    }

    #[test]
    fn macs_are_merged_by_host_and_other_devices_left_out() {
        let found = vec![
            seen(
                CONTINUITY,
                "MacBook Pro",
                "MacBook-Pro.local.",
                Some("Mac15,9"),
            ),
            seen(SSH, "MacBook Pro", "macbook-pro.local.", None),
            seen(CONTINUITY, "Studio", "studio.local.", Some("Mac14,13")),
            seen(CONTINUITY, "Phone", "phone.local.", Some("iPhone17,1")),
            seen(CONTINUITY, "Living room", "tv.local.", Some("AppleTV14,1")),
            seen(CONTINUITY, "Unknown", "unknown.local.", None),
            seen(SSH, "build-box", "build-box.local.", None),
            seen(SSH, "nas", "nas.local.", Some("MacSamba")),
        ];
        let macs = nearby(found, &[]);
        let summary: Vec<(&str, &str, RemoteLogin, bool)> = macs
            .iter()
            .map(|m| {
                (
                    m.name.as_str(),
                    m.host.as_str(),
                    m.remote_login,
                    m.addable(),
                )
            })
            .collect();
        let on = RemoteLogin::On { ssh_port: 22 };
        assert_eq!(
            summary,
            [
                ("MacBook Pro", "macbook-pro.local", on, true),
                ("Studio", "studio.local", RemoteLogin::Off, false),
                // An SSH host without a model may not be a Mac: listed last, never
                // suggested.
                ("build-box", "build-box.local", on, false),
            ]
        );
        assert_eq!(macs[0].model.as_deref(), Some("Mac15,9"));
        assert_eq!(
            macs[0].addresses,
            ["192.168.64.30".parse::<IpAddr>().unwrap()]
        );
    }

    #[test]
    fn a_member_needs_its_name_and_an_address_to_match() {
        let found = vec![
            seen(SSH, "Mac mini", "mac-mini.local.", Some("Mac16,10")),
            seen(SSH, "MacBook Pro", "macbook-pro.local.", Some("Mac15,9")),
            seen(SSH, "Studio", "studio.local.", Some("Mac14,13")),
            seen(SSH, "New", "new-mac.local.", Some("Mac16,10")),
        ];
        let here = Some(vec!["192.168.64.30".parse().unwrap()]);
        let members = [
            KnownMember {
                name: "build-host".into(),
                host_names: vec!["Mac-mini".into(), "MiniOnDhcp.home".into()],
                lan_addresses: here,
            },
            // Same default name, but on another network: not this Mac.
            KnownMember {
                name: "studio".into(),
                host_names: vec!["studio".into()],
                lan_addresses: Some(vec!["10.0.0.7".parse().unwrap()]),
            },
            // Unreachable: only its name is known.
            KnownMember {
                name: "macbook-pro".into(),
                ..KnownMember::default()
            },
        ];
        let macs = nearby(found, &members);
        let marked: Vec<(&str, &Membership)> = macs
            .iter()
            .map(|m| (m.host.as_str(), &m.membership))
            .collect();
        assert_eq!(
            marked,
            [
                ("new-mac.local", &Membership::NotMember),
                ("studio.local", &Membership::NotMember),
                (
                    "macbook-pro.local",
                    &Membership::PossibleMember {
                        member: "macbook-pro".into()
                    }
                ),
                (
                    "mac-mini.local",
                    &Membership::Member {
                        member: "build-host".into()
                    }
                ),
            ]
        );
    }

    #[test]
    fn a_mac_without_local_network_addresses_may_be_the_member_it_is_named_after() {
        // A Mac sees its own services on loopback too, where its host has only
        // addresses no other Mac can use.
        let unresolved = Advertisement {
            addresses: vec!["127.0.0.1".parse().unwrap(), "fe80::1".parse().unwrap()],
            ..seen(CONTINUITY, "Mac mini", "mac-mini.local.", Some("Mac16,10"))
        };
        let members = [KnownMember {
            name: "build-host".into(),
            host_names: vec!["Mac-mini".into()],
            lan_addresses: Some(vec!["192.168.64.30".parse().unwrap()]),
        }];
        let macs = nearby(vec![unresolved], &members);
        assert!(macs[0].addresses.is_empty());
        assert_eq!(
            macs[0].membership,
            Membership::PossibleMember {
                member: "build-host".into()
            }
        );
    }

    #[test]
    fn remote_login_and_membership_are_flattened_into_the_mac() {
        let mac = NearbyMac {
            name: "MacBook Pro".into(),
            host: "macbook-pro.local".into(),
            addresses: vec![],
            model: None,
            remote_login: RemoteLogin::On { ssh_port: 22 },
            membership: Membership::Member {
                member: "macbook-pro".into(),
            },
        };
        let json = serde_json::to_value(&mac).unwrap();
        assert_eq!(json["remote_login"], "on");
        assert_eq!(json["ssh_port"], 22);
        assert_eq!(json["membership"], "member");
        assert_eq!(json["member"], "macbook-pro");
        let off = serde_json::to_value(NearbyMac {
            remote_login: RemoteLogin::Off,
            membership: Membership::NotMember,
            ..mac
        })
        .unwrap();
        assert_eq!(off["remote_login"], "off");
        assert_eq!(off["membership"], "not_member");
        assert!(off.get("ssh_port").is_none());
        assert!(off.get("member").is_none());
        // The window reads them back.
        assert_eq!(
            serde_json::from_value::<NearbyMac>(json)
                .unwrap()
                .membership,
            Membership::Member {
                member: "macbook-pro".into()
            }
        );
        assert_eq!(
            serde_json::from_value::<NearbyMac>(off)
                .unwrap()
                .remote_login,
            RemoteLogin::Off
        );
    }
}
