//! `elsecall fleet nearby`: the Macs on the local network, and which are members.
//!
//! It browses in the CLI's own process, which macOS lets use the local network
//! when run from a terminal, over SSH or by an app the user allowed.
use elsecall_add_mac::nearby_macs::{
    KnownMember, LocalNetwork, Membership, NearbyMac, RemoteLogin, nearby,
};
use elsecall_failure::Result;
use elsecall_local_client as client;
use elsecall_local_state::paths::Paths;
use elsecall_platform_macos::macos::dns_sd::Bonjour;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{net::IpAddr, time::Duration};

const FLEET_TIMEOUT: Duration = Duration::from_secs(15);

pub async fn run(paths: &Paths, browse: Duration) -> Result<(Value, bool)> {
    // Asking every member for its state takes as long as browsing, so both run at once.
    let socket = paths.socket();
    let (view, advertisements) = tokio::join!(
        client::get_large_within::<FleetView>(&socket, "/v1/fleet", FLEET_TIMEOUT),
        tokio::task::spawn_blocking(move || Bonjour.advertisements(browse)),
    );
    let advertisements = advertisements.expect("browsing does not panic")?;
    let macs = nearby(advertisements, &view?.known_members());
    let next_step = next_step(&macs);
    Ok((json!({ "macs": macs, "next_step": next_step }), true))
}

/// The parts of `/v1/fleet` that identify members on a local network.
#[derive(Deserialize)]
struct FleetView {
    #[serde(default)]
    members: Vec<ViewedMember>,
}
#[derive(Deserialize)]
struct ViewedMember {
    name: String,
    #[serde(default)]
    status: Option<ViewedStatus>,
}
#[derive(Deserialize)]
struct ViewedStatus {
    machine: ViewedMachine,
}
#[derive(Deserialize)]
struct ViewedMachine {
    hostname: Option<String>,
    #[serde(default)]
    local_host_name: Option<String>,
    #[serde(default)]
    lan_addresses: Option<Vec<IpAddr>>,
}

impl FleetView {
    fn known_members(self) -> Vec<KnownMember> {
        self.members
            .into_iter()
            .map(|member| {
                let machine = member.status.map(|status| status.machine);
                KnownMember {
                    name: member.name,
                    host_names: machine
                        .as_ref()
                        .map(|m| {
                            [&m.local_host_name, &m.hostname]
                                .into_iter()
                                .flatten()
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default(),
                    lan_addresses: machine.and_then(|m| m.lan_addresses),
                }
            })
            .collect()
    }
}

fn next_step(macs: &[NearbyMac]) -> String {
    if let Some(mac) = macs.iter().find(|mac| mac.addable()) {
        return format!(
            "Add a Mac with: elsecall fleet add {} --user <account> (the password on standard input).",
            mac.host
        );
    }
    let off = macs.iter().find(|mac| {
        mac.remote_login == RemoteLogin::Off && mac.membership == Membership::NotMember
    });
    if let Some(mac) = off {
        return format!(
            "Turn on Remote Login on {} (System Settings > General > Sharing), then run elsecall fleet nearby again.",
            mac.name
        );
    }
    "No other Mac was found on this network. Turn on Remote Login on the new Mac (System Settings > General > Sharing), or add it by host name or address with elsecall fleet add <host>.".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use elsecall_machine::inspection::Machine;

    fn mac(name: &str, remote_login: RemoteLogin, membership: Membership) -> NearbyMac {
        NearbyMac {
            name: name.into(),
            host: format!("{}.local", name.to_lowercase()),
            addresses: Vec::new(),
            model: Some("Mac16,10".into()),
            remote_login,
            membership,
        }
    }

    #[test]
    fn the_next_step_is_the_first_mac_that_can_be_added() {
        let on = RemoteLogin::On { ssh_port: 22 };
        let off = RemoteLogin::Off;
        let not = || Membership::NotMember;
        assert!(
            next_step(&[mac("studio", off, not()), mac("new", on, not())])
                .contains("elsecall fleet add new.local")
        );
        assert!(
            next_step(&[mac("studio", off, not())]).starts_with("Turn on Remote Login on studio")
        );
        let member = Membership::Member {
            member: "mini".into(),
        };
        assert!(next_step(&[mac("mini", on, member)]).starts_with("No other Mac"));
        let unknown = NearbyMac {
            model: None,
            ..mac("box", on, not())
        };
        assert!(next_step(&[unknown]).starts_with("No other Mac"));
        assert!(next_step(&[]).starts_with("No other Mac"));
    }

    #[test]
    fn members_are_read_from_the_machine_state_members_report() {
        // A real inspection, as a member's status carries it.
        let mut machine = serde_json::to_value(Machine::fixture()).unwrap();
        machine["hostname"] = json!("MiniOnDhcp.home");
        machine["local_host_name"] = json!("Mac-mini");
        machine["lan_addresses"] = json!(["192.168.1.20"]);
        let view: FleetView = serde_json::from_value(json!({"members": [
            {"name": "mac-mini", "reachability": "reachable", "status": {"machine": machine}},
            {"name": "macbook-pro", "reachability": "unreachable", "error": {}},
        ]}))
        .unwrap();
        let members = view.known_members();
        assert_eq!(members[0].host_names, ["Mac-mini", "MiniOnDhcp.home"]);
        assert_eq!(
            members[0].lan_addresses,
            Some(vec!["192.168.1.20".parse().unwrap()])
        );
        assert!(members[1].host_names.is_empty());
        assert_eq!(members[1].lan_addresses, None);
    }
}
