//! What in the fleet view needs attention: members awaiting provisioning,
//! members on another build than the master, and placed runs that failed for
//! lack of a tool nothing pins. Derived from the view each time it is read, so an
//! item clears as soon as its condition does.
use crate::config_sync_model::consent::ConsentRequest;
use crate::fleet_view::{MemberView, Reach, Role, UnpinnedTool};
use elsecall_contract::attention::{Action, Category, Item};
use elsecall_contract::build_identity::Build;
use elsecall_peers::roster::{MemberName, Readiness};
use elsecall_scheduling::missing_tool::Manager;

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
    let waiting = members
        .iter()
        .filter(|m| matches!(m.role, Role::Master))
        .filter_map(|m| match &m.reach {
            Reach::Reachable { status, .. } => status.config_sync.as_ref(),
            Reach::Unreachable { .. } => None,
        })
        .flat_map(|summary| &summary.consent)
        .map(|request| secret_consent(request, master));
    members
        .iter()
        .flat_map(|member| {
            let awaiting = (member.readiness == Readiness::AwaitingProvisioning)
                .then(|| awaiting_provisioning(&member.name, master));
            let skewed = (member.version_skew == Some(true))
                .then(|| version_skew(&member.name, master, member.build.as_ref(), master_build));
            awaiting.into_iter().chain(skewed)
        })
        .chain(waiting)
        .collect()
}

/// One `secret_consent` item per synced path whose change waits on the master,
/// with two confirmations, both for a person only: this change, or this change
/// and later ones to the same files.
fn secret_consent(request: &ConsentRequest, master: &MemberName) -> Item {
    let category = Category::SecretConsent;
    let path = request.path.to_string();
    let names: Vec<&str> = request.files.iter().map(|f| f.file.as_str()).collect();
    let confirm = |always: bool| {
        let mut words = vec!["sync", "consent", &path, "--confirm", &request.consent_id];
        if always {
            words.push("--always");
        }
        elsecall(&words)
    };
    Item {
        id: Item::id(category, master.as_str(), Some(&path)),
        member: master.to_string(),
        category,
        subject: Some(path.clone()),
        code: "secret_consent".into(),
        message: format!(
            "Files in {path} look like secrets ({}): their latest change waits on {master} until a person confirms it.",
            names.join(", ")
        ),
        hint: format!(
            "Review them with elsecall sync consent {path} on {master}; a person confirms from their own terminal."
        ),
        since: None,
        actions: vec![
            Action {
                summary: "Sync this change to every member, and ask again at the next one.".into(),
                command: confirm(false),
                run_on: master.to_string(),
                person_only: true,
            },
            Action {
                summary: "Sync this change, and changes to these files automatically from now on."
                    .into(),
                command: confirm(true),
                run_on: master.to_string(),
                person_only: true,
            },
        ],
    }
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

/// One `missing_tool_pin` item per member and tool: a run `own` placed failed on
/// that member because the tool is missing there, and nothing pins it.
pub(crate) fn missing_tool_pins(unpinned: &[UnpinnedTool], own: &MemberName) -> Vec<Item> {
    unpinned
        .iter()
        .map(|tool| missing_tool_pin(tool, own))
        .collect()
}

fn missing_tool_pin(unpinned: &UnpinnedTool, own: &MemberName) -> Item {
    let category = Category::MissingToolPin;
    let standing = &unpinned.standing;
    let (member, tool) = (&standing.member, standing.missing.tool.as_str());
    let pin_in = match &standing.workspace {
        Some(workspace) if !unpinned.pin_in.starts_with('~') => {
            format!("{workspace}/{}", unpinned.pin_in)
        }
        _ => unpinned.pin_in.clone(),
    };
    let lacking = match &standing.missing.called {
        Some(called) => format!("{called} needs {tool}, which"),
        None => tool.to_owned(),
    };
    let mut actions = vec![Action {
        summary: format!(
            "Pin {tool} in {pin_in}, so every member installs it (recommended); then this command shows the pin."
        ),
        command: match &standing.workspace {
            Some(workspace) => elsecall(&["tools", "pins", workspace]),
            None => elsecall(&["fleet"]),
        },
        run_on: own.to_string(),
        person_only: false,
    }];
    // A backend name (`npm:…`) is not offered for installing by hand.
    if standing.missing.manager == Manager::Mise && !tool.contains(':') {
        actions.push(Action {
            summary: format!(
                "Not recommended: install {tool} on {member} alone, outside any pin. The next member without it fails the same way."
            ),
            command: elsecall(&[
                "run",
                "--member",
                member,
                "--",
                "mise",
                "use",
                "--global",
                &format!("{tool}@latest"),
            ]),
            run_on: own.to_string(),
            person_only: true,
        });
    }
    Item {
        id: Item::id(category, member, Some(tool)),
        member: member.clone(),
        category,
        subject: Some(tool.to_owned()),
        code: "missing_tool_pin".into(),
        message: format!(
            "Run {} failed on {member}: {lacking} is not installed there, and nothing pins it.",
            standing.run_id
        ),
        hint: format!(
            "Pin {tool} in {pin_in} so every member installs it. Installing it on {member} by hand fixes only {member}."
        ),
        since: Some(standing.ended_at),
        actions,
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
    fn a_run_that_hit_an_unpinned_tool_names_the_member_tool_run_and_file() {
        use elsecall_scheduling::missing_tool::{MissingTool, Standing, ToolName};
        let own: MemberName = "mac-mini".parse().unwrap();
        let standing =
            |manager, tool: &str, called: Option<&str>, workspace: Option<&str>| UnpinnedTool {
                standing: Standing {
                    run_id: elsecall_contract::scheduling_model::run_id::RunId::parse(
                        "r-000000000001",
                    )
                    .unwrap(),
                    member: "macbook-pro".into(),
                    missing: MissingTool {
                        manager,
                        tool: ToolName::new(tool).unwrap(),
                        called: called.map(|c| ToolName::new(c).unwrap()),
                    },
                    workspace: workspace.map(str::to_owned),
                    ended_at:
                        elsecall_contract::scheduling_model::timestamp::Timestamp::from_millis(
                            1_000,
                        ),
                },
                pin_in: match (manager, workspace) {
                    (Manager::Xcodes, _) => ".xcode-version".into(),
                    (_, Some(_)) => "mise.toml".into(),
                    (_, None) => "~/.config/mise/config.toml".into(),
                },
            };
        let items = missing_tool_pins(
            &[
                standing(Manager::Mise, "bun", None, Some("/Users/me/acme-ios")),
                standing(
                    Manager::Xcodes,
                    "xcode",
                    Some("xcodebuild"),
                    Some("/Users/me/acme-ios"),
                ),
                standing(Manager::Mise, "tuist", None, None),
            ],
            &own,
        );
        let bun = serde_json::to_value(&items[0]).unwrap();
        assert_eq!(bun["id"], "missing_tool_pin:macbook-pro:bun");
        assert_eq!(bun["category"], "missing_tool_pin");
        assert_eq!(bun["subject"], "bun");
        assert_eq!(
            bun["message"],
            "Run r-000000000001 failed on macbook-pro: bun is not installed there, and nothing pins it."
        );
        assert_eq!(
            bun["hint"],
            "Pin bun in /Users/me/acme-ios/mise.toml so every member installs it. Installing it on macbook-pro by hand fixes only macbook-pro."
        );
        assert_eq!(
            bun["actions"][0]["command"],
            json!(["elsecall", "tools", "pins", "/Users/me/acme-ios"])
        );
        assert_eq!(bun["actions"][0]["person_only"], false);
        assert_eq!(
            bun["actions"][1]["command"],
            json!([
                "elsecall",
                "run",
                "--member",
                "macbook-pro",
                "--",
                "mise",
                "use",
                "--global",
                "bun@latest"
            ])
        );
        assert_eq!(
            bun["actions"][1]["person_only"], true,
            "not recommended, a person's call"
        );

        let xcode = &items[1];
        assert!(
            xcode
                .message
                .contains("xcodebuild needs xcode, which is not installed"),
            "{xcode:?}"
        );
        assert!(
            xcode.hint.contains("/Users/me/acme-ios/.xcode-version"),
            "{xcode:?}"
        );
        assert_eq!(xcode.actions.len(), 1, "Xcode has no by-hand command");

        let outside = &items[2];
        assert!(
            outside.hint.contains("~/.config/mise/config.toml"),
            "{outside:?}"
        );
        assert_eq!(outside.actions[0].command, elsecall(&["fleet"]));
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
