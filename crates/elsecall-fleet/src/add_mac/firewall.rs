//! Opening the new Mac's Application Firewall and waiting for console approval
//! if the rules could not be changed remotely.

use super::{Event, Outcome};
use crate::{
    inbound_firewall::Rule,
    remote_mac::{RemoteMac, TailnetNode},
};
use elsecall_contract::failure::{Failure, Result};
use elsecall_peers::{member_code, tailnet::is_tailnet_address};
use std::time::Duration;

const APPROVAL_WITHIN: Duration = Duration::from_secs(60);
const APPROVAL_POLL: Duration = Duration::from_secs(1);

pub(super) async fn configure<M: RemoteMac>(
    mac: &mut M,
    host: &str,
    executable: &str,
    daemon: Option<&str>,
    node: &TailnetNode,
    progress: &mut impl FnMut(Event),
) -> Result<Outcome> {
    // Elsecall's listener and, if we installed it, tailscaled for direct peer connections.
    let programs: Vec<_> = std::iter::once(executable)
        .chain(daemon)
        .map(str::to_owned)
        .collect();
    let blocked = blocked_programs(mac, host, &programs).await?;
    let opened = !blocked.is_empty();
    if opened {
        mac.allow_incoming(&blocked).await?;
        let mut remaining = blocked_programs(mac, host, &programs).await?;
        if !remaining.is_empty() {
            progress(Event::FirewallApproval);
            let wait = async {
                probe_listener(mac, node).await;
                while !remaining.is_empty() {
                    remaining = blocked_programs(mac, host, &programs).await?;
                    if !remaining.is_empty() {
                        tokio::time::sleep(APPROVAL_POLL).await;
                    }
                }
                Ok(())
            };
            match tokio::time::timeout(APPROVAL_WITHIN, wait).await {
                Ok(result) => result?,
                Err(_) => {
                    return Err(Failure::new(
                        "firewall_unchanged",
                        format!(
                            "The firewall on {host} still does not allow {}.",
                            remaining[0]
                        ),
                        "Click Allow on the new Mac's incoming connections prompt. If there is no prompt, allow the program in System Settings > Network > Firewall > Options, then try again.",
                    ));
                }
            }
        }
    }
    // If an unanswered desktop prompt is later treated as a denial, the guard
    // restores the allow rule for the installed program.
    let guarded = mac.guard_incoming(&programs).await?;
    Ok(if opened || guarded {
        Outcome::Done
    } else {
        Outcome::Already
    })
}

async fn blocked_programs(
    mac: &mut impl RemoteMac,
    host: &str,
    programs: &[String],
) -> Result<Vec<String>> {
    let mut blocked = Vec::new();
    for program in programs {
        match mac.incoming(program).await? {
            Rule::Unfiltered | Rule::Allowed => {}
            Rule::NotAllowed | Rule::Unknown => blocked.push(program.clone()),
            Rule::AllBlocked => return Err(blocks_all(host)),
        }
    }
    Ok(blocked)
}

/// An incoming connection can cause macOS to show its prompt. Try only an
/// advertised Elsecall endpoint on the SSH target's own tailnet address.
async fn probe_listener(mac: &mut impl RemoteMac, node: &TailnetNode) {
    let Ok(identity) = mac.elsecall(&["fleet", "identity"]).await else {
        return;
    };
    let member = super::text(&identity, "code").and_then(member_code::decode);
    if let Ok(member) = member
        && member.name.as_str() == node.name
        && let Some(address) = member.endpoints.into_iter().find(|endpoint| {
            node.addresses.contains(&endpoint.ip()) && is_tailnet_address(endpoint.ip())
        })
    {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::TcpStream::connect(address),
        )
        .await;
    }
}

pub(super) fn blocks_all(host: &str) -> Failure {
    Failure::new(
        "firewall_blocks_all",
        format!(
            "The firewall on {host} blocks all incoming connections, so other members could never reach Elsecall there."
        ),
        "On that Mac, turn off \"Block all incoming connections\" in System Settings > Network > Firewall > Options, then add it again.",
    )
}
