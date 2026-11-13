//! What members on different builds may do together until they are upgraded.
//!
//! Every service names its build on each member request (`x-elsecall-build`). A
//! member refuses work between different builds with `version_skew`: placement
//! (quotes, offers, attaching a placed run), configuration sync, workspace checks,
//! report gathering and tool inventories, whose messages change between builds. What a fleet needs
//! to diagnose and repair itself keeps working across builds: status (so `fleet`
//! shows the skew), the roster (enrolment, removal, moving the master, sync), the
//! placement-authority probe, and the upgrade routes, whose wire format is frozen.
//!
//! A request that names no build comes from a build that predates upgrades and is
//! not refused; the header is not part of the signed request, because it decides
//! compatibility, not authority.
use crate::{build_identity::BuildId, failure::Failure};

pub const BUILD_HEADER: &str = "x-elsecall-build";

/// Routes every build answers, whatever build asks.
fn open_across_builds(path: &str) -> bool {
    matches!(
        path,
        "/peer/v1/status" | "/peer/v1/roster" | "/peer/v1/placement/probe"
    ) || path.starts_with("/peer/v1/upgrade/")
}

/// Why this member refuses a request from a member on another build, if it does.
/// `own` is this member's announced build; `sender` the build the request names.
pub fn refusal(path: &str, own: Option<&BuildId>, sender: Option<&str>) -> Option<Failure> {
    let (Some(own), Some(sender)) = (own, sender) else {
        return None;
    };
    if open_across_builds(path) || sender == own.as_str() {
        return None;
    }
    Some(skewed(own.as_str(), sender))
}

pub fn skewed(own: &str, sender: &str) -> Failure {
    Failure::new(
        "version_skew",
        format!(
            "This member runs build {own} and the request came from build {sender}; members on different builds do not exchange work."
        ),
        "Run elsecall fleet upgrade on the master so every member runs its build, then retry.",
    )
    .with_details(serde_json::json!({"this_build": own, "sender_build": sender}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_routes_refuse_other_builds_and_repair_routes_never_do() {
        let own: BuildId = "0123456789abcdef".parse().unwrap();
        let other = "fedcba9876543210";
        for work in [
            "/peer/v1/placements/quote",
            "/peer/v1/placements",
            "/peer/v1/placements/r-1/attach",
            "/peer/v1/config-sync/attach",
            "/peer/v1/config-sync/status",
            "/peer/v1/workspaces/check",
            "/peer/v1/report/facts",
            crate::fleet_tool_inventory::PEER_ROUTE,
            "/peer/v1/status/../placements",
        ] {
            let refused = refusal(work, Some(&own), Some(other)).expect(work);
            assert_eq!(refused.code, "version_skew");
            assert!(refusal(work, Some(&own), Some(own.as_str())).is_none());
        }
        for repair in [
            "/peer/v1/status",
            "/peer/v1/roster",
            "/peer/v1/placement/probe",
            "/peer/v1/upgrade/attach",
            "/peer/v1/upgrade/status",
        ] {
            assert!(
                refusal(repair, Some(&own), Some(other)).is_none(),
                "{repair}"
            );
        }
        // Builds that predate upgrades name none; a process that announced none
        // (a library user) gates nothing.
        assert!(refusal("/peer/v1/placements", Some(&own), None).is_none());
        assert!(refusal("/peer/v1/placements", None, Some(other)).is_none());
    }
}
