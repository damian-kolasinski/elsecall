//! What in the fleet view needs attention: members awaiting provisioning and
//! members on another build than the master. Derived from the view each time it is
//! read, so an item clears as soon as its condition does.
use crate::fleet_view::{MemberView, Role};
use elsecall_contract::attention::{Action, Category, Item};
use elsecall_contract::build_identity::Build;
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
            let skewed = (member.version_skew == Some(true))
                .then(|| version_skew(&member.name, master, member.build.as_ref(), master_build));
            awaiting.into_iter().chain(skewed)
        })
        .collect()
}

/// `elsecall` followed by `arguments`.
fn elsecall(arguments: &[&str]) -> Vec<String> {
    std::iter::once("elsecall")
        .chain(arguments.iter().copied())
        .map(str::to_owned)
        .collect()
}

fn awaiting_provisioning(member: &MemberName, master: &MemberName) -> Item {
    let category = Category::AwaitingProvisioning;
    Item {
        id: Item::id(category, member.as_str(), None),
        member: member.to_string(),
        category,
        subject: None,
        code: "member_awaiting_provisioning".into(),
        message: format!(
            "{member} is awaiting provisioning: placement gives it no work until it is marked ready. Work required on it by name still runs there."
        ),
        hint: format!(
            "Install the tools the fleet's work pins on {member}, then run elsecall fleet ready {member} on {master}."
        ),
        since: None,
        actions: vec![Action {
            summary: format!(
                "Mark {member} ready once its tools are installed, so placement may give it work."
            ),
            command: elsecall(&["fleet", "ready", member.as_str()]),
            run_on: master.to_string(),
            person_only: false,
        }],
    }
}

fn version_skew(
    member: &MemberName,
    master: &MemberName,
    runs: Option<&Build>,
    wanted: Option<&Build>,
) -> Item {
    let category = Category::VersionSkew;
    let build = |b: Option<&Build>| {
        b.map_or_else(
            || "an unknown build".to_owned(),
            |b| format!("{} (build {})", b.version, b.build),
        )
    };
    // Upgrading a member that is ahead of the master installs an older build on
    // it; whether to do that or update the master instead is a person's call.
    let ahead = matches!((runs, wanted), (Some(runs), Some(wanted)) if newer(&runs.version, &wanted.version));
    let upgrade = elsecall(&["fleet", "upgrade", "--member", member.as_str()]);
    Item {
        id: Item::id(category, member.as_str(), None),
        member: member.to_string(),
        category,
        subject: None,
        code: "version_skew".into(),
        message: format!(
            "{member} runs {} and the master {master} runs {}: placement, configuration sync, workspace checks, reports and tool inventories between them are refused.",
            build(runs),
            build(wanted)
        ),
        hint: if ahead {
            format!(
                "{member} is ahead of the master: update {master} first, or run elsecall fleet upgrade --member {member} on {master} to put the master's older build on it."
            )
        } else {
            format!(
                "Run elsecall fleet upgrade --member {member} on {master} so {member} runs the master's build."
            )
        },
        since: None,
        actions: vec![Action {
            summary: if ahead {
                format!(
                    "Replace {member}'s newer build with the master's older one and restart its service."
                )
            } else {
                format!("Install the master's build on {member} and restart its service.")
            },
            command: upgrade,
            run_on: master.to_string(),
            person_only: ahead,
        }],
    }
}

/// Whether dotted version `a` is later than `b`, comparing numeric parts; a part
/// that is not a number ends the comparison.
fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> { v.split('.').map_while(|p| p.parse().ok()).collect() };
    parts(a) > parts(b)
}

#[cfg(test)]
mod tests {
    use crate::fleet_attention::*;
    use crate::fleet_view::Reach;
    use elsecall_contract::failure::Failure;
    use serde_json::json;

    fn member(name: &str, role: Role, readiness: Readiness, build: &str, skew: bool) -> MemberView {
        versioned(name, role, readiness, "0.4.0", build, skew)
    }

    fn versioned(
        name: &str,
        role: Role,
        readiness: Readiness,
        version: &str,
        build: &str,
        skew: bool,
    ) -> MemberView {
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
                version: version.into(),
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
                    "subject": null,
                    "code": "member_awaiting_provisioning",
                    "message": "macbook-pro is awaiting provisioning: placement gives it no work until it is marked ready. Work required on it by name still runs there.",
                    "hint": "Install the tools the fleet's work pins on macbook-pro, then run elsecall fleet ready macbook-pro on mac-mini.",
                    "since": null,
                    "actions": [{
                        "summary": "Mark macbook-pro ready once its tools are installed, so placement may give it work.",
                        "command": ["elsecall", "fleet", "ready", "macbook-pro"],
                        "run_on": "mac-mini",
                        "person_only": false
                    }]
                },
                {
                    "id": "version_skew:macbook-pro",
                    "member": "macbook-pro",
                    "category": "version_skew",
                    "subject": null,
                    "code": "version_skew",
                    "message": "macbook-pro runs 0.4.0 (build fedcba9876543210) and the master mac-mini runs 0.4.0 (build 0123456789abcdef): placement, configuration sync, workspace checks, reports and tool inventories between them are refused.",
                    "hint": "Run elsecall fleet upgrade --member macbook-pro on mac-mini so macbook-pro runs the master's build.",
                    "since": null,
                    "actions": [{
                        "summary": "Install the master's build on macbook-pro and restart its service.",
                        "command": ["elsecall", "fleet", "upgrade", "--member", "macbook-pro"],
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

    #[test]
    fn a_member_ahead_of_the_master_is_left_to_a_person() {
        let master: MemberName = "mac-mini".parse().unwrap();
        let members = [
            versioned(
                "mac-mini",
                Role::Master,
                Readiness::Ready,
                "0.4.0",
                MASTER_BUILD,
                false,
            ),
            versioned(
                "macbook-pro",
                Role::Member,
                Readiness::Ready,
                "0.10.0",
                "fedcba9876543210",
                true,
            ),
        ];
        let found = items(&members, Some(&master));
        assert_eq!(found.len(), 1);
        assert!(found[0].actions[0].person_only, "{found:?}");
        assert!(found[0].hint.contains("update mac-mini first"), "{found:?}");
        assert!(newer("0.10.0", "0.4.0") && !newer("0.4.0", "0.4.0") && !newer("0.3.9", "0.4"));
    }
}
