//! What one source of pins asks every member to have, judged on the master.
//!
//! A pin becomes a [`Want`]: a version to plan, or an unresolved request shown
//! with a hint. Pins are judged against what the master has installed, since
//! jobs run there first and the tools are installed there:
//!
//! - mise: an exact pin is planned only when the master has that version, since
//!   `22` with no `22.x` installed reads as exact although mise would pick the
//!   newest `22.x` upstream; otherwise it is unresolved. An inexact pin is
//!   planned at the version it resolves to on the master. A selector (`lts`,
//!   `ref:`…) is unresolved. When the master's mise cannot be read, every mise
//!   pin is unresolved.
//! - Xcode: two or more segments are exact and planned. One segment resolves to
//!   the newest installed match on the master.
//! - Simulator runtimes: always exact and planned.
//!
//! A workspace that pins Xcode and no runtime also wants the default runtime:
//! iOS at that Xcode's version, from Xcode 26 on, where Xcode and its iOS SDK
//! share their version (Xcode 26.2 → iOS 26.2, 26.0.1 → 26.0). An older Xcode's
//! runtime is unresolved, with the hint to pin it in `.elsecall-tools.toml`.
//! A derived runtime is satisfied by an installed patch of it (iOS 26.0 by
//! 26.0.1), since Xcode's SDK names only major and minor.
use crate::{
    pins::{
        Environment, InstallOptions, Pin, Platform, SourceFile, Target,
        exactness::{Exactness, classify},
    },
    tool_inventory::{Manager, ToolName, ToolVersion, compare_versions, version_selector},
};
use elsecall_peers::roster::MemberName;
use std::cmp::Ordering;

/// A tool as inventory and `fleet-tools.toml` name it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ToolKey {
    pub(crate) manager: Manager,
    pub(crate) tool: ToolName,
}

impl ToolKey {
    pub(crate) fn of(target: &Target) -> Self {
        let (manager, tool) = match target {
            Target::Mise { tool, .. } => (Manager::Mise, tool.clone()),
            Target::Xcode => (Manager::Xcodes, xcode()),
            Target::SimulatorRuntime(platform) => (Manager::Simulators, runtime(*platform)),
        };
        Self { manager, tool }
    }
}

fn xcode() -> ToolName {
    ToolName::new("xcode").expect("xcode is a tool name")
}

fn runtime(platform: Platform) -> ToolName {
    ToolName::new(platform.as_str()).expect("platforms are tool names")
}

/// Why a version is wanted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reason {
    /// A pin, as written. `workspace` is `None` for the master's global config.
    Pin {
        member: MemberName,
        workspace: Option<String>,
        source: SourceFile,
        version: ToolVersion,
        environment: Option<Environment>,
    },
    /// The runtime matching an Xcode a workspace pins.
    DefaultRuntime {
        member: MemberName,
        workspace: String,
        xcode: ToolVersion,
    },
    /// A member's exception in `fleet-tools.toml`, by its key.
    Exception { key: String },
    /// The runtime matching the Xcode a member's exception keeps instead of
    /// the pinned one.
    ExceptionRuntime { key: String, xcode: ToolVersion },
}

/// Why a request is shown rather than planned. Each has its own hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Unresolved {
    /// `lts`, `system`, `ref:`, `path:`, `prefix:`, `sub-`.
    Selector,
    /// No version on the master matches, or an exact-looking mise version the
    /// master has not installed.
    NotOnMaster,
    /// The master's manager could not be read.
    MasterUnread,
    /// An Xcode before 26, whose runtime version is not its own.
    OlderXcode,
    /// Pins ask for one version with different install options.
    OptionsDiffer,
}

impl Reason {
    /// Whether this asks for a runtime derived from an Xcode version rather
    /// than one pinned as written.
    pub(crate) fn derived_runtime(&self) -> bool {
        matches!(
            self,
            Self::DefaultRuntime { .. } | Self::ExceptionRuntime { .. }
        )
    }
}

impl Unresolved {
    pub(crate) fn hint(self, master: &MemberName) -> String {
        match self {
            Self::Selector => {
                "Pin an exact version; Elsecall installs only exact versions.".into()
            }
            Self::NotOnMaster => format!(
                "Install the version on the master, {master}, so Elsecall can tell which build the pin means, or pin a full version it has."
            ),
            Self::MasterUnread => format!(
                "The master, {master}, could not report this manager's tools; run elsecall tools inventory there, then retry."
            ),
            Self::OptionsDiffer => "Pins ask for this version with different install options; pin it with the same options everywhere, as tools pins shows them.".into(),
            Self::OlderXcode => "Before Xcode 26 the runtime's version differs from Xcode's; pin the runtime in .elsecall-tools.toml, such as runtimes = [\"iOS 18.5\"].".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// `exact` is false when the version is what an inexact pin resolved to.
    Planned { version: ToolVersion, exact: bool },
    /// `written` is the pin as written; `None` for a derived runtime.
    Unresolved {
        written: Option<ToolVersion>,
        why: Unresolved,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Want {
    pub(crate) key: ToolKey,
    pub(crate) outcome: Outcome,
    pub(crate) reason: Reason,
    /// The install options of the pin behind it; empty for a derived runtime.
    pub(crate) options: InstallOptions,
}

/// What the master has installed, by manager; `None` when it could not be read.
#[derive(Debug, Clone)]
pub(crate) struct MasterTools {
    pub(crate) mise: Option<Vec<(ToolName, ToolVersion)>>,
    pub(crate) xcodes: Option<Vec<ToolVersion>>,
}

/// Where a set of pins was read: a member's workspace, or the master's global
/// config (`workspace` is `None`).
#[derive(Debug, Clone)]
pub(crate) struct Origin {
    pub(crate) member: MemberName,
    pub(crate) workspace: Option<String>,
}

/// The wants of one source's pins, with the default runtime when it applies.
pub(crate) fn wants(master: &MasterTools, origin: &Origin, pins: &[Pin]) -> Vec<Want> {
    let mut wants: Vec<Want> = pins
        .iter()
        .map(|pin| Want {
            key: ToolKey::of(&pin.target),
            outcome: judge(master, pin),
            reason: Reason::Pin {
                member: origin.member.clone(),
                workspace: origin.workspace.clone(),
                source: pin.source.clone(),
                version: pin.version.clone(),
                environment: pin.target.environment().cloned(),
            },
            options: pin.options.clone(),
        })
        .collect();
    let pins_runtime = pins
        .iter()
        .any(|pin| matches!(pin.target, Target::SimulatorRuntime(_)));
    if let Some(workspace) = &origin.workspace
        && !pins_runtime
    {
        let xcodes: Vec<ToolVersion> = wants
            .iter()
            .filter_map(|want| match (&want.key.manager, &want.outcome) {
                (Manager::Xcodes, Outcome::Planned { version, .. }) => Some(version.clone()),
                _ => None,
            })
            .collect();
        for xcode in xcodes {
            wants.push(Want {
                key: ToolKey {
                    manager: Manager::Simulators,
                    tool: runtime(Platform::Ios),
                },
                outcome: default_runtime(&xcode),
                reason: Reason::DefaultRuntime {
                    member: origin.member.clone(),
                    workspace: workspace.clone(),
                    xcode,
                },
                options: InstallOptions::default(),
            });
        }
    }
    wants
}

fn judge(master: &MasterTools, pin: &Pin) -> Outcome {
    let written = pin.version.clone();
    let unresolved = |why| Outcome::Unresolved {
        written: Some(written.clone()),
        why,
    };
    match &pin.target {
        Target::Mise { tool, .. } => {
            let Some(installed) = &master.mise else {
                return unresolved(Unresolved::MasterUnread);
            };
            let versions: Vec<ToolVersion> = installed
                .iter()
                .filter(|(name, _)| name == tool)
                .map(|(_, version)| version.clone())
                .collect();
            match classify(&pin.target, &pin.version, &versions) {
                Exactness::Exact if has(&versions, &pin.version) => Outcome::Planned {
                    version: pin.version.clone(),
                    exact: true,
                },
                Exactness::Exact => unresolved(Unresolved::NotOnMaster),
                Exactness::Inexact {
                    resolved: Some(version),
                } => Outcome::Planned {
                    version,
                    exact: false,
                },
                Exactness::Inexact { resolved: None }
                    if version_selector(pin.version.as_str())
                        && pin.version.as_str() != "latest" =>
                {
                    unresolved(Unresolved::Selector)
                }
                Exactness::Inexact { resolved: None } => unresolved(Unresolved::NotOnMaster),
            }
        }
        Target::Xcode => {
            let one_segment = !pin.version.as_str().contains('.');
            let installed = match (&master.xcodes, one_segment) {
                (Some(installed), _) => installed.clone(),
                (None, false) => Vec::new(),
                (None, true) => return unresolved(Unresolved::MasterUnread),
            };
            match classify(&pin.target, &pin.version, &installed) {
                Exactness::Exact => Outcome::Planned {
                    version: pin.version.clone(),
                    exact: true,
                },
                Exactness::Inexact {
                    resolved: Some(version),
                } => Outcome::Planned {
                    version,
                    exact: false,
                },
                Exactness::Inexact { resolved: None } => unresolved(Unresolved::NotOnMaster),
            }
        }
        Target::SimulatorRuntime(_) => Outcome::Planned {
            version: pin.version.clone(),
            exact: true,
        },
    }
}

fn has(versions: &[ToolVersion], version: &ToolVersion) -> bool {
    versions
        .iter()
        .any(|candidate| compare_versions(candidate.as_str(), version.as_str()) == Ordering::Equal)
}

/// iOS at Xcode's major and minor version, from Xcode 26 on.
pub(crate) fn default_runtime(xcode: &ToolVersion) -> Outcome {
    let mut segments = xcode.as_str().split('.');
    let major = segments.next().and_then(|major| major.parse::<u32>().ok());
    let minor = segments.next().unwrap_or("0");
    match major {
        Some(major) if major >= 26 => Outcome::Planned {
            version: ToolVersion::new(format!("{major}.{minor}"))
                .expect("digits and a dot are a version"),
            exact: true,
        },
        _ => Outcome::Unresolved {
            written: None,
            why: Unresolved::OlderXcode,
        },
    }
}

#[cfg(test)]
mod tests {
    use crate::{pins::SourceKind, plan::wants::*};

    fn master(mise: &[(&str, &str)], xcodes: &[&str]) -> MasterTools {
        MasterTools {
            mise: Some(
                mise.iter()
                    .map(|(name, version)| {
                        (
                            ToolName::new(*name).unwrap(),
                            ToolVersion::new(*version).unwrap(),
                        )
                    })
                    .collect(),
            ),
            xcodes: Some(
                xcodes
                    .iter()
                    .map(|version| ToolVersion::new(*version).unwrap())
                    .collect(),
            ),
        }
    }

    fn pin(target: Target, version: &str, kind: SourceKind, file: &str) -> Pin {
        Pin {
            target,
            version: ToolVersion::new(version).unwrap(),
            source: SourceFile {
                kind,
                file: file.into(),
            },
            options: crate::pins::InstallOptions::default(),
        }
    }

    fn mise(tool: &str, version: &str) -> Pin {
        pin(
            Target::Mise {
                tool: ToolName::new(tool).unwrap(),
                environment: None,
            },
            version,
            SourceKind::MiseProject,
            "mise.toml",
        )
    }

    fn xcode_pin(version: &str) -> Pin {
        pin(
            Target::Xcode,
            version,
            SourceKind::XcodeVersion,
            ".xcode-version",
        )
    }

    fn workspace() -> Origin {
        Origin {
            member: "mac-mini".parse().unwrap(),
            workspace: Some("~/Developer/acme-ios".into()),
        }
    }

    fn outcomes(master: &MasterTools, pins: &[Pin]) -> Vec<(String, Outcome)> {
        wants(master, &workspace(), pins)
            .into_iter()
            .map(|want| (want.key.tool.as_str().to_owned(), want.outcome))
            .collect()
    }

    fn planned(version: &str, exact: bool) -> Outcome {
        Outcome::Planned {
            version: ToolVersion::new(version).unwrap(),
            exact,
        }
    }

    fn unresolved(written: Option<&str>, why: Unresolved) -> Outcome {
        Outcome::Unresolved {
            written: written.map(|value| ToolVersion::new(value).unwrap()),
            why,
        }
    }

    #[test]
    fn mise_pins_are_judged_by_what_the_master_has() {
        let master = master(
            &[("node", "22.19.0"), ("bun", "1.3.5"), ("bun", "1.3.14")],
            &[],
        );
        let judged = outcomes(
            &master,
            &[
                mise("node", "22.19.0"),
                mise("node", "22"),
                mise("bun", "latest"),
                mise("python", "3.10.4"),
                mise("ruby", "latest"),
                mise("java", "lts"),
            ],
        );
        assert_eq!(
            judged,
            [
                ("node".into(), planned("22.19.0", true)),
                ("node".into(), planned("22.19.0", false)),
                ("bun".into(), planned("1.3.14", false)),
                (
                    "python".into(),
                    unresolved(Some("3.10.4"), Unresolved::NotOnMaster)
                ),
                (
                    "ruby".into(),
                    unresolved(Some("latest"), Unresolved::NotOnMaster)
                ),
                ("java".into(), unresolved(Some("lts"), Unresolved::Selector)),
            ]
        );
    }

    #[test]
    fn an_unreadable_master_resolves_no_mise_pin() {
        let mut master = master(&[], &["26.2"]);
        master.mise = None;
        let judged = outcomes(&master, &[mise("node", "22.19.0")]);
        assert_eq!(
            judged,
            [(
                "node".into(),
                unresolved(Some("22.19.0"), Unresolved::MasterUnread)
            )]
        );
    }

    #[test]
    fn xcode_brings_its_default_runtime() {
        let master = master(&[], &["16.4", "26.2"]);
        assert_eq!(
            outcomes(&master, &[xcode_pin("26.2")]),
            [
                ("xcode".into(), planned("26.2", true)),
                ("iOS".into(), planned("26.2", true)),
            ]
        );
        assert_eq!(
            outcomes(&master, &[xcode_pin("26")]),
            [
                ("xcode".into(), planned("26.2", false)),
                ("iOS".into(), planned("26.2", true)),
            ]
        );
        assert_eq!(
            outcomes(&master, &[xcode_pin("26.0.1")])[1],
            ("iOS".into(), planned("26.0", true))
        );
        assert_eq!(
            outcomes(&master, &[xcode_pin("16.4")])[1],
            ("iOS".into(), unresolved(None, Unresolved::OlderXcode))
        );
        assert_eq!(
            outcomes(&master, &[xcode_pin("27")]),
            [(
                "xcode".into(),
                unresolved(Some("27"), Unresolved::NotOnMaster)
            )]
        );
    }

    #[test]
    fn a_pinned_runtime_replaces_the_default_and_global_pins_bring_none() {
        let master = master(&[], &["26.2"]);
        let runtime = pin(
            Target::SimulatorRuntime(Platform::Ios),
            "26.1",
            SourceKind::ElsecallTools,
            ".elsecall-tools.toml",
        );
        assert_eq!(
            outcomes(&master, &[xcode_pin("26.2"), runtime]),
            [
                ("xcode".into(), planned("26.2", true)),
                ("iOS".into(), planned("26.1", true)),
            ]
        );
        let global = Origin {
            member: "mac-mini".parse().unwrap(),
            workspace: None,
        };
        assert_eq!(wants(&master, &global, &[xcode_pin("26.2")]).len(), 1);
    }

    #[test]
    fn a_reason_keeps_the_pin_as_written() {
        let master = master(&[("node", "22.19.0")], &[]);
        let want = wants(&master, &workspace(), &[mise("node", "22")]).remove(0);
        assert_eq!(
            want.reason,
            Reason::Pin {
                member: "mac-mini".parse().unwrap(),
                workspace: Some("~/Developer/acme-ios".into()),
                source: SourceFile {
                    kind: SourceKind::MiseProject,
                    file: "mise.toml".into(),
                },
                version: ToolVersion::new("22").unwrap(),
                environment: None,
            }
        );
    }
}
