//! Scenes of provisioning: a member installing its tools, one waiting for a
//! person, one ready, the Setup dialog with failures and person steps, and the
//! attention list with a secret consent's checkbox.
use super::{AppState, Phase, scene_fleets::members_snapshot};
use crate::fleet_source::{FleetSnapshot, Gate, Reachability, Release};
use crate::provisioning_source::{Applied, AppliedState, Apply, Plan, Setup, Tool, Unresolved};
use elsecall_contract::attention::{Action, Category, Item};
use elsecall_contract::scheduling_model::timestamp::Timestamp;
use elsecall_fleet::provisioning_model::{ApplyState, FailedStep, ProvisioningSummary};
use elsecall_peers::roster::Readiness;

const AT: &str = "2026-11-20T09:30:00Z";

/// The members scene with macbook-pro online, awaiting provisioning or not,
/// and its latest apply.
fn fleet(readiness: Readiness, apply: Option<ProvisioningSummary>) -> FleetSnapshot {
    let mut snapshot = members_snapshot();
    snapshot.members[0].release = Release::Matching {
        version: "0.4.0".into(),
    };
    let mac = &mut snapshot.members[2];
    mac.readiness = readiness;
    mac.reach = Reachability::Reachable;
    mac.gate = Gate::Open;
    mac.release = Release::Matching {
        version: "0.4.0".into(),
    };
    mac.provisioning = apply;
    snapshot
}

fn show(state: &mut AppState, snapshot: FleetSnapshot) {
    state.follow_service = false;
    state.refreshed = Some("14:32:05".into());
    state.phase = Phase::Ready(snapshot);
}

fn at() -> Timestamp {
    Timestamp::parse_rfc3339(AT).unwrap()
}

fn summary(
    state: ApplyState,
    steps: usize,
    installed: usize,
    failed: Vec<FailedStep>,
) -> ProvisioningSummary {
    ProvisioningSummary {
        state,
        started_at: at(),
        finished_at: (state == ApplyState::Finished).then(at),
        steps,
        installed,
        failed,
    }
}

fn failed(manager: &str, tool: &str, version: &str, stage: &str, code: &str) -> FailedStep {
    FailedStep {
        manager: manager.into(),
        tool: tool.into(),
        version: version.into(),
        stage: stage.into(),
        code: code.into(),
        failed_at: at(),
    }
}

fn command(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

/// The items the service derives for macbook-pro's failed Xcode step and the
/// runtime that needed it.
fn privileges_items() -> Vec<Item> {
    let category = Category::ProvisioningStepFailed;
    vec![
        Item {
            id: Item::id(category, "macbook-pro", Some("xcodes:xcode@26.2")),
            member: "macbook-pro".into(),
            category,
            subject: Some("xcodes:xcode@26.2".into()),
            code: "xcode_privileges_missing".into(),
            message: "Installing Xcode 26.2 on macbook-pro failed at its privileges stage: Elsecall may not place, select or license Xcode there yet.".into(),
            hint: "A person runs elsecall tools privileges install on macbook-pro, with an administrator's password; the master then copies Xcode there by itself.".into(),
            since: Some(at()),
            actions: vec![
                Action {
                    summary: "On macbook-pro, let Elsecall place, select and license Xcode.".into(),
                    command: command(&["elsecall", "tools", "privileges", "install"]),
                    run_on: "macbook-pro".into(),
                    person_only: true,
                },
                Action {
                    summary: "Apply macbook-pro's plan again, which retries this step.".into(),
                    command: command(&["elsecall", "tools", "apply", "--member", "macbook-pro"]),
                    run_on: "mac-mini".into(),
                    person_only: false,
                },
            ],
        },
        Item {
            id: Item::id(category, "macbook-pro", Some("simulators:iOS@26.2")),
            member: "macbook-pro".into(),
            category,
            subject: Some("simulators:iOS@26.2".into()),
            code: "xcode_missing".into(),
            message: "Installing simulators iOS 26.2 on macbook-pro failed at its download stage: no Xcode installed there knows the runtime.".into(),
            hint: "It downloads once Xcode 26.2 is installed on macbook-pro.".into(),
            since: Some(at()),
            actions: vec![Action {
                summary: "Apply macbook-pro's plan again, which retries this step.".into(),
                command: command(&["elsecall", "tools", "apply", "--member", "macbook-pro"]),
                run_on: "mac-mini".into(),
                person_only: false,
            }],
        },
    ]
}

fn apple_id_item() -> Item {
    let category = Category::XcodeAppleIdRequired;
    Item {
        id: Item::id(category, "mac-mini", Some("26.4")),
        member: "mac-mini".into(),
        category,
        subject: Some("26.4".into()),
        code: "xcode_not_on_master".into(),
        message: "Xcode 26.4 is pinned, and the master, mac-mini, has none to copy: members get Xcode only from the master.".into(),
        hint: "A person downloads Xcode 26.4 onto mac-mini with an Apple ID (xcodes install 26.4, or developer.apple.com/download); the master then copies it to the members that need it.".into(),
        since: Some(at()),
        actions: vec![Action {
            summary: "Once Xcode 26.4 is installed on mac-mini, apply the plan again.".into(),
            command: command(&["elsecall", "tools", "apply"]),
            run_on: "mac-mini".into(),
            person_only: true,
        }],
    }
}

fn waiting_for_privileges() -> FleetSnapshot {
    let mut snapshot = fleet(
        Readiness::AwaitingProvisioning,
        Some(summary(
            ApplyState::Finished,
            3,
            1,
            vec![
                failed(
                    "xcodes",
                    "xcode",
                    "26.2",
                    "privileges",
                    "xcode_privileges_missing",
                ),
                failed("simulators", "iOS", "26.2", "download", "xcode_missing"),
            ],
        )),
    );
    snapshot.attention = privileges_items();
    snapshot
}

pub(super) fn scene_provisioning_running(state: &mut AppState) {
    show(
        state,
        fleet(
            Readiness::AwaitingProvisioning,
            Some(summary(ApplyState::Running, 3, 1, Vec::new())),
        ),
    );
}

pub(super) fn scene_provisioning_starting(state: &mut AppState) {
    show(state, fleet(Readiness::AwaitingProvisioning, None));
    state.provisioning.asked("macbook-pro");
}

pub(super) fn scene_provisioning_needs_you(state: &mut AppState) {
    show(state, waiting_for_privileges());
}

pub(super) fn scene_provisioning_ready(state: &mut AppState) {
    show(
        state,
        fleet(
            Readiness::Ready,
            Some(summary(ApplyState::Finished, 3, 3, Vec::new())),
        ),
    );
}

fn tool(manager: &str, tool: &str, version: &str) -> Tool {
    Tool {
        manager: manager.into(),
        tool: tool.into(),
        version: Some(version.into()),
    }
}

fn applied(manager: &str, name: &str, version: &str, state: AppliedState) -> Applied {
    Applied {
        tool: tool(manager, name, version),
        state,
    }
}

fn failure(stage: &str, code: &str) -> AppliedState {
    AppliedState::Failed {
        stage: stage.into(),
        code: code.into(),
    }
}

/// macbook-pro's setup: Xcode waits for privileges, its runtime for Xcode, and
/// a pin the master cannot resolve.
pub(super) fn scene_provisioning_setup(state: &mut AppState) {
    show(state, waiting_for_privileges());
    state.provisioning.show(
        "macbook-pro",
        Setup {
            plan: Some(Ok(Plan {
                steps: vec![
                    tool("xcodes", "xcode", "26.2"),
                    tool("simulators", "iOS", "26.2"),
                ],
                unresolved: vec![Unresolved {
                    tool: tool("mise", "python", "3.10.4"),
                    hint: "Install python 3.10.4 on the master, mac-mini, or pin a version it has: Elsecall installs on members only what the master has.".into(),
                }],
                partial: false,
            })),
            apply: Some(Ok(Some(Apply {
                finished: true,
                steps: vec![
                    applied("mise", "node", "22.19.0", AppliedState::Installed),
                    applied(
                        "xcodes",
                        "xcode",
                        "26.2",
                        failure("privileges", "xcode_privileges_missing"),
                    ),
                    applied(
                        "simulators",
                        "iOS",
                        "26.2",
                        failure("download", "xcode_missing"),
                    ),
                ],
            }))),
        },
    );
}

/// An Xcode no member can copy: the master's row needs a person with an
/// Apple ID, and the member's setup says so.
pub(super) fn scene_provisioning_setup_apple_id(state: &mut AppState) {
    let mut snapshot = fleet(
        Readiness::AwaitingProvisioning,
        Some(summary(
            ApplyState::Finished,
            2,
            0,
            vec![
                failed("xcodes", "xcode", "26.4", "copy", "xcode_not_on_master"),
                failed("simulators", "iOS", "26.4", "download", "xcode_missing"),
            ],
        )),
    );
    snapshot.attention = vec![apple_id_item()];
    show(state, snapshot);
    state.provisioning.show(
        "macbook-pro",
        Setup {
            plan: Some(Ok(Plan {
                steps: vec![
                    tool("xcodes", "xcode", "26.4"),
                    tool("simulators", "iOS", "26.4"),
                ],
                unresolved: Vec::new(),
                partial: false,
            })),
            apply: Some(Ok(Some(Apply {
                finished: true,
                steps: vec![
                    applied(
                        "xcodes",
                        "xcode",
                        "26.4",
                        failure("copy", "xcode_not_on_master"),
                    ),
                    applied(
                        "simulators",
                        "iOS",
                        "26.4",
                        failure("download", "xcode_missing"),
                    ),
                ],
            }))),
        },
    );
}

/// The window on a member, not the master: the plan is the master's to
/// read, and the latest apply shows its running step.
pub(super) fn scene_provisioning_setup_member(state: &mut AppState) {
    let mut snapshot = fleet(
        Readiness::AwaitingProvisioning,
        Some(summary(ApplyState::Running, 2, 1, Vec::new())),
    );
    for row in &mut snapshot.members {
        row.this_member = row.name == "macbook-pro";
    }
    show(state, snapshot);
    state.provisioning.show(
        "macbook-pro",
        Setup {
            plan: Some(Err(
                "Only the master, mac-mini, plans the fleet: pins are judged against what it has installed.".into(),
            )),
            apply: Some(Ok(Some(Apply {
                finished: false,
                steps: vec![
                    applied("mise", "node", "22.19.0", AppliedState::Installed),
                    applied("mise", "bun", "1.3.14", AppliedState::Running),
                ],
            }))),
        },
    );
}

pub(super) fn scene_provisioning_setup_loading(state: &mut AppState) {
    show(
        state,
        fleet(
            Readiness::AwaitingProvisioning,
            Some(summary(ApplyState::Running, 3, 1, Vec::new())),
        ),
    );
    state.provisioning.show("macbook-pro", Setup::default());
}

pub(super) fn scene_attention_consent(state: &mut AppState) {
    let mut snapshot = fleet(Readiness::Ready, None);
    let category = Category::SecretConsent;
    let confirm = |always: bool| {
        let mut words = vec![
            "elsecall",
            "sync",
            "consent",
            "~/.config/acme",
            "--confirm",
            "c-4f2a",
        ];
        if always {
            words.push("--always");
        }
        command(&words)
    };
    snapshot.attention = vec![Item {
        id: Item::id(category, "mac-mini", Some("~/.config/acme")),
        member: "mac-mini".into(),
        category,
        subject: Some("~/.config/acme".into()),
        code: "secret_consent".into(),
        message: "Files in ~/.config/acme look like secrets (.env, deploy.pem): their latest change waits on mac-mini until a person confirms it.".into(),
        hint: "Review them with elsecall sync consent ~/.config/acme on mac-mini; a person confirms from their own terminal.".into(),
        since: None,
        actions: vec![
            Action {
                summary: "Sync this change to every member, and ask again at the next one.".into(),
                command: confirm(false),
                run_on: "mac-mini".into(),
                person_only: true,
            },
            Action {
                summary: "Sync this change, and changes to these files automatically from now on.".into(),
                command: confirm(true),
                run_on: "mac-mini".into(),
                person_only: true,
            },
        ],
    }];
    show(state, snapshot);
    let id = state.shown().unwrap().attention[0].id.clone();
    state.provisioning.toggle_always(&id);
}
