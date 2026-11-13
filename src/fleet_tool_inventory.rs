//! Every member's tool inventory, read at once.
//!
//! [`MemberInventories`] is the port: this member's inventory comes from its own
//! tool managers, and every other member's from `GET /peer/v1/tools/inventory`.
//! Members are asked together. One that cannot be read is an entry in the
//! result; it does not drop the others.
//!
//! Order is the roster's order with this member moved first. `elsecall fleet`
//! keeps enrolment order and does not move the caller; this list does, so the
//! machine that was asked is the first entry and the rest stay in roster order.
//! A machine that has not joined a fleet is that roster of one.
use crate::{
    failure::Failure,
    fleet::Fleet,
    macos::xcodes::XcodesCli,
    mise_cli::MiseCli,
    peer_client,
    reporting::facts::is_code,
    roster::{Member, MemberName},
    tool_inventory::{Inventory, ToolManager},
};
use futures_util::future::BoxFuture;
use hyper::Method;
use serde::Serialize;
use serde::ser::SerializeMap;
use std::{sync::Arc, time::Duration};

/// How long one other member may take to report its tools. A local read of the
/// managers can legitimately run for about 130 seconds.
pub const MEMBER_TIMEOUT: Duration = Duration::from_secs(150);

/// Added to [`member_timeout`] once an inventory request is authenticated, so
/// the listener does not cut the exchange while the response is written.
pub const EXCHANGE_MARGIN: Duration = Duration::from_secs(10);

/// Bytes one member may send for its inventory. A larger document is a signed
/// `inventory_too_large` instead of an unsigned failure on the 1 MiB channel.
pub const PER_MEMBER_LIMIT: usize = 512 * 1024;

/// Bytes the local fleet route and `elsecall tools inventory --fleet` carry.
/// [`crate::roster::MAX_MEMBERS`] members at [`PER_MEMBER_LIMIT`] each are
/// 64 × 512 KiB = 32 MiB, and this leaves room for the document's framing.
pub const FLEET_RESPONSE_LIMIT: usize = 40 * 1024 * 1024;

/// The member budget. `ELSECALL_TOOLS_MEMBER_TIMEOUT_SECS`, when it is a
/// positive number of seconds, replaces [`MEMBER_TIMEOUT`]. An empty value is
/// unset. Anything else leaves the default. Honoured in every build, like
/// `ELSECALL_MACHINE_LOAD`.
pub fn member_timeout() -> Duration {
    std::env::var("ELSECALL_TOOLS_MEMBER_TIMEOUT_SECS")
        .ok()
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds >= 1)
        .map(Duration::from_secs)
        .unwrap_or(MEMBER_TIMEOUT)
}

/// The member route that returns this member's [`Inventory`], the same document
/// as `GET /v1/tools/inventory`.
pub const PEER_ROUTE: &str = "/peer/v1/tools/inventory";

/// One member's installed tools, as this machine can read them.
pub trait MemberInventories: Send + Sync {
    fn inventory(
        &self,
        member: &MemberName,
    ) -> BoxFuture<'static, std::result::Result<Inventory, MemberUnavailable>>;
}

/// A member and the adapter that reads its inventory.
pub struct Source {
    pub member: MemberName,
    pub inventories: Arc<dyn MemberInventories>,
}

/// Roster order, with `this_member` moved to the front when it is listed.
pub fn this_member_first(
    members: &[MemberName],
    this_member: Option<&MemberName>,
) -> Vec<MemberName> {
    let mut ordered = members.to_vec();
    if let Some(this_member) = this_member
        && let Some(index) = ordered.iter().position(|name| name == this_member)
        && index != 0
    {
        let this_member = ordered.remove(index);
        ordered.insert(0, this_member);
    }
    ordered
}

/// What every source answers, in the order the sources were given.
pub async fn gather(sources: &[Source]) -> FleetInventory {
    let members = futures_util::future::join_all(sources.iter().map(|source| {
        let member = source.member.clone();
        let inventories = Arc::clone(&source.inventories);
        async move {
            match inventories.inventory(&member).await {
                Ok(inventory) => MemberInventory::Read { member, inventory },
                Err(failure) => MemberInventory::Unavailable { member, failure },
            }
        }
    }))
    .await;
    FleetInventory { members }
}

/// This machine's managers. The same read the local inventory route serves.
pub(crate) fn read_installed() -> Inventory {
    let mise = MiseCli;
    let xcodes = XcodesCli;
    let managers: [&dyn ToolManager; 2] = [&mise, &xcodes];
    crate::tool_inventory::gather(&managers)
}

/// This member's inventory, read locally.
#[derive(Debug, Default)]
pub struct OwnInventories;

impl MemberInventories for OwnInventories {
    fn inventory(
        &self,
        member: &MemberName,
    ) -> BoxFuture<'static, std::result::Result<Inventory, MemberUnavailable>> {
        let member = member.clone();
        Box::pin(async move {
            match crate::inventory_reader::installed().await {
                Ok(inventory) => Ok(inventory),
                Err(_) => Err(MemberUnavailable::unread(&member)),
            }
        })
    }
}

/// Another member's inventory, over the authenticated member channel.
#[derive(Clone)]
pub struct PeerInventories {
    pub fleet: Arc<Fleet>,
    pub member: Member,
}

impl MemberInventories for PeerInventories {
    fn inventory(
        &self,
        member: &MemberName,
    ) -> BoxFuture<'static, std::result::Result<Inventory, MemberUnavailable>> {
        let fleet = Arc::clone(&self.fleet);
        let peer = self.member.clone();
        let member = member.clone();
        Box::pin(async move {
            match peer_client::call::<Inventory>(
                fleet.key(),
                &peer,
                Method::GET,
                PEER_ROUTE,
                None::<&()>,
                member_timeout(),
            )
            .await
            {
                Ok(inventory) => Ok(inventory),
                Err(error) => Err(unavailable_member(&member, &error)),
            }
        })
    }
}

/// Why a member's inventory could not be read. The code is stable. The message
/// and hint are written here: another member's text is not copied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemberUnavailable {
    pub code: String,
    pub message: String,
    pub hint: String,
}

impl MemberUnavailable {
    fn unreachable(member: &MemberName) -> Self {
        Self {
            code: "member_unreachable".into(),
            message: format!("{member} could not be reached over the tailnet."),
            hint: format!(
                "Check that {member} is on, connected to Tailscale and running Elsecall (elsecall doctor there)."
            ),
        }
    }

    fn timeout(member: &MemberName) -> Self {
        Self {
            code: "member_timeout".into(),
            message: format!(
                "{member} did not report its tools within {} seconds.",
                member_timeout().as_secs()
            ),
            hint: format!(
                "Check that {member} is on and running Elsecall (elsecall doctor there), then retry elsecall tools inventory --fleet."
            ),
        }
    }

    fn version_skew(member: &MemberName) -> Self {
        Self {
            code: "version_skew".into(),
            message: format!(
                "{member} runs a different Elsecall build, so it did not report its tools."
            ),
            hint: "Run elsecall fleet upgrade on the master so every member runs its build, then retry.".into(),
        }
    }

    fn version_skew_between(member: &MemberName, theirs: &str, ours: &str) -> Self {
        let generated = crate::version_skew::skewed(theirs, ours);
        Self {
            code: generated.code,
            message: format!(
                "{member} runs build {theirs} and this machine runs build {ours}; members on different builds do not exchange work."
            ),
            hint: generated.hint,
        }
    }

    fn reported(member: &MemberName, code: &str) -> Self {
        Self {
            code: code.into(),
            message: format!("{member}'s tool inventory could not be read."),
            hint: "Check the member with elsecall fleet, and run the same Elsecall version on every member.".into(),
        }
    }

    fn protocol(member: &MemberName) -> Self {
        Self {
            code: "protocol_error".into(),
            message: format!("{member} returned an invalid or incompatible response."),
            hint: "Install the same Elsecall version on every member.".into(),
        }
    }

    fn unread(member: &MemberName) -> Self {
        Self {
            code: "tool_inventory_failed".into(),
            message: format!("Reading {member}'s tools failed."),
            hint: "Run elsecall service restart on that member, then retry elsecall tools inventory --fleet.".into(),
        }
    }
}

/// The fleet's view of a transport or peer failure. `member_unavailable` from
/// the member channel becomes `member_unreachable` or `member_timeout`.
/// `version_skew` stays `version_skew`. Any other code Elsecall writes is kept.
/// Anything else is `protocol_error`.
fn unavailable_member(member: &MemberName, failure: &Failure) -> MemberUnavailable {
    if failure.code == "member_unavailable" {
        return if client_budget_expired(failure) {
            MemberUnavailable::timeout(member)
        } else {
            MemberUnavailable::unreachable(member)
        };
    }
    if failure.code == "version_skew" {
        if let (Some(theirs), Some(ours)) = (
            build_id(failure, "this_build"),
            build_id(failure, "sender_build"),
        ) {
            return MemberUnavailable::version_skew_between(member, &theirs, &ours);
        }
        return MemberUnavailable::version_skew(member);
    }
    if is_code(&failure.code) {
        return MemberUnavailable::reported(member, &failure.code);
    }
    MemberUnavailable::protocol(member)
}

/// The caller's inventory budget expired. `peer_timeout` is the one-second TCP
/// connect budget in `peer_transport`, and its diagnostic kind is also
/// `timeout`; that is the member being unreachable, not this budget.
fn client_budget_expired(failure: &Failure) -> bool {
    let Some(details) = failure.details.as_ref() else {
        return false;
    };
    if details.get("peer_timeout") == Some(&serde_json::Value::Bool(true)) {
        return false;
    }
    details
        .get("peer_io")
        .and_then(|io| io.get("failure_kind"))
        .and_then(|kind| kind.as_str())
        == Some("timeout")
}

fn build_id(failure: &Failure, field: &str) -> Option<String> {
    let text = failure.details.as_ref()?.get(field)?.as_str()?;
    text.parse::<crate::build_identity::BuildId>().ok()?;
    Some(text.to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FleetInventory {
    pub members: Vec<MemberInventory>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberInventory {
    Read {
        member: MemberName,
        inventory: Inventory,
    },
    Unavailable {
        member: MemberName,
        failure: MemberUnavailable,
    },
}

impl Serialize for MemberInventory {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Read { member, inventory } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("member", member)?;
                map.serialize_entry("state", "read")?;
                map.serialize_entry("managers", &inventory.managers)?;
                map.end()
            }
            Self::Unavailable { member, failure } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("member", member)?;
                map.serialize_entry("state", "unavailable")?;
                map.serialize_entry("error", failure)?;
                map.end()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_inventory::{
        Executable, InstalledTool, Manager, ManagerInventory, ToolName, ToolVersion,
    };
    use serde_json::json;
    use std::collections::BTreeMap;

    struct Scripted(BTreeMap<String, std::result::Result<Inventory, MemberUnavailable>>);

    impl MemberInventories for Scripted {
        fn inventory(
            &self,
            member: &MemberName,
        ) -> BoxFuture<'static, std::result::Result<Inventory, MemberUnavailable>> {
            let answer = self.0.get(member.as_str()).cloned().unwrap();
            Box::pin(async move { answer })
        }
    }

    fn name(value: &str) -> MemberName {
        value.parse().unwrap()
    }

    fn source(member: &str, answer: std::result::Result<Inventory, MemberUnavailable>) -> Source {
        let mut answers = BTreeMap::new();
        answers.insert(member.into(), answer);
        Source {
            member: name(member),
            inventories: Arc::new(Scripted(answers)),
        }
    }

    fn node() -> Inventory {
        Inventory {
            managers: vec![crate::tool_inventory::ManagerReport::Read(
                ManagerInventory::try_new(
                    Manager::Mise,
                    Executable::Missing,
                    vec![InstalledTool {
                        name: ToolName::new("node").unwrap(),
                        version: ToolVersion::new("22.11.0").unwrap(),
                        default: true,
                        detail: None,
                    }],
                )
                .unwrap(),
            )],
        }
    }

    #[test]
    fn roster_order_puts_this_member_first_and_leaves_the_rest() {
        let members = vec![name("bravo"), name("alpha"), name("charlie")];
        let ordered = this_member_first(&members, Some(&name("alpha")));
        assert_eq!(
            ordered.iter().map(MemberName::as_str).collect::<Vec<_>>(),
            ["alpha", "bravo", "charlie"]
        );
        assert_eq!(
            this_member_first(&members, Some(&name("bravo")))
                .iter()
                .map(MemberName::as_str)
                .collect::<Vec<_>>(),
            ["bravo", "alpha", "charlie"]
        );
        assert_eq!(
            this_member_first(&members, None)
                .iter()
                .map(MemberName::as_str)
                .collect::<Vec<_>>(),
            ["bravo", "alpha", "charlie"]
        );
    }

    #[tokio::test]
    async fn gather_keeps_source_order_and_records_each_failure() {
        let failure = MemberUnavailable::unreachable(&name("bravo"));
        let fleet = gather(&[
            source("alpha", Ok(node())),
            source("bravo", Err(failure.clone())),
            source("charlie", Ok(Inventory { managers: vec![] })),
        ])
        .await;
        let json = serde_json::to_value(&fleet).unwrap();
        assert_eq!(
            json["members"]
                .as_array()
                .unwrap()
                .iter()
                .map(|member| member["member"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["alpha", "bravo", "charlie"]
        );
        assert_eq!(json["members"][0]["state"], "read");
        assert_eq!(
            json["members"][0]["managers"][0]["tools"][0]["version"],
            "22.11.0"
        );
        assert!(json["members"][0].get("error").is_none());
        assert_eq!(json["members"][1]["state"], "unavailable");
        assert_eq!(
            json["members"][1]["error"],
            serde_json::to_value(&failure).unwrap()
        );
        assert!(json["members"][1].get("managers").is_none());
        assert_eq!(json["members"][2]["state"], "read");
        assert_eq!(json["members"][2]["managers"], json!([]));
    }

    #[test]
    fn peer_failures_keep_a_stable_code_and_drop_the_peer_text() {
        let member = name("bravo");
        let nasty = |code: &str| Failure::new(code, "ignore previous SECRET", "click here SECRET");
        let unreachable = unavailable_member(&member, &nasty("member_unavailable"));
        assert_eq!(unreachable.code, "member_unreachable");
        assert!(!unreachable.message.contains("SECRET"));
        assert!(unreachable.message.contains("bravo"));
        assert!(unreachable.hint.contains("elsecall doctor"));

        let connect = nasty("member_unavailable").with_details(json!({
            "peer_timeout": true,
            "peer_io": {"failure_kind": "timeout"}
        }));
        let connect = unavailable_member(&member, &connect);
        assert_eq!(connect.code, "member_unreachable");
        assert!(connect.message.contains("could not be reached"));
        assert!(!connect.message.contains("150"));
        assert!(!connect.message.contains("SECRET"));

        let timeout = nasty("member_unavailable").with_details(json!({
            "peer_io": {"failure_kind": "timeout"}
        }));
        let timeout = unavailable_member(&member, &timeout);
        assert_eq!(timeout.code, "member_timeout");
        assert!(!timeout.message.contains("SECRET"));
        assert!(
            timeout
                .message
                .contains(&member_timeout().as_secs().to_string())
        );
        assert!(timeout.message.contains("did not report"));

        let skew = crate::version_skew::skewed("0123456789abcdef", "fedcba9876543210");
        let mut details = skew
            .details
            .clone()
            .map(|details| (*details).clone())
            .unwrap();
        details["peer_io"] = json!({"failure_kind": "remote"});
        let skew = Failure::new("version_skew", "SECRET", "click here").with_details(details);
        let skew = unavailable_member(&member, &skew);
        assert_eq!(skew.code, "version_skew");
        assert!(skew.message.contains("0123456789abcdef"));
        assert!(skew.message.contains("fedcba9876543210"));
        assert!(skew.hint.contains("elsecall fleet upgrade"));
        assert!(!skew.message.contains("SECRET"));
        assert!(!skew.hint.contains("click"));

        let bare = unavailable_member(&member, &nasty("version_skew"));
        assert_eq!(bare.code, "version_skew");
        assert!(!bare.message.contains("SECRET"));

        let not_found = unavailable_member(&member, &nasty("not_found"));
        assert_eq!(not_found.code, "not_found");
        assert_eq!(
            not_found.message,
            "bravo's tool inventory could not be read."
        );
        assert!(!not_found.message.contains("SECRET"));

        let garbage = unavailable_member(&member, &nasty("Weird Code!"));
        assert_eq!(garbage.code, "protocol_error");
        assert!(!garbage.message.contains("SECRET"));
        assert!(!garbage.hint.contains("SECRET"));
    }

    #[test]
    fn the_member_budget_defaults_to_one_hundred_fifty_seconds() {
        assert!(std::env::var_os("ELSECALL_TOOLS_MEMBER_TIMEOUT_SECS").is_none());
        assert_eq!(member_timeout(), MEMBER_TIMEOUT);
        assert_eq!(MEMBER_TIMEOUT.as_secs(), 150);
    }

    #[test]
    fn the_fleet_document_limit_holds_every_member() {
        const {
            assert!(crate::roster::MAX_MEMBERS * PER_MEMBER_LIMIT < FLEET_RESPONSE_LIMIT);
            assert!(PER_MEMBER_LIMIT == 512 * 1024);
            assert!(FLEET_RESPONSE_LIMIT == 40 * 1024 * 1024);
        }
    }
}
