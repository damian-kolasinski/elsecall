//! One member's plan: the fleet's wants, with that member's exceptions applied,
//! minus what it has installed.
//!
//! Wants are grouped by tool. An exception replaces everything the pins want for
//! its tool on this member: leaving the tool out plans nothing, and a member's
//! versions plan those versions instead. An Xcode exception also replaces the
//! default runtimes derived from the pinned Xcode: a member kept on another
//! Xcode wants that Xcode's runtime, and one without Xcode wants none. An exception for a tool nothing pins is
//! listed and plans nothing, since Elsecall installs only what is pinned. A
//! version is a step unless the member's inventory has it; when the member
//! could not read that manager, its steps are unknown; every manager the member
//! could not read is listed as unread. A derived runtime is installed when an
//! installed version equals or extends it.
use crate::{
    pins::InstallOptions,
    plan::wants::{Outcome, Reason, ToolKey, Unresolved, Want, default_runtime},
    tool_inventory::{
        Inventory, InventoryFailure, Manager, ManagerReport, ToolName, ToolVersion,
        compare_versions, version_extends,
    },
    toolset::document::{Exception, MemberException},
};
use std::{cmp::Ordering, collections::BTreeMap};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemberPlan {
    pub(crate) steps: Vec<Step>,
    pub(crate) unresolved: Vec<Pending>,
    pub(crate) exceptions: Vec<Applied>,
    pub(crate) installed: Vec<Installed>,
    pub(crate) unread: Vec<(Manager, InventoryFailure)>,
}

/// A version to install. `exact` is false when an inexact pin resolved to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Step {
    pub(crate) key: ToolKey,
    pub(crate) version: ToolVersion,
    pub(crate) exact: bool,
    pub(crate) reasons: Vec<Reason>,
    /// The install options every pin asking for this version carries.
    pub(crate) options: InstallOptions,
}

/// A request that is shown, not planned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pending {
    pub(crate) key: ToolKey,
    pub(crate) written: Option<ToolVersion>,
    pub(crate) why: Unresolved,
    pub(crate) reasons: Vec<Reason>,
}

/// An exception that applies to this member, and the versions its tool's pins
/// asked for (empty when nothing pins the tool).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Applied {
    pub(crate) exception: MemberException,
    pub(crate) pinned: Vec<ToolVersion>,
}

/// A wanted version the member already has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Installed {
    pub(crate) key: ToolKey,
    pub(crate) version: ToolVersion,
}

pub(crate) fn plan_member(
    wants: &[Want],
    exceptions: &[MemberException],
    inventory: &Inventory,
) -> MemberPlan {
    let wants = with_member_xcode(wants, exceptions);
    let mut groups: BTreeMap<ToolKey, Vec<&Want>> = BTreeMap::new();
    for want in &wants {
        groups.entry(want.key.clone()).or_default().push(want);
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut applied = Vec::new();
    for exception in exceptions {
        let key = ToolKey {
            manager: exception.manager,
            tool: exception.tool.clone(),
        };
        let replaced = groups.remove(&key).unwrap_or_default();
        if let Exception::Versions(versions) = &exception.exception
            && !replaced.is_empty()
        {
            let options = shared_options(&replaced);
            for version in versions.versions() {
                let index = add(
                    &mut candidates,
                    &key,
                    version.clone(),
                    true,
                    Reason::Exception {
                        key: exception.key.clone(),
                    },
                    options.as_ref().unwrap_or(&InstallOptions::default()),
                );
                candidates[index].options_differ |= options.is_none();
            }
        }
        applied.push(Applied {
            exception: exception.clone(),
            pinned: written(&replaced),
        });
    }
    let mut unresolved: Vec<Pending> = Vec::new();
    for (key, wants) in groups {
        for want in wants {
            match &want.outcome {
                Outcome::Planned { version, exact } => {
                    add(
                        &mut candidates,
                        &key,
                        version.clone(),
                        *exact,
                        want.reason.clone(),
                        &want.options,
                    );
                }
                Outcome::Unresolved { written, why } => {
                    match unresolved.iter_mut().find(|pending| {
                        pending.key == key && &pending.written == written && pending.why == *why
                    }) {
                        Some(pending) => pending.reasons.push(want.reason.clone()),
                        None => unresolved.push(Pending {
                            key: key.clone(),
                            written: written.clone(),
                            why: *why,
                            reasons: vec![want.reason.clone()],
                        }),
                    }
                }
            }
        }
    }
    let mut plan = MemberPlan {
        steps: Vec::new(),
        unresolved,
        exceptions: applied,
        installed: Vec::new(),
        unread: Vec::new(),
    };
    candidates.sort_by(|left, right| {
        order(
            &left.step.key,
            &left.step.version,
            &right.step.key,
            &right.step.version,
        )
    });
    for report in &inventory.managers {
        if let ManagerReport::Failed { manager, failure } = report {
            plan.unread.push((*manager, failure.clone()));
        }
    }
    for Candidate {
        step,
        options_differ,
    } in candidates
    {
        match installed(inventory, &step) {
            Some(true) => plan.installed.push(Installed {
                key: step.key,
                version: step.version,
            }),
            Some(false) if options_differ => plan.unresolved.push(Pending {
                key: step.key,
                written: Some(step.version),
                why: Unresolved::OptionsDiffer,
                reasons: step.reasons,
            }),
            Some(false) => plan.steps.push(step),
            None => {}
        }
    }
    plan
}

/// The fleet's wants as this member sees them: with an Xcode exception, the
/// runtimes derived from the pinned Xcode give way to those of the member's own
/// versions, or to none when it leaves Xcode out.
fn with_member_xcode(wants: &[Want], exceptions: &[MemberException]) -> Vec<Want> {
    let Some(xcode) = exceptions
        .iter()
        .find(|exception| exception.manager == Manager::Xcodes)
    else {
        return wants.to_vec();
    };
    let derived = wants
        .iter()
        .any(|want| matches!(want.reason, Reason::DefaultRuntime { .. }));
    let mut kept: Vec<Want> = wants
        .iter()
        .filter(|want| !matches!(want.reason, Reason::DefaultRuntime { .. }))
        .cloned()
        .collect();
    if let (true, Exception::Versions(versions)) = (derived, &xcode.exception) {
        for version in versions.versions() {
            kept.push(Want {
                key: ToolKey {
                    manager: Manager::Simulators,
                    tool: ToolName::new("iOS").expect("iOS is a tool name"),
                },
                outcome: default_runtime(version),
                reason: Reason::ExceptionRuntime {
                    key: xcode.key.clone(),
                    xcode: version.clone(),
                },
                options: InstallOptions::default(),
            });
        }
    }
    kept
}

/// A version wanted so far, and whether its wants disagree on install options.
struct Candidate {
    step: Step,
    options_differ: bool,
}

fn add(
    candidates: &mut Vec<Candidate>,
    key: &ToolKey,
    version: ToolVersion,
    exact: bool,
    reason: Reason,
    options: &InstallOptions,
) -> usize {
    match candidates.iter_mut().position(|candidate| {
        &candidate.step.key == key
            && compare_versions(candidate.step.version.as_str(), version.as_str())
                == Ordering::Equal
    }) {
        Some(index) => {
            let candidate = &mut candidates[index];
            candidate.step.exact &= exact;
            candidate.step.reasons.push(reason);
            candidate.options_differ |= &candidate.step.options != options;
            index
        }
        None => {
            candidates.push(Candidate {
                step: Step {
                    key: key.clone(),
                    version,
                    exact,
                    reasons: vec![reason],
                    options: options.clone(),
                },
                options_differ: false,
            });
            candidates.len() - 1
        }
    }
}

/// The install options the pins an exception replaces agree on, which its own
/// versions take, since an exception names versions and no options. None when
/// they disagree: which options a member's own version needs is then unknown.
fn shared_options(replaced: &[&Want]) -> Option<InstallOptions> {
    let mut options = replaced.iter().map(|want| &want.options);
    let first = options.next()?;
    options.all(|other| other == first).then(|| first.clone())
}

/// The versions the replaced wants asked for, as written, without repeats.
fn written(wants: &[&Want]) -> Vec<ToolVersion> {
    let mut versions: Vec<ToolVersion> = Vec::new();
    for want in wants {
        let version = match (&want.reason, &want.outcome) {
            (Reason::Pin { version, .. }, _) => version.clone(),
            (_, Outcome::Planned { version, .. }) => version.clone(),
            _ => continue,
        };
        if !versions.contains(&version) {
            versions.push(version);
        }
    }
    versions
}

fn order(
    left_key: &ToolKey,
    left: &ToolVersion,
    right_key: &ToolKey,
    right: &ToolVersion,
) -> Ordering {
    left_key
        .cmp(right_key)
        .then_with(|| compare_versions(left.as_str(), right.as_str()))
}

/// Whether the member's inventory has the step's version; `None` when it could
/// not read that manager. A runtime's trailing zero segments do not count:
/// `simctl` reports iOS 26.2 for a pinned 26.2.0.
fn installed(inventory: &Inventory, step: &Step) -> Option<bool> {
    let derived = step.reasons.iter().all(Reason::derived_runtime);
    let runtime = step.key.manager == Manager::Simulators;
    let trimmed = |version: &str| {
        if runtime {
            version.trim_end_matches(".0").to_owned()
        } else {
            version.to_owned()
        }
    };
    inventory.managers.iter().find_map(|report| match report {
        ManagerReport::Read(read) if read.manager == step.key.manager => {
            Some(read.tools.iter().any(|tool| {
                tool.name == step.key.tool
                    && (compare_versions(
                        &trimmed(tool.version.as_str()),
                        &trimmed(step.version.as_str()),
                    ) == Ordering::Equal
                        || (derived
                            && version_extends(tool.version.as_str(), step.version.as_str())))
            }))
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use crate::{
        pins::{SourceFile, SourceKind},
        plan::member::*,
        tool_inventory::{Executable, InstalledTool, ManagerInventory},
        toolset::document::Toolset,
    };

    fn key(manager: Manager, tool: &str) -> ToolKey {
        ToolKey {
            manager,
            tool: ToolName::new(tool).unwrap(),
        }
    }

    fn version(text: &str) -> ToolVersion {
        ToolVersion::new(text).unwrap()
    }

    fn pin_reason(workspace: &str, written: &str) -> Reason {
        Reason::Pin {
            member: "mac-mini".parse().unwrap(),
            workspace: Some(workspace.into()),
            source: SourceFile {
                kind: SourceKind::MiseProject,
                file: "mise.toml".into(),
            },
            version: version(written),
            environment: None,
        }
    }

    fn want(manager: Manager, tool: &str, planned: &str, exact: bool, written: &str) -> Want {
        Want {
            key: key(manager, tool),
            outcome: Outcome::Planned {
                version: version(planned),
                exact,
            },
            reason: pin_reason("~/acme-ios", written),
            options: InstallOptions::default(),
        }
    }

    fn inventory(mise: &[(&str, &str)], xcodes: Option<&[&str]>) -> Inventory {
        let tools = |entries: Vec<(&str, &str)>| {
            entries
                .into_iter()
                .map(|(name, text)| InstalledTool {
                    name: ToolName::new(name).unwrap(),
                    version: version(text),
                    default: false,
                    detail: None,
                })
                .collect()
        };
        let xcodes = match xcodes {
            Some(versions) => ManagerReport::Read(
                ManagerInventory::try_new(
                    Manager::Xcodes,
                    Executable::Missing,
                    tools(versions.iter().map(|v| ("xcode", *v)).collect()),
                )
                .unwrap(),
            ),
            None => ManagerReport::Failed {
                manager: Manager::Xcodes,
                failure: InventoryFailure::timeout("xcodes version"),
            },
        };
        Inventory {
            managers: vec![
                ManagerReport::Read(
                    ManagerInventory::try_new(
                        Manager::Mise,
                        Executable::Missing,
                        tools(mise.to_vec()),
                    )
                    .unwrap(),
                ),
                xcodes,
                ManagerReport::Read(
                    ManagerInventory::try_new(Manager::Simulators, Executable::Missing, Vec::new())
                        .unwrap(),
                ),
            ],
        }
    }

    fn exceptions(text: &str, member: &str) -> Vec<MemberException> {
        Toolset::parse(text.as_bytes())
            .unwrap()
            .exceptions(&member.parse().unwrap())
    }

    #[test]
    fn a_step_is_a_wanted_version_the_member_lacks() {
        let wants = [
            want(Manager::Mise, "node", "22.19.0", true, "22.19.0"),
            want(Manager::Mise, "node", "22.19.0", false, "22"),
            want(Manager::Mise, "bun", "1.3.14", false, "latest"),
            want(Manager::Mise, "python", "3.10.4", true, "3.10.4"),
        ];
        let plan = plan_member(&wants, &[], &inventory(&[("python", "3.10.4")], Some(&[])));
        let steps: Vec<_> = plan
            .steps
            .iter()
            .map(|step| {
                (
                    step.key.tool.as_str(),
                    step.version.as_str(),
                    step.exact,
                    step.reasons.len(),
                )
            })
            .collect();
        assert_eq!(
            steps,
            [("bun", "1.3.14", false, 1), ("node", "22.19.0", false, 2)]
        );
        assert_eq!(
            plan.installed,
            [Installed {
                key: key(Manager::Mise, "python"),
                version: version("3.10.4")
            }]
        );
        assert!(plan.unread.is_empty());
    }

    #[test]
    fn an_unread_manager_hides_its_steps_but_not_the_others() {
        let wants = [
            want(Manager::Mise, "node", "22.19.0", true, "22.19.0"),
            want(Manager::Xcodes, "xcode", "26.2", true, "26.2"),
        ];
        let plan = plan_member(&wants, &[], &inventory(&[], None));
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.unread.len(), 1);
        assert_eq!(plan.unread[0].0, Manager::Xcodes);
    }

    #[test]
    fn unresolved_requests_are_grouped_with_their_reasons() {
        let pending = |workspace: &str| Want {
            key: key(Manager::Mise, "java"),
            outcome: Outcome::Unresolved {
                written: Some(version("lts")),
                why: Unresolved::Selector,
            },
            reason: pin_reason(workspace, "lts"),
            options: InstallOptions::default(),
        };
        let plan = plan_member(
            &[pending("~/a"), pending("~/b")],
            &[],
            &inventory(&[], Some(&[])),
        );
        assert!(plan.steps.is_empty());
        assert_eq!(plan.unresolved.len(), 1);
        assert_eq!(plan.unresolved[0].reasons.len(), 2);
    }

    #[test]
    fn exceptions_leave_tools_out_or_replace_their_versions() {
        let text = "[defaults.mise.tools.bun]\n\n[members.macbook-pro.xcodes.tools.xcode]\ndefault = \"26.3\"\n\n[members.macbook-pro.mise.tools.tuist]\ninstall = [\"4.205.0\"]\n";
        let wants = [
            want(Manager::Mise, "bun", "1.3.14", false, "latest"),
            want(Manager::Mise, "node", "22.19.0", true, "22.19.0"),
            want(Manager::Xcodes, "xcode", "26.2", true, "26.2"),
        ];
        let empty = inventory(&[], Some(&[]));
        let mini = plan_member(&wants, &exceptions(text, "mac-mini"), &empty);
        let tools: Vec<_> = mini
            .steps
            .iter()
            .map(|step| (step.key.tool.as_str(), step.version.as_str()))
            .collect();
        assert_eq!(tools, [("node", "22.19.0"), ("xcode", "26.2")]);
        assert_eq!(mini.exceptions.len(), 1);
        assert_eq!(mini.exceptions[0].pinned, [version("latest")]);

        let pro = plan_member(&wants, &exceptions(text, "macbook-pro"), &empty);
        let tools: Vec<_> = pro
            .steps
            .iter()
            .map(|step| (step.key.tool.as_str(), step.version.as_str()))
            .collect();
        assert_eq!(tools, [("node", "22.19.0"), ("xcode", "26.3")]);
        assert_eq!(
            pro.steps[1].reasons,
            [Reason::Exception {
                key: "members.macbook-pro.xcodes.tools.xcode".into()
            }]
        );
        let unused = pro
            .exceptions
            .iter()
            .find(|applied| applied.exception.tool.as_str() == "tuist")
            .unwrap();
        assert!(
            unused.pinned.is_empty(),
            "nothing pins tuist, so nothing is planned"
        );
    }

    fn runtime_want(xcode: &str) -> Want {
        Want {
            key: key(Manager::Simulators, "iOS"),
            outcome: Outcome::Planned {
                version: version(xcode),
                exact: true,
            },
            reason: Reason::DefaultRuntime {
                member: "mac-mini".parse().unwrap(),
                workspace: "~/acme-ios".into(),
                xcode: version(xcode),
            },
            options: InstallOptions::default(),
        }
    }

    fn with_runtimes(runtimes: &[&str]) -> Inventory {
        let mut inventory = inventory(&[], Some(&[]));
        inventory.managers[2] = ManagerReport::Read(
            ManagerInventory::try_new(
                Manager::Simulators,
                Executable::Missing,
                runtimes
                    .iter()
                    .map(|text| InstalledTool {
                        name: ToolName::new("iOS").unwrap(),
                        version: version(text),
                        default: false,
                        detail: None,
                    })
                    .collect(),
            )
            .unwrap(),
        );
        inventory
    }

    #[test]
    fn a_patch_satisfies_a_derived_runtime_but_not_a_pinned_one() {
        let zero = plan_member(&[runtime_want("26.2.0")], &[], &with_runtimes(&["26.2"]));
        assert!(
            zero.steps.is_empty(),
            "a runtime's trailing zero does not count: {zero:?}"
        );
        let derived = plan_member(&[runtime_want("26.0")], &[], &with_runtimes(&["26.0.1"]));
        assert!(derived.steps.is_empty(), "{derived:?}");
        assert_eq!(derived.installed.len(), 1);
        let mut pinned = runtime_want("26.0");
        pinned.reason = pin_reason("~/acme-ios", "26.0");
        let plan = plan_member(&[pinned], &[], &with_runtimes(&["26.0.1"]));
        assert_eq!(plan.steps.len(), 1);
    }

    #[test]
    fn a_members_xcode_exception_brings_its_own_runtime() {
        let wants = [
            want(Manager::Xcodes, "xcode", "26.2", true, "26.2"),
            runtime_want("26.2"),
        ];
        let kept = "[members.alpha.xcodes.tools.xcode]\ndefault = \"26.0\"\n";
        let plan = plan_member(&wants, &exceptions(kept, "alpha"), &with_runtimes(&[]));
        let steps: Vec<_> = plan
            .steps
            .iter()
            .map(|step| (step.key.tool.as_str(), step.version.as_str()))
            .collect();
        assert_eq!(steps, [("xcode", "26.0"), ("iOS", "26.0")]);
        assert_eq!(
            plan.steps[1].reasons,
            [Reason::ExceptionRuntime {
                key: "members.alpha.xcodes.tools.xcode".into(),
                xcode: version("26.0")
            }]
        );
        let older = "[members.alpha.xcodes.tools.xcode]\ndefault = \"16.4\"\n";
        let plan = plan_member(&wants, &exceptions(older, "alpha"), &with_runtimes(&[]));
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.unresolved[0].why, Unresolved::OlderXcode);
        let none = "[members.alpha.xcodes.tools.xcode]\n";
        let plan = plan_member(&wants, &exceptions(none, "alpha"), &with_runtimes(&[]));
        assert!(
            plan.steps.is_empty() && plan.unresolved.is_empty(),
            "{plan:?}"
        );
    }

    fn with_options(mut want: Want, toml: &str) -> Want {
        let table: toml::Table = toml::from_str(toml).unwrap();
        want.options =
            InstallOptions::from_table("mise.toml", "tools.x", &table, &Default::default())
                .unwrap();
        want
    }

    #[test]
    fn steps_carry_install_options_and_disagreeing_ones_are_unresolved() {
        let prefixed = |version: &str| {
            with_options(
                want(Manager::Mise, "github:example/tool", version, true, version),
                "version_prefix = \"tool-\"",
            )
        };
        let wants = [
            prefixed("1.0.0"),
            prefixed("1.0.0"),
            prefixed("2.0.0"),
            want(Manager::Mise, "github:example/tool", "2.0.0", true, "2.0.0"),
        ];
        let plan = plan_member(&wants, &[], &inventory(&[], Some(&[])));
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].version, version("1.0.0"));
        assert_eq!(plan.steps[0].reasons.len(), 2);
        assert_eq!(plan.steps[0].options.to_map()["version_prefix"], "tool-");
        assert_eq!(plan.unresolved.len(), 1);
        assert_eq!(plan.unresolved[0].why, Unresolved::OptionsDiffer);
        assert_eq!(plan.unresolved[0].written, Some(version("2.0.0")));

        let installed = plan_member(
            &wants,
            &[],
            &inventory(&[("github:example/tool", "2.0.0")], Some(&[])),
        );
        assert!(
            installed.unresolved.is_empty(),
            "an installed version needs no options"
        );

        let kept = "[members.alpha.mise.tools.\"github:example/tool\"]\ninstall = [\"3.0.0\"]\n";
        let plan = plan_member(
            &wants[..1],
            &exceptions(kept, "alpha"),
            &inventory(&[], Some(&[])),
        );
        assert_eq!(plan.steps[0].version, version("3.0.0"));
        assert_eq!(
            plan.steps[0].options.to_map()["version_prefix"],
            "tool-",
            "an exception's versions take the options its pins agree on"
        );
        let plan = plan_member(
            &wants[2..],
            &exceptions(kept, "alpha"),
            &inventory(&[], Some(&[])),
        );
        assert!(plan.steps.is_empty(), "{plan:?}");
        assert_eq!(plan.unresolved[0].why, Unresolved::OptionsDiffer);
    }

    #[test]
    fn every_manager_the_member_could_not_read_is_listed() {
        let mut inventory = inventory(&[], Some(&[]));
        inventory.managers[2] = ManagerReport::Failed {
            manager: Manager::Simulators,
            failure: InventoryFailure::timeout("simctl runtime list -j"),
        };
        let plan = plan_member(&[], &[], &inventory);
        assert_eq!(plan.unread.len(), 1);
        assert_eq!(plan.unread[0].0, Manager::Simulators);
    }
}
