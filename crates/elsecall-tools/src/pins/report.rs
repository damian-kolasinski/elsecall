//! Joining pin sources into one report for a workspace.
//!
//! Two workspace files that pin the same tool for the same mise environment
//! conflict unless both are mise project files asking for the same versions,
//! which is one pin. The master's global config is a layer under the project,
//! as mise treats it, so a global pin never conflicts with a project one: both
//! are reported. Each pin is then classified against the versions this machine
//! has installed. A tool manager that cannot be read is a warning, not a
//! failure: its plain versions count as exact and nothing is resolved.
use crate::{
    pins::{
        Environment, Pin, PinSource, SourceFile, SourceKind, Target,
        exactness::{Exactness, classify},
    },
    tool_inventory::{InventoryFailure, Manager, ToolManager, ToolVersion},
};
use elsecall_contract::failure::{Failure, Result};
use serde::{Serialize, ser::SerializeMap};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

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
    pins.sort_by(|left, right| {
        (&left.target, &left.environment, &left.source).cmp(&(
            &right.target,
            &right.environment,
            &right.source,
        ))
    });
    let mut warnings = Vec::new();
    let mise = installed(Manager::Mise, needs_mise(&pins), managers, &mut warnings);
    let xcode = installed(Manager::Xcodes, needs_xcode(&pins), managers, &mut warnings);
    let pins = pins
        .into_iter()
        .map(|pin| {
            let installed = match &pin.target {
                Target::Mise(name) => tool_versions(&mise, name.as_str()),
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
    pins.iter().any(|pin| matches!(pin.target, Target::Mise(_)))
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

/// Refuses two workspace files pinning one tool, keeping identical mise project
/// pins once, from the first file that declares them.
fn without_conflicts(pins: Vec<Pin>) -> Result<Vec<Pin>> {
    type Key = (Target, Option<Environment>);
    let mut groups: BTreeMap<Key, Vec<(SourceFile, Vec<ToolVersion>)>> = BTreeMap::new();
    for pin in pins
        .iter()
        .filter(|pin| pin.source.kind != SourceKind::MiseGlobal)
    {
        let files = groups
            .entry((pin.target.clone(), pin.environment.clone()))
            .or_default();
        match files.iter_mut().find(|(source, _)| *source == pin.source) {
            Some((_, versions)) => versions.push(pin.version.clone()),
            None => files.push((pin.source.clone(), vec![pin.version.clone()])),
        }
    }
    for ((target, environment), files) in &groups {
        let (first, first_versions) = &files[0];
        for (other, versions) in &files[1..] {
            let repeated = first.kind == SourceKind::MiseProject
                && other.kind == SourceKind::MiseProject
                && versions == first_versions;
            if !repeated {
                return Err(conflict(target, environment.as_ref(), first, other));
            }
        }
    }
    Ok(pins
        .into_iter()
        .filter(|pin| {
            pin.source.kind == SourceKind::MiseGlobal
                || groups[&(pin.target.clone(), pin.environment.clone())][0].0 == pin.source
        })
        .collect())
}

fn conflict(
    target: &Target,
    environment: Option<&Environment>,
    first: &SourceFile,
    second: &SourceFile,
) -> Failure {
    let environment = environment
        .map(|environment| format!(" for MISE_ENV={}", environment.as_str()))
        .unwrap_or_default();
    Failure::new(
        "pins_conflict",
        format!(
            "{} {} is pinned in both {} and {}{environment}.",
            target.manager(),
            target.tool(),
            first.file,
            second.file,
        ),
        "Pin each tool in one file of the workspace. Values are omitted from this error.",
    )
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
            &pin.environment
                .as_ref()
                .map(|environment| environment.as_str()),
        )?;
        map.serialize_entry(
            "source",
            &Source {
                kind: pin.source.kind.as_str(),
                file: pin.source.file.clone(),
            },
        )?;
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        pins::{Platform, Reading, report::*},
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
        Pin {
            target,
            version: ToolVersion::new(version).unwrap(),
            environment: environment.map(|value| Environment::new(value).unwrap()),
            source: SourceFile {
                kind,
                file: file.to_owned(),
            },
        }
    }

    fn mise(name: &str) -> Target {
        Target::Mise(ToolName::new(name).unwrap())
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

    #[test]
    fn identical_project_pins_are_one_and_different_ones_conflict() {
        let same = report(
            vec![
                project(".tool-versions", "node", "22.19.0"),
                project("mise.toml", "node", "22.19.0"),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(same["pins"].as_array().unwrap().len(), 1);
        assert_eq!(same["pins"][0]["source"]["file"], ".tool-versions");
        assert_eq!(same["sources"].as_array().unwrap().len(), 2);

        for second in [
            vec![project("mise.toml", "node", "24.0.1")],
            vec![
                project("mise.toml", "node", "22.19.0"),
                project("mise.toml", "node", "20.19.5"),
            ],
        ] {
            let mut pins = vec![project(".tool-versions", "node", "22.19.0")];
            pins.extend(second);
            let error = report(pins, &[]).unwrap_err();
            assert_eq!(error.code, "pins_conflict");
            assert_eq!(
                error.message,
                "mise node is pinned in both .tool-versions and mise.toml."
            );
            assert!(!error.message.contains("22.19.0"));
        }
    }

    #[test]
    fn environments_and_the_global_layer_do_not_conflict_with_the_project() {
        let value = report(
            vec![
                project("mise.toml", "node", "22.19.0"),
                pin(
                    SourceKind::MiseProject,
                    "mise.ci.toml",
                    mise("node"),
                    "24.0.1",
                    Some("ci"),
                ),
                pin(
                    SourceKind::MiseGlobal,
                    "~/.config/mise/config.toml",
                    mise("node"),
                    "20.19.5",
                    None,
                ),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(value["pins"].as_array().unwrap().len(), 3);

        let error = report(
            vec![
                pin(
                    SourceKind::MiseProject,
                    "mise.ci.toml",
                    mise("node"),
                    "24.0.1",
                    Some("ci"),
                ),
                pin(
                    SourceKind::MiseProject,
                    ".mise.ci.toml",
                    mise("node"),
                    "24.0.2",
                    Some("ci"),
                ),
            ],
            &[],
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            "mise node is pinned in both mise.ci.toml and .mise.ci.toml for MISE_ENV=ci."
        );
    }

    #[test]
    fn another_source_repeating_a_pin_conflicts_even_when_equal() {
        let xcode = |kind, file: &str| pin(kind, file, Target::Xcode, "26.2", None);
        let error = report(
            vec![
                xcode(SourceKind::XcodeVersion, ".xcode-version"),
                xcode(SourceKind::ElsecallTools, ".elsecall-tools.toml"),
            ],
            &[],
        )
        .unwrap_err();
        assert_eq!(error.code, "pins_conflict");
    }
}
