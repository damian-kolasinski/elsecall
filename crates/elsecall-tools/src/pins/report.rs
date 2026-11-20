//! Joining pin sources into one report for a workspace.
//!
//! The sources' pins are checked for conflicts (`conflicts`), ordered by
//! manager, tool, environment and source, and classified against the versions
//! this machine has installed (`exactness`). A tool manager that cannot be read
//! is a warning, not a failure: its plain versions count as exact and nothing
//! is resolved.
use crate::{
    pins::{
        Pin, PinSource, Target,
        conflicts::without_conflicts,
        exactness::{Exactness, classify},
    },
    tool_inventory::{InventoryFailure, Manager, ToolManager, ToolVersion},
};
use elsecall_contract::failure::Result;
use serde::{Serialize, ser::SerializeMap};
use std::path::{Path, PathBuf};

/// Everything a workspace pins, as `elsecall tools pins` prints it.
#[derive(Debug, Serialize)]
pub(crate) struct PinReport {
    workspace: PathBuf,
    sources: Vec<Source>,
    pins: Vec<ReportedPin>,
    warnings: Vec<Warning>,
}

#[derive(Debug, Serialize)]
struct Source {
    kind: &'static str,
    file: String,
}

#[derive(Debug)]
struct ReportedPin {
    pin: Pin,
    exactness: Exactness,
}

/// A tool manager this machine could not read, so its pins were not resolved.
#[derive(Debug, Serialize)]
struct Warning {
    manager: Manager,
    error: InventoryFailure,
}

/// Reads every source for `workspace`, a canonical directory, and classifies the
/// pins with what `managers` report as installed here.
pub(crate) fn read(
    workspace: &Path,
    sources: &[&dyn PinSource],
    managers: &[&dyn ToolManager],
) -> Result<PinReport> {
    let mut files = Vec::new();
    let mut pins = Vec::new();
    for source in sources {
        let reading = source.read(workspace)?;
        files.extend(reading.files);
        pins.extend(reading.pins);
    }
    let mut pins = without_conflicts(pins)?;
    pins.sort_by(|left, right| (&left.target, &left.source).cmp(&(&right.target, &right.source)));
    let mut warnings = Vec::new();
    let mise = installed(Manager::Mise, needs_mise(&pins), managers, &mut warnings);
    let xcode = installed(Manager::Xcodes, needs_xcode(&pins), managers, &mut warnings);
    let pins = pins
        .into_iter()
        .map(|pin| {
            let installed = match &pin.target {
                Target::Mise { tool, .. } => tool_versions(&mise, tool.as_str()),
                Target::Xcode => tool_versions(&xcode, "xcode"),
                Target::SimulatorRuntime(_) => Vec::new(),
            };
            let exactness = classify(&pin.target, &pin.version, &installed);
            ReportedPin { pin, exactness }
        })
        .collect();
    Ok(PinReport {
        workspace: workspace.to_owned(),
        sources: files
            .into_iter()
            .map(|file| Source {
                kind: file.kind.as_str(),
                file: file.file,
            })
            .collect(),
        pins,
        warnings,
    })
}

fn needs_mise(pins: &[Pin]) -> bool {
    pins.iter()
        .any(|pin| matches!(pin.target, Target::Mise { .. }))
}

/// Only a one-segment Xcode version needs the installed Xcodes to resolve.
fn needs_xcode(pins: &[Pin]) -> bool {
    pins.iter()
        .any(|pin| pin.target == Target::Xcode && !pin.version.as_str().contains('.'))
}

/// `(tool, version)` for every installed version `manager` reports, when needed.
fn installed(
    manager: Manager,
    needed: bool,
    managers: &[&dyn ToolManager],
    warnings: &mut Vec<Warning>,
) -> Vec<(String, ToolVersion)> {
    if !needed {
        return Vec::new();
    }
    let Some(reader) = managers.iter().find(|reader| reader.manager() == manager) else {
        return Vec::new();
    };
    match reader.inventory() {
        Ok(inventory) => inventory
            .tools
            .into_iter()
            .map(|tool| (tool.name.as_str().to_owned(), tool.version))
            .collect(),
        Err(error) => {
            warnings.push(Warning { manager, error });
            Vec::new()
        }
    }
}

fn tool_versions(installed: &[(String, ToolVersion)], tool: &str) -> Vec<ToolVersion> {
    installed
        .iter()
        .filter(|(name, _)| name == tool)
        .map(|(_, version)| version.clone())
        .collect()
}

impl Serialize for ReportedPin {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let pin = &self.pin;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("manager", pin.target.manager())?;
        map.serialize_entry("tool", pin.target.tool())?;
        map.serialize_entry("version", &pin.version)?;
        map.serialize_entry("exact", &matches!(self.exactness, Exactness::Exact))?;
        if let Exactness::Inexact { resolved } = &self.exactness {
            map.serialize_entry("resolved", resolved)?;
            map.serialize_entry(
                "hint",
                match resolved {
                    Some(_) => "Pin the resolved version, or another exact one, so every member installs the same build.",
                    None => "Pin an exact version; Elsecall installs only exact versions.",
                },
            )?;
        }
        map.serialize_entry(
            "environment",
            &pin.target
                .environment()
                .map(|environment| environment.as_str()),
        )?;
        map.serialize_entry(
            "source",
            &Source {
                kind: pin.source.kind.as_str(),
                file: pin.source.file.clone(),
            },
        )?;
        if !pin.options.is_empty() {
            map.serialize_entry("options", &pin.options)?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        pins::{Environment, Platform, Reading, SourceFile, SourceKind, report::*},
        tool_inventory::{Executable, InstalledTool, ManagerInventory, ToolName},
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixed(Vec<Pin>);

    impl PinSource for Fixed {
        fn read(&self, _workspace: &Path) -> Result<Reading> {
            let mut files: Vec<SourceFile> = Vec::new();
            for pin in &self.0 {
                if !files.contains(&pin.source) {
                    files.push(pin.source.clone());
                }
            }
            Ok(Reading {
                files,
                pins: self.0.clone(),
            })
        }
    }

    struct Installed {
        manager: Manager,
        tools: Vec<(&'static str, &'static str)>,
        fails: bool,
        reads: AtomicUsize,
    }

    impl Installed {
        fn new(manager: Manager, tools: Vec<(&'static str, &'static str)>) -> Self {
            Self {
                manager,
                tools,
                fails: false,
                reads: AtomicUsize::new(0),
            }
        }
    }

    impl ToolManager for Installed {
        fn manager(&self) -> Manager {
            self.manager
        }

        fn inventory(&self) -> std::result::Result<ManagerInventory, InventoryFailure> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            if self.fails {
                return Err(InventoryFailure::timeout("mise ls --json"));
            }
            ManagerInventory::try_new(
                self.manager,
                Executable::Missing,
                self.tools
                    .iter()
                    .map(|(name, version)| InstalledTool {
                        name: ToolName::new(*name).unwrap(),
                        version: ToolVersion::new(*version).unwrap(),
                        default: false,
                        detail: None,
                    })
                    .collect(),
            )
        }
    }

    fn pin(
        kind: SourceKind,
        file: &str,
        target: Target,
        version: &str,
        environment: Option<&str>,
    ) -> Pin {
        let target = match target {
            Target::Mise { tool, .. } => Target::Mise {
                tool,
                environment: environment.map(|value| Environment::new(value).unwrap()),
            },
            other => other,
        };
        Pin {
            target,
            version: ToolVersion::new(version).unwrap(),
            source: SourceFile {
                kind,
                file: file.to_owned(),
            },
            options: crate::pins::InstallOptions::default(),
        }
    }

    fn mise(name: &str) -> Target {
        Target::Mise {
            tool: ToolName::new(name).unwrap(),
            environment: None,
        }
    }

    fn project(file: &str, tool: &str, version: &str) -> Pin {
        pin(SourceKind::MiseProject, file, mise(tool), version, None)
    }

    fn report(pins: Vec<Pin>, managers: &[&dyn ToolManager]) -> Result<serde_json::Value> {
        let source = Fixed(pins);
        read(Path::new("/Users/me/acme-ios"), &[&source], managers)
            .map(|report| serde_json::to_value(report).unwrap())
    }

    #[test]
    fn pins_are_classified_ordered_and_printed() {
        let mise_tools = Installed::new(
            Manager::Mise,
            vec![
                ("bun", "1.3.5"),
                ("bun", "1.3.14"),
                ("node", "22.19.0"),
                ("python", "3.10.4"),
            ],
        );
        let xcodes = Installed::new(Manager::Xcodes, vec![("xcode", "26.2")]);
        let value = report(
            vec![
                project("mise.toml", "python", "3.10.4"),
                project("mise.toml", "bun", "latest"),
                pin(
                    SourceKind::MiseProject,
                    "mise.signing.toml",
                    mise("crem"),
                    "0.2.0",
                    Some("signing"),
                ),
                pin(
                    SourceKind::MiseGlobal,
                    "~/.config/mise/config.toml",
                    mise("node"),
                    "22",
                    None,
                ),
                pin(
                    SourceKind::MiseGlobal,
                    "~/.config/mise/config.toml",
                    mise("java"),
                    "lts",
                    None,
                ),
                pin(
                    SourceKind::XcodeVersion,
                    ".xcode-version",
                    Target::Xcode,
                    "26.2",
                    None,
                ),
                pin(
                    SourceKind::ElsecallTools,
                    ".elsecall-tools.toml",
                    Target::SimulatorRuntime(Platform::Ios),
                    "26.2",
                    None,
                ),
            ],
            &[&mise_tools, &xcodes],
        )
        .unwrap();
        let source = |kind: &str, file: &str| json!({"kind": kind, "file": file});
        let global = source("mise_global", "~/.config/mise/config.toml");
        assert_eq!(
            value,
            json!({
                "workspace": "/Users/me/acme-ios",
                "sources": [
                    source("mise_project", "mise.toml"),
                    source("mise_project", "mise.signing.toml"),
                    global,
                    source("xcode_version", ".xcode-version"),
                    source("elsecall_tools", ".elsecall-tools.toml"),
                ],
                "pins": [
                    {"manager": "mise", "tool": "bun", "version": "latest", "exact": false,
                     "resolved": "1.3.14",
                     "hint": "Pin the resolved version, or another exact one, so every member installs the same build.",
                     "environment": null, "source": source("mise_project", "mise.toml")},
                    {"manager": "mise", "tool": "crem", "version": "0.2.0", "exact": true,
                     "environment": "signing", "source": source("mise_project", "mise.signing.toml")},
                    {"manager": "mise", "tool": "java", "version": "lts", "exact": false,
                     "resolved": null,
                     "hint": "Pin an exact version; Elsecall installs only exact versions.",
                     "environment": null, "source": global},
                    {"manager": "mise", "tool": "node", "version": "22", "exact": false,
                     "resolved": "22.19.0",
                     "hint": "Pin the resolved version, or another exact one, so every member installs the same build.",
                     "environment": null, "source": global},
                    {"manager": "mise", "tool": "python", "version": "3.10.4", "exact": true,
                     "environment": null, "source": source("mise_project", "mise.toml")},
                    {"manager": "xcodes", "tool": "xcode", "version": "26.2", "exact": true,
                     "environment": null, "source": source("xcode_version", ".xcode-version")},
                    {"manager": "simulators", "tool": "iOS", "version": "26.2", "exact": true,
                     "environment": null, "source": source("elsecall_tools", ".elsecall-tools.toml")},
                ],
                "warnings": [],
            })
        );
        assert_eq!(mise_tools.reads.load(Ordering::Relaxed), 1);
        assert_eq!(
            xcodes.reads.load(Ordering::Relaxed),
            0,
            "an exact Xcode pin needs no inventory"
        );
    }

    #[test]
    fn a_manager_that_cannot_be_read_is_a_warning() {
        let mut broken = Installed::new(Manager::Mise, Vec::new());
        broken.fails = true;
        let value = report(vec![project("mise.toml", "node", "22")], &[&broken]).unwrap();
        assert_eq!(value["pins"][0]["exact"], true);
        assert_eq!(value["warnings"][0]["manager"], "mise");
        assert_eq!(
            value["warnings"][0]["error"]["code"],
            "tool_manager_timeout"
        );
    }

    #[test]
    fn nothing_is_read_when_nothing_is_pinned() {
        let mise_tools = Installed::new(Manager::Mise, Vec::new());
        let value = report(Vec::new(), &[&mise_tools]).unwrap();
        assert_eq!(value["pins"], json!([]));
        assert_eq!(mise_tools.reads.load(Ordering::Relaxed), 0);
    }
}
