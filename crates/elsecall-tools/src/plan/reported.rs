//! What a member reports about the workspaces its runs started in: each one's
//! name and the pins its own files declare, as data.
//!
//! This is the document `GET /peer/v1/tools/workspace-pins` returns. A
//! workspace is named home-relative on the member that holds it
//! (`~/Developer/acme-ios`), or by its absolute path when it is outside the
//! home folder, since members' home folders differ. Pins are read by the
//! member, from its own files, without running mise or any repository code;
//! they are judged later, on the master. Another member's document is
//! untrusted: it is validated on arrival, with each source's own grammar, and
//! its error text is not copied. A workspace that does not validate fails on
//! its own, as `protocol_error`; the member's other workspaces still count.
use crate::{
    pins::{
        Environment, InstallOptions, Pin, Platform, Reading, SourceFile, SourceKind, Target,
        dotted_version, literal_version,
    },
    tool_inventory::{ToolName, ToolVersion},
};
use elsecall_contract::{failure::Failure, scheduling_model::timestamp::Timestamp};
use serde::{Deserialize, Serialize, ser::SerializeMap};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

/// Workspaces a member reports, the most recently used first.
pub(crate) const MAXIMUM_WORKSPACES: usize = 64;
/// More than one workspace can list: 7 base files, 64 `conf.d` files, 6 × 64
/// environment files, `.tool-versions`, `.xcode-version`, `.elsecall-tools.toml`.
const MAXIMUM_SOURCES: usize = 512;
pub(crate) const MAXIMUM_PINS: usize = 4_096;
const MAXIMUM_TEXT: usize = 1_024;

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceReport {
    pub(crate) signed_pins: Option<BTreeSet<String>>,
    pub(crate) workspaces: Vec<ReportedWorkspace>,
}

#[derive(Debug, Clone)]
pub(crate) struct ReportedWorkspace {
    pub(crate) workspace: String,
    pub(crate) last_run_at: Timestamp,
    pub(crate) pins: std::result::Result<Reading, Failure>,
}

/// `root` as members name it: `~/…` under `home`, otherwise absolute.
pub(crate) fn workspace_name(root: &Path, home: &Path) -> String {
    let homes = [Some(home.to_owned()), std::fs::canonicalize(home).ok()];
    for home in homes.into_iter().flatten() {
        if let Ok(relative) = root.strip_prefix(&home)
            && !relative.as_os_str().is_empty()
        {
            return format!("~/{}", relative.display());
        }
    }
    root.display().to_string()
}

impl Serialize for WorkspaceReport {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        if let Some(pins) = &self.signed_pins {
            map.serialize_entry("signed_pins", pins)?;
        }
        map.serialize_entry("workspaces", &self.workspaces)?;
        map.end()
    }
}

impl Serialize for ReportedWorkspace {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("workspace", &self.workspace)?;
        map.serialize_entry("last_run_at", &self.last_run_at)?;
        match &self.pins {
            Ok(reading) => {
                map.serialize_entry("state", "read")?;
                map.serialize_entry(
                    "sources",
                    &reading.files.iter().map(WireSource::of).collect::<Vec<_>>(),
                )?;
                map.serialize_entry(
                    "pins",
                    &reading.pins.iter().map(WirePin::of).collect::<Vec<_>>(),
                )?;
            }
            Err(failure) => {
                map.serialize_entry("state", "failed")?;
                map.serialize_entry("error", failure)?;
            }
        }
        map.end()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSource {
    kind: String,
    file: String,
}

impl WireSource {
    fn of(source: &SourceFile) -> Self {
        Self {
            kind: source.kind.as_str().to_owned(),
            file: source.file.clone(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePin {
    manager: String,
    tool: String,
    version: String,
    environment: Option<String>,
    source: WireSource,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    options: BTreeMap<String, String>,
}

impl WirePin {
    fn of(pin: &Pin) -> Self {
        Self {
            manager: pin.target.manager().to_owned(),
            tool: pin.target.tool().to_owned(),
            version: pin.version.as_str().to_owned(),
            environment: pin.target.environment().map(|e| e.as_str().to_owned()),
            source: WireSource::of(&pin.source),
            options: pin.options.to_map(),
        }
    }
}

#[derive(Deserialize)]
struct WireReport {
    signed_pins: Option<BTreeSet<String>>,
    workspaces: Vec<WireWorkspace>,
}

#[derive(Deserialize)]
struct WireWorkspace {
    workspace: String,
    last_run_at: Timestamp,
    state: String,
    #[serde(default)]
    sources: Option<Vec<WireSource>>,
    #[serde(default)]
    pins: Option<Vec<WirePin>>,
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Deserialize)]
struct WireError {
    code: String,
}

impl<'de> Deserialize<'de> for WorkspaceReport {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let raw = WireReport::deserialize(deserializer)?;
        accept(raw).map_err(serde::de::Error::custom)
    }
}

fn accept(raw: WireReport) -> std::result::Result<WorkspaceReport, &'static str> {
    if raw.workspaces.len() > MAXIMUM_WORKSPACES {
        return Err("a member reports too many workspaces");
    }
    let mut workspaces = Vec::with_capacity(raw.workspaces.len());
    for workspace in raw.workspaces {
        workspaces.push(accept_workspace(workspace)?);
    }
    if raw.signed_pins.as_ref().is_some_and(|pins| {
        pins.len() > MAXIMUM_PINS
            || pins
                .iter()
                .any(|pin| pin.len() != 64 || !pin.bytes().all(|b| b.is_ascii_hexdigit()))
    }) {
        return Err("invalid signed pin fingerprints");
    }
    Ok(WorkspaceReport {
        workspaces,
        signed_pins: raw.signed_pins,
    })
}

fn accept_workspace(raw: WireWorkspace) -> std::result::Result<ReportedWorkspace, &'static str> {
    if !text(&raw.workspace) || !(raw.workspace.starts_with("~/") || raw.workspace.starts_with('/'))
    {
        return Err("a workspace name is not a home-relative or absolute path");
    }
    let workspace = raw.workspace;
    let pins = match (raw.state.as_str(), raw.sources, raw.pins, raw.error) {
        ("read", Some(sources), Some(pins), None) => {
            accept_reading(sources, pins).map_err(|rule| unreadable(&workspace, rule))
        }
        ("failed", None, None, Some(error)) => Err(failure_of(&workspace, &error.code)),
        _ => Err(unreadable(
            &workspace,
            "a workspace is either read, with sources and pins, or failed",
        )),
    };
    Ok(ReportedWorkspace {
        workspace,
        last_run_at: raw.last_run_at,
        pins,
    })
}

fn accept_reading(
    sources: Vec<WireSource>,
    pins: Vec<WirePin>,
) -> std::result::Result<Reading, &'static str> {
    if sources.len() > MAXIMUM_SOURCES || pins.len() > MAXIMUM_PINS {
        return Err("it lists too many sources or pins");
    }
    let files = sources
        .into_iter()
        .map(accept_source)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut accepted = Vec::with_capacity(pins.len());
    for pin in pins {
        let source = accept_source(pin.source)?;
        if !files.contains(&source) {
            return Err("a pin names a source the workspace did not list");
        }
        let target = target(&pin.manager, &pin.tool, pin.environment.as_deref())?;
        let version = version_of(&target, &source, &pin.version)?;
        if !pin.options.is_empty() && !matches!(target, Target::Mise { .. }) {
            return Err("only a mise pin carries install options");
        }
        let options = InstallOptions::from_wire(pin.options)?;
        accepted.push(Pin {
            target,
            version,
            source,
            options,
        });
    }
    Ok(Reading {
        files,
        pins: accepted,
    })
}

/// The grammar of the file the pin came from: Xcode is digits and dots, a
/// runtime at least two such segments, a mise version literal text. Nothing
/// that reads as an option.
fn version_of(
    target: &Target,
    source: &SourceFile,
    text: &str,
) -> std::result::Result<ToolVersion, &'static str> {
    let version = match target {
        Target::Xcode => dotted_version(text, 1),
        Target::SimulatorRuntime(_) => dotted_version(text, 2),
        Target::Mise { .. } => literal_version(&source.file, "version", text)
            .ok()
            .filter(|_| !text.starts_with('-')),
    };
    version.ok_or("a pinned version breaks its source's grammar")
}

/// A workspace whose report did not validate. The rule is written here.
fn unreadable(workspace: &str, rule: &str) -> Failure {
    Failure::new(
        "protocol_error",
        format!("{workspace}: the member's report of its pins could not be read ({rule})."),
        "Run the same Elsecall build on every member (elsecall fleet upgrade on the master), then retry.",
    )
}

/// Workspace files only: a member's global config is never read from it.
fn accept_source(raw: WireSource) -> std::result::Result<SourceFile, &'static str> {
    let kind = match raw.kind.as_str() {
        "mise_project" => SourceKind::MiseProject,
        "xcode_version" => SourceKind::XcodeVersion,
        "elsecall_tools" => SourceKind::ElsecallTools,
        _ => return Err("a source kind is not a workspace file"),
    };
    if !text(&raw.file) || raw.file.starts_with('/') || raw.file.starts_with('~') {
        return Err("a source file is not workspace-relative");
    }
    Ok(SourceFile {
        kind,
        file: raw.file,
    })
}

fn target(
    manager: &str,
    tool: &str,
    environment: Option<&str>,
) -> std::result::Result<Target, &'static str> {
    match (manager, environment) {
        ("mise", environment) => Ok(Target::Mise {
            tool: ToolName::new(tool)
                .filter(|_| text(tool) && !tool.starts_with('-') && !tool.contains(['[', ']']))
                .ok_or("a mise tool name is not a name")?,
            environment: environment
                .map(|name| Environment::new(name).ok_or("an environment is not a name"))
                .transpose()?,
        }),
        ("xcodes", None) if tool == "xcode" => Ok(Target::Xcode),
        ("simulators", None) => Platform::ALL
            .into_iter()
            .find(|platform| platform.as_str() == tool)
            .map(Target::SimulatorRuntime)
            .ok_or("a runtime platform is not one simctl names"),
        _ => Err("a pin's manager, tool or environment is not one Elsecall reads"),
    }
}

/// A peer's failure, with text written here naming the workspace.
fn failure_of(workspace: &str, code: &str) -> Failure {
    let rule = match code {
        "pin_source_unreadable" => "a file could not be read as a pin source",
        "pin_source_invalid" => "a file breaks a pin rule",
        "pins_conflict" => "two files pin one tool",
        _ => return unreadable(workspace, "its error is not one pins are read with"),
    };
    Failure::new(
        code,
        format!("{workspace}: {rule}."),
        format!(
            "Run elsecall tools pins {workspace} on the member that holds it to see which file and key."
        ),
    )
}

fn text(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAXIMUM_TEXT && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use crate::plan::reported::*;
    use serde_json::json;

    fn version(value: &str) -> ToolVersion {
        ToolVersion::new(value).unwrap()
    }

    fn report() -> WorkspaceReport {
        let project = SourceFile {
            kind: SourceKind::MiseProject,
            file: "mise.signing.toml".into(),
        };
        WorkspaceReport {
            signed_pins: None,
            workspaces: vec![
                ReportedWorkspace {
                    workspace: "~/Developer/acme-ios".into(),
                    last_run_at: Timestamp::from_millis(1_700_000_000_000),
                    pins: Ok(Reading {
                        files: vec![project.clone()],
                        pins: vec![Pin {
                            target: Target::Mise {
                                tool: ToolName::new("crem").unwrap(),
                                environment: Environment::new("signing"),
                            },
                            version: version("0.2.0"),
                            source: project,
                            options: InstallOptions::from_wire(BTreeMap::from([(
                                "version_prefix".to_owned(),
                                "v".to_owned(),
                            )]))
                            .unwrap(),
                        }],
                    }),
                },
                ReportedWorkspace {
                    workspace: "/Volumes/Work/acme".into(),
                    last_run_at: Timestamp::from_millis(1_600_000_000_000),
                    pins: Err(Failure::new("pins_conflict", "mise.toml and SECRET", "fix")),
                },
            ],
        }
    }

    #[test]
    fn a_report_round_trips_and_peer_text_is_rewritten() {
        let value = serde_json::to_value(report()).unwrap();
        assert_eq!(value["workspaces"][0]["state"], "read");
        assert_eq!(
            value["workspaces"][0]["pins"][0],
            json!({"manager": "mise", "tool": "crem", "version": "0.2.0", "environment": "signing",
                   "source": {"kind": "mise_project", "file": "mise.signing.toml"},
                   "options": {"version_prefix": "v"}})
        );
        let parsed: WorkspaceReport = serde_json::from_value(value).unwrap();
        let first = &parsed.workspaces[0];
        assert_eq!(first.workspace, "~/Developer/acme-ios");
        assert_eq!(first.last_run_at, report().workspaces[0].last_run_at);
        assert_eq!(
            first.pins.as_ref().unwrap(),
            report().workspaces[0].pins.as_ref().unwrap()
        );
        let failure = parsed.workspaces[1].pins.as_ref().unwrap_err();
        assert_eq!(failure.code, "pins_conflict");
        assert!(!failure.message.contains("SECRET"), "{}", failure.message);
        assert!(failure.message.contains("/Volumes/Work/acme"));
    }

    #[test]
    fn a_workspace_that_does_not_validate_fails_alone() {
        let base = serde_json::to_value(report()).unwrap();
        let first_fails = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut value = base.clone();
            edit(&mut value);
            let parsed: WorkspaceReport = serde_json::from_value(value).unwrap();
            assert_eq!(parsed.workspaces.len(), 2);
            match &parsed.workspaces[0].pins {
                Err(failure) => failure.code.clone(),
                Ok(_) => "read".into(),
            }
        };
        for edit in [
            &(|v: &mut serde_json::Value| {
                v["workspaces"][0]["pins"][0]["version"] = json!("{{ env.X }}")
            }) as &dyn Fn(&mut serde_json::Value),
            &|v| v["workspaces"][0]["pins"][0]["version"] = json!("--force"),
            &|v| v["workspaces"][0]["pins"][0]["tool"] = json!("-x"),
            &|v| v["workspaces"][0]["pins"][0]["tool"] = json!("crem[postinstall=x]"),
            &|v| v["workspaces"][0]["pins"][0]["source"]["kind"] = json!("mise_global"),
            &|v| v["workspaces"][0]["sources"][0]["file"] = json!("/etc/passwd"),
            &|v| v["workspaces"][0]["pins"][0]["source"]["file"] = json!("mise.toml"),
            &|v| v["workspaces"][0]["pins"][0]["manager"] = json!("xcodes"),
            &|v| v["workspaces"][0]["pins"][0]["environment"] = json!("local"),
            &|v| v["workspaces"][0]["state"] = json!("failed"),
            &|v| v["workspaces"][0]["pins"][0]["options"]["postinstall"] = json!("touch /tmp/x"),
            &|v| v["workspaces"][0]["pins"][0]["options"]["bin"] = json!("{{ env.X }}"),
        ] {
            assert_eq!(first_fails(edit), "protocol_error");
        }
        let xcode = |version: &str| {
            let mut value = base.clone();
            value["workspaces"][0]["sources"][0] =
                json!({"kind": "xcode_version", "file": ".xcode-version"});
            value["workspaces"][0]["pins"][0] = json!({"manager": "xcodes", "tool": "xcode",
                "version": version, "environment": null,
                "source": {"kind": "xcode_version", "file": ".xcode-version"}});
            let parsed: WorkspaceReport = serde_json::from_value(value).unwrap();
            parsed.workspaces[0].pins.is_ok()
        };
        assert!(xcode("26.2") && xcode("26"));
        let mut optioned_xcode = base.clone();
        optioned_xcode["workspaces"][0]["sources"][0] =
            json!({"kind": "xcode_version", "file": ".xcode-version"});
        optioned_xcode["workspaces"][0]["pins"][0] = json!({"manager": "xcodes", "tool": "xcode",
            "version": "26.2", "environment": null, "options": {"bin": "x"},
            "source": {"kind": "xcode_version", "file": ".xcode-version"}});
        let parsed: WorkspaceReport = serde_json::from_value(optioned_xcode).unwrap();
        assert_eq!(
            parsed.workspaces[0].pins.as_ref().unwrap_err().code,
            "protocol_error"
        );
        assert!(!xcode("latest") && !xcode("26.2-beta"));
        let mut odd = base.clone();
        odd["workspaces"][1]["error"]["code"] = json!("internal_error");
        let parsed: WorkspaceReport = serde_json::from_value(odd).unwrap();
        assert_eq!(
            parsed.workspaces[1].pins.as_ref().unwrap_err().code,
            "protocol_error"
        );
    }

    #[test]
    fn a_document_with_an_unusable_name_or_too_many_workspaces_is_refused() {
        let base = serde_json::to_value(report()).unwrap();
        let mut relative = base.clone();
        relative["workspaces"][0]["workspace"] = json!("relative/path");
        assert!(serde_json::from_value::<WorkspaceReport>(relative).is_err());
        let mut many = base.clone();
        let first = many["workspaces"][0].clone();
        many["workspaces"] = json!(vec![first; MAXIMUM_WORKSPACES + 1]);
        assert!(serde_json::from_value::<WorkspaceReport>(many).is_err());
    }

    #[test]
    fn workspaces_are_named_home_relative() {
        let home = Path::new("/Users/me");
        assert_eq!(
            workspace_name(Path::new("/Users/me/Developer/acme-ios"), home),
            "~/Developer/acme-ios"
        );
        assert_eq!(
            workspace_name(Path::new("/Volumes/Work/acme"), home),
            "/Volumes/Work/acme"
        );
        assert_eq!(
            workspace_name(Path::new("/Users/devbox/acme"), home),
            "/Users/devbox/acme"
        );
    }
}
