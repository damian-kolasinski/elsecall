//! What in the fleet view needs attention: members awaiting provisioning and
//! members on another build than the master. Derived from the view each time it is
//! read, so an item clears as soon as its condition does.
use crate::fleet_view::{MemberView, Role};
use elsecall_contract::attention::{Action, Category, Item};
use elsecall_peers::roster::{MemberName, Readiness};

/// The items `members` call for, member by member in roster order. Fleet changes
/// run on `master`; without one (no roster), nothing can be acted on.
pub(crate) fn items(members: &[MemberView], master: Option<&MemberName>) -> Vec<Item> {
    let Some(master) = master else {
        return Vec::new();
    };
    let master_build = members
        .iter()
        .find(|m| matches!(m.role, Role::Master))
        .and_then(|m| m.build.as_ref());
    members
        .iter()
        .flat_map(|member| {
            let awaiting = (member.readiness == Readiness::AwaitingProvisioning)
                .then(|| awaiting_provisioning(&member.name, master));
            let skewed = (member.version_skew == Some(true)).then(|| {
                let build = |b: Option<&elsecall_contract::build_identity::Build>| {
                    b.map_or_else(
                        || "an unknown build".to_owned(),
                        |b| format!("build {}", b.build),
                    )
                };
                version_skew(
                    &member.name,
                    master,
                    &build(member.build.as_ref()),
                    &build(master_build),
                )
            });
            awaiting.into_iter().chain(skewed)
        })
        .collect()
}

fn awaiting_provisioning(member: &MemberName, master: &MemberName) -> Item {
    let category = Category::AwaitingProvisioning;
    let ready = format!("elsecall fleet ready {member}");
    Item {
        id: Item::id(category, member.as_str(), None),
        member: member.to_string(),
        category,
        code: "member_awaiting_provisioning".into(),
        message: format!(
            "{member} is awaiting provisioning: placement gives it no work until it is marked ready. Work required on it by name still runs there."
        ),
        hint: format!(
            "Install the tools the fleet's work pins on {member}, then run {ready} on {master}."
        ),
        since: None,
        actions: vec![Action {
            summary: format!(
                "Mark {member} ready once its tools are installed, so placement may give it work."
            ),
            command: ready,
            run_on: master.to_string(),
            person_only: false,
        }],
    }
}

fn version_skew(member: &MemberName, master: &MemberName, runs: &str, wanted: &str) -> Item {
    let category = Category::VersionSkew;
    let upgrade = format!("elsecall fleet upgrade --member {member}");
    Item {
        id: Item::id(category, member.as_str(), None),
        member: member.to_string(),
        category,
        code: "version_skew".into(),
        message: format!(
            "{member} runs {runs} and the master {master} runs {wanted}: placement, configuration sync, workspace checks, reports and tool inventories between them are refused."
        ),
        hint: format!("Run {upgrade} on {master} so {member} runs the master's build."),
        since: None,
        actions: vec![Action {
            summary: format!("Install the master's build on {member} and restart its service."),
            command: upgrade,
            run_on: master.to_string(),
            person_only: false,
        }],
    }
}

#[cfg(test)]
mod tests {
    use crate::fleet_attention::*;
    use crate::fleet_view::Reach;
    use elsecall_contract::build_identity::Build;
    use elsecall_contract::failure::Failure;
    use serde_json::json;

    fn member(name: &str, role: Role, readiness: Readiness, build: &str, skew: bool) -> MemberView {
        MemberView {
            name: name.parse().unwrap(),
            key: elsecall_peers::member_key::MemberKey::generate()
                .unwrap()
                .public(),
            endpoints: Vec::new(),
            role,
            readiness,
            this_member: false,
            reach: Reach::Unreachable {
                error: Failure::new("member_unavailable", "gone", "check"),
            },
            latest_placed_run: None,
            config_sync: None,
            build: Some(Build {
                version: "0.4.0".into(),
                build: build.parse().unwrap(),
            }),
            version_skew: Some(skew),
            blocked: None,
            outside_warnings: Vec::new(),
        }
    }

    const MASTER_BUILD: &str = "0123456789abcdef";

    #[test]
    fn members_awaiting_provisioning_or_on_another_build_need_attention() {
        let master: MemberName = "mac-mini".parse().unwrap();
        let members = [
            member(
                "mac-mini",
                Role::Master,
                Readiness::Ready,
                MASTER_BUILD,
                false,
            ),
            member(
                "macbook-pro",
                Role::Member,
                Readiness::AwaitingProvisioning,
                "fedcba9876543210",
                true,
            ),
            member(
                "mac-studio",
                Role::Member,
                Readiness::Ready,
                MASTER_BUILD,
                false,
            ),
        ];
        let rendered = serde_json::to_value(items(&members, Some(&master))).unwrap();
        assert_eq!(
            rendered,
            json!([
                {
                    "id": "awaiting_provisioning:macbook-pro",
                    "member": "macbook-pro",
                    "category": "awaiting_provisioning",
                    "code": "member_awaiting_provisioning",
                    "message": "macbook-pro is awaiting provisioning: placement gives it no work until it is marked ready. Work required on it by name still runs there.",
                    "hint": "Install the tools the fleet's work pins on macbook-pro, then run elsecall fleet ready macbook-pro on mac-mini.",
                    "since": null,
                    "actions": [{
                        "summary": "Mark macbook-pro ready once its tools are installed, so placement may give it work.",
                        "command": "elsecall fleet ready macbook-pro",
                        "run_on": "mac-mini",
                        "person_only": false
                    }]
                },
                {
                    "id": "version_skew:macbook-pro",
                    "member": "macbook-pro",
                    "category": "version_skew",
                    "code": "version_skew",
                    "message": "macbook-pro runs build fedcba9876543210 and the master mac-mini runs build 0123456789abcdef: placement, configuration sync, workspace checks, reports and tool inventories between them are refused.",
                    "hint": "Run elsecall fleet upgrade --member macbook-pro on mac-mini so macbook-pro runs the master's build.",
                    "since": null,
                    "actions": [{
                        "summary": "Install the master's build on macbook-pro and restart its service.",
                        "command": "elsecall fleet upgrade --member macbook-pro",
                        "run_on": "mac-mini",
                        "person_only": false
                    }]
                }
            ])
        );
    }

    #[test]
    fn a_ready_fleet_on_one_build_or_without_a_master_needs_nothing() {
        let master: MemberName = "mac-mini".parse().unwrap();
        let ready = [
            member(
                "mac-mini",
                Role::Master,
                Readiness::Ready,
                MASTER_BUILD,
                false,
            ),
            member(
                "macbook-pro",
                Role::Member,
                Readiness::Ready,
                MASTER_BUILD,
                false,
            ),
        ];
        assert!(items(&ready, Some(&master)).is_empty());
        let awaiting = [member(
            "macbook-pro",
            Role::Member,
            Readiness::AwaitingProvisioning,
            MASTER_BUILD,
            false,
        )];
        assert!(
            items(&awaiting, None).is_empty(),
            "nothing can act without a master"
        );
    }
}
