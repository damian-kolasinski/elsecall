//! What this machine has installed, read from its tool managers.
//!
//! A manager that cannot be read is reported on its own. The others are still
//! returned. Inventories are built with [`ManagerInventory::try_new`], which
//! sorts tools (name, then version with the newest last, then bundle path) and
//! refuses more than one default version of the same tool. A document from
//! another member is accepted only when it is mise, then xcodes, and every
//! Xcode path is absolute.
use serde::{Deserialize, Serialize, ser::SerializeMap};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Manager {
    #[serde(rename = "mise")]
    Mise,
    #[serde(rename = "xcodes")]
    Xcodes,
}

impl Manager {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mise => "mise",
            Self::Xcodes => "xcodes",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ToolName(String);

impl ToolName {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        plain_token(&value).then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ToolVersion(String);

impl ToolVersion {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        plain_token(&value).then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn plain_token(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| !c.is_whitespace() && !c.is_control())
}

/// The first whitespace-separated token of a tool manager's version line.
pub(crate) fn version_token(stdout: &[u8], command: &str) -> Result<ToolVersion, InventoryFailure> {
    let text = std::str::from_utf8(stdout)
        .map_err(|_| InventoryFailure::output_invalid(command, "the version was not text"))?;
    let token = text
        .split_whitespace()
        .next()
        .ok_or_else(|| InventoryFailure::output_invalid(command, "the version was empty"))?;
    ToolVersion::new(token).ok_or_else(|| {
        InventoryFailure::output_invalid(
            command,
            "the version contained whitespace or control characters",
        )
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledTool {
    pub name: ToolName,
    pub version: ToolVersion,
    pub default: bool,
    pub detail: Option<Detail>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detail {
    Xcode { build: String, path: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Executable {
    Missing,
    Found { path: PathBuf, version: ToolVersion },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerInventory {
    pub manager: Manager,
    pub executable: Executable,
    pub tools: Vec<InstalledTool>,
}

/// Orders versions with the newest last.
///
/// Segments split on `.` and `-`. A numeric segment compares as a number, and
/// any other segment compares as a string. When one side is numeric and the
/// other is not, the numeric one comes first. A version that matches another
/// and then has more segments is newer (`26.0` before `26.0.1`).
pub(crate) fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    let left: Vec<&str> = version_segments(left).collect();
    let right: Vec<&str> = version_segments(right).collect();
    for (left, right) in left.iter().zip(right.iter()) {
        let order = compare_segments(left, right);
        if order != std::cmp::Ordering::Equal {
            return order;
        }
    }
    left.len().cmp(&right.len())
}

fn version_segments(version: &str) -> impl Iterator<Item = &str> {
    version.split(['.', '-'])
}

fn compare_segments(left: &str, right: &str) -> std::cmp::Ordering {
    match (numeric_segment(left), numeric_segment(right)) {
        (true, true) => compare_numeric(left, right),
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        (false, false) => left.cmp(right),
    }
}

fn numeric_segment(segment: &str) -> bool {
    !segment.is_empty() && segment.bytes().all(|byte| byte.is_ascii_digit())
}

fn compare_numeric(left: &str, right: &str) -> std::cmp::Ordering {
    let left = trim_zeros(left);
    let right = trim_zeros(right);
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

fn trim_zeros(segment: &str) -> &str {
    let trimmed = segment.trim_start_matches('0');
    if trimmed.is_empty() { "0" } else { trimmed }
}

impl ManagerInventory {
    pub fn try_new(
        manager: Manager,
        executable: Executable,
        mut tools: Vec<InstalledTool>,
    ) -> Result<Self, InventoryFailure> {
        tools.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| compare_versions(left.version.as_str(), right.version.as_str()))
                .then_with(|| detail_path(left).cmp(detail_path(right)))
        });
        let mut defaults = BTreeSet::new();
        for tool in &tools {
            if tool.default && !defaults.insert(tool.name.as_str()) {
                let name = tool.name.as_str();
                return Err(InventoryFailure::output_invalid(
                    manager.as_str(),
                    &format!("more than one default version of {name}"),
                ));
            }
        }
        Ok(Self {
            manager,
            executable,
            tools,
        })
    }
}

fn detail_path(tool: &InstalledTool) -> &Path {
    match &tool.detail {
        Some(Detail::Xcode { path, .. }) => path,
        None => Path::new(""),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryFailure {
    Failed { message: String, hint: String },
    Timeout { message: String, hint: String },
    OutputInvalid { message: String, hint: String },
}

impl InventoryFailure {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Failed { .. } => "tool_manager_failed",
            Self::Timeout { .. } => "tool_manager_timeout",
            Self::OutputInvalid { .. } => "tool_manager_output_invalid",
        }
    }

    pub fn message(&self) -> &str {
        self.parts().0
    }

    pub fn hint(&self) -> &str {
        self.parts().1
    }

    fn parts(&self) -> (&str, &str) {
        match self {
            Self::Failed { message, hint }
            | Self::Timeout { message, hint }
            | Self::OutputInvalid { message, hint } => (message, hint),
        }
    }

    pub fn failed(detail: &str) -> Self {
        Self::Failed {
            message: format!("{detail}."),
            hint: "Run the tool manager in a terminal to see why, then retry elsecall tools inventory.".into(),
        }
    }

    pub fn not_absolute(variable: &str) -> Self {
        Self::Failed {
            message: format!("{variable} is not an absolute path."),
            hint: format!("{variable} must be an absolute path"),
        }
    }

    pub fn timeout(command: &str) -> Self {
        Self::Timeout {
            message: format!("{command} did not finish within 30 seconds."),
            hint:
                "Run it in a terminal to see why it is stuck, then retry elsecall tools inventory."
                    .into(),
        }
    }

    pub fn output_invalid(command: &str, reason: &str) -> Self {
        Self::OutputInvalid {
            message: format!("{command} returned output Elsecall could not read ({reason})."),
            hint: "Update the tool manager and retry elsecall tools inventory.".into(),
        }
    }
}

impl Serialize for InventoryFailure {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("code", self.code())?;
        map.serialize_entry("message", self.message())?;
        map.serialize_entry("hint", self.hint())?;
        map.end()
    }
}

pub trait ToolManager: Send + Sync {
    fn manager(&self) -> Manager;
    fn inventory(&self) -> Result<ManagerInventory, InventoryFailure>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Inventory {
    pub managers: Vec<ManagerReport>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerReport {
    Read(ManagerInventory),
    Failed {
        manager: Manager,
        failure: InventoryFailure,
    },
}

impl Serialize for ManagerReport {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Read(inventory) => {
                let mut map = serializer.serialize_map(Some(4))?;
                map.serialize_entry("manager", &inventory.manager)?;
                map.serialize_entry("state", "read")?;
                map.serialize_entry("executable", &inventory.executable)?;
                map.serialize_entry("tools", &inventory.tools)?;
                map.end()
            }
            Self::Failed { manager, failure } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("manager", manager)?;
                map.serialize_entry("state", "failed")?;
                map.serialize_entry("error", failure)?;
                map.end()
            }
        }
    }
}

impl Serialize for Executable {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Missing => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("state", "missing")?;
                map.end()
            }
            Self::Found { path, version } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("state", "found")?;
                map.serialize_entry("path", path)?;
                map.serialize_entry("version", version)?;
                map.end()
            }
        }
    }
}

impl Serialize for InstalledTool {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("name", &self.name)?;
        map.serialize_entry("version", &self.version)?;
        map.serialize_entry("default", &self.default)?;
        if let Some(Detail::Xcode { build, path }) = &self.detail {
            map.serialize_entry("build", build)?;
            map.serialize_entry("path", path)?;
        }
        map.end()
    }
}

/// Reads every manager. One failure becomes that manager's report and does not
/// drop the reports of the others.
pub fn gather(managers: &[&dyn ToolManager]) -> Inventory {
    Inventory {
        managers: managers
            .iter()
            .map(|manager| match manager.inventory() {
                Ok(inventory) => ManagerReport::Read(inventory),
                Err(failure) => ManagerReport::Failed {
                    manager: manager.manager(),
                    failure,
                },
            })
            .collect(),
    }
}

impl<'de> Deserialize<'de> for Inventory {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let document = incoming::Document::deserialize(deserializer)?;
        incoming::inventory(document).map_err(serde::de::Error::custom)
    }
}

/// An inventory document from another member. Unknown fields are ignored so a
/// later build can add them; a state or code this build does not write is refused.
mod incoming {
    use super::{
        Detail, Executable, InstalledTool, Inventory, InventoryFailure, Manager, ManagerInventory,
        ManagerReport, ToolName, ToolVersion,
    };
    use serde::Deserialize;
    use std::path::PathBuf;

    const MAX_TOOLS: usize = 4_096;
    const MAX_TEXT: usize = 4_096;
    const MAX_PATH: usize = 4_096;

    #[derive(Deserialize)]
    pub(super) struct Document {
        managers: Vec<RawManager>,
    }

    #[derive(Deserialize)]
    struct RawManager {
        manager: Manager,
        state: String,
        #[serde(default)]
        executable: Option<RawExecutable>,
        #[serde(default)]
        tools: Option<Vec<RawTool>>,
        #[serde(default)]
        error: Option<RawFailure>,
    }

    #[derive(Deserialize)]
    struct RawExecutable {
        state: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        version: Option<String>,
    }

    #[derive(Deserialize)]
    struct RawTool {
        name: String,
        version: String,
        default: bool,
        #[serde(default)]
        build: Option<String>,
        #[serde(default)]
        path: Option<String>,
    }

    #[derive(Deserialize)]
    struct RawFailure {
        code: String,
        message: String,
        hint: String,
    }

    pub(super) fn inventory(document: Document) -> Result<Inventory, &'static str> {
        let mut managers = Vec::with_capacity(document.managers.len());
        for manager in document.managers {
            managers.push(manager_report(manager)?);
        }
        match managers.as_slice() {
            [
                ManagerReport::Read(ManagerInventory {
                    manager: Manager::Mise,
                    ..
                })
                | ManagerReport::Failed {
                    manager: Manager::Mise,
                    ..
                },
                ManagerReport::Read(ManagerInventory {
                    manager: Manager::Xcodes,
                    ..
                })
                | ManagerReport::Failed {
                    manager: Manager::Xcodes,
                    ..
                },
            ] => Ok(Inventory { managers }),
            _ => Err("an inventory lists mise, then xcodes"),
        }
    }

    fn manager_report(raw: RawManager) -> Result<ManagerReport, &'static str> {
        match raw.state.as_str() {
            "read" => read_manager(raw),
            "failed" => failed_manager(raw),
            _ => Err("a manager state must be read or failed"),
        }
    }

    fn read_manager(raw: RawManager) -> Result<ManagerReport, &'static str> {
        if raw.error.is_some() {
            return Err("a read manager also reported an error");
        }
        let executable = executable(raw.executable.ok_or("a read manager has no executable")?)?;
        let tools = raw.tools.ok_or("a read manager has no tools")?;
        if tools.len() > MAX_TOOLS {
            return Err("a manager lists too many tools");
        }
        let mut installed = Vec::with_capacity(tools.len());
        for tool in tools {
            installed.push(installed_tool(tool)?);
        }
        let inventory = ManagerInventory::try_new(raw.manager, executable, installed)
            .map_err(|_| "a manager reported more than one default version of a tool")?;
        Ok(ManagerReport::Read(inventory))
    }

    fn failed_manager(raw: RawManager) -> Result<ManagerReport, &'static str> {
        if raw.executable.is_some() || raw.tools.is_some() {
            return Err("a failed manager also reported tools");
        }
        Ok(ManagerReport::Failed {
            manager: raw.manager,
            failure: manager_failure(raw.error.ok_or("a failed manager has no error")?)?,
        })
    }

    fn executable(raw: RawExecutable) -> Result<Executable, &'static str> {
        match raw.state.as_str() {
            "missing" if raw.path.is_none() && raw.version.is_none() => Ok(Executable::Missing),
            "found" => Ok(Executable::Found {
                path: tool_path(
                    raw.path
                        .as_deref()
                        .ok_or("a found executable has no path")?,
                )?,
                version: ToolVersion::new(
                    raw.version
                        .as_deref()
                        .ok_or("a found executable has no version")?,
                )
                .ok_or("a found executable has an unusable version")?,
            }),
            _ => Err("an executable state must be found or missing"),
        }
    }

    fn installed_tool(raw: RawTool) -> Result<InstalledTool, &'static str> {
        let name = ToolName::new(raw.name).ok_or("a tool name is empty or contains whitespace")?;
        let version = ToolVersion::new(raw.version)
            .ok_or("a tool version is empty or contains whitespace")?;
        let detail = match (raw.build, raw.path) {
            (None, None) => None,
            (Some(build), Some(path)) => {
                if ToolVersion::new(&build).is_none() {
                    return Err("an Xcode build is empty or contains whitespace");
                }
                Some(Detail::Xcode {
                    build,
                    path: absolute_path(&path)?,
                })
            }
            _ => return Err("an Xcode tool needs both a build and a path"),
        };
        Ok(InstalledTool {
            name,
            version,
            default: raw.default,
            detail,
        })
    }

    fn manager_failure(raw: RawFailure) -> Result<InventoryFailure, &'static str> {
        if !bounded_text(&raw.message) || !bounded_text(&raw.hint) {
            return Err("a manager error is empty, too long, or not text");
        }
        match raw.code.as_str() {
            "tool_manager_failed" => Ok(InventoryFailure::Failed {
                message: raw.message,
                hint: raw.hint,
            }),
            "tool_manager_timeout" => Ok(InventoryFailure::Timeout {
                message: raw.message,
                hint: raw.hint,
            }),
            "tool_manager_output_invalid" => Ok(InventoryFailure::OutputInvalid {
                message: raw.message,
                hint: raw.hint,
            }),
            _ => Err("a manager error code is not one Elsecall writes"),
        }
    }

    fn absolute_path(value: &str) -> Result<PathBuf, &'static str> {
        let path = tool_path(value)?;
        if path.is_absolute() {
            Ok(path)
        } else {
            Err("an Xcode path must be absolute")
        }
    }

    fn tool_path(value: &str) -> Result<PathBuf, &'static str> {
        if value.is_empty() || value.len() > MAX_PATH || value.chars().any(char::is_control) {
            return Err("a path is empty, too long, or not text");
        }
        Ok(PathBuf::from(value))
    }

    fn bounded_text(value: &str) -> bool {
        !value.is_empty()
            && value.chars().count() <= MAX_TEXT
            && !value.chars().any(char::is_control)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, version: &str, default: bool) -> InstalledTool {
        InstalledTool {
            name: ToolName::new(name).unwrap(),
            version: ToolVersion::new(version).unwrap(),
            default,
            detail: None,
        }
    }

    struct Fixed {
        manager: Manager,
        result: Result<ManagerInventory, InventoryFailure>,
    }

    impl ToolManager for Fixed {
        fn manager(&self) -> Manager {
            self.manager
        }
        fn inventory(&self) -> Result<ManagerInventory, InventoryFailure> {
            self.result.clone()
        }
    }

    #[test]
    fn names_and_versions_reject_empty_whitespace_and_control_characters() {
        assert!(ToolName::new("").is_none());
        assert!(ToolName::new("node js").is_none());
        assert!(ToolName::new("node\n").is_none());
        assert!(ToolName::new("a\u{0001}").is_none());
        assert!(ToolName::new("packslip:example/acme").is_some());
        assert!(ToolVersion::new("22.19.0").is_some());
        assert_eq!(
            version_token(b"2026.10.11 macos-arm64 (2026-10-15)\n", "mise --version")
                .unwrap()
                .as_str(),
            "2026.10.11"
        );
        let error = version_token(b"\xffSECRET", "mise --version").unwrap_err();
        assert_eq!(error.code(), "tool_manager_output_invalid");
        assert!(!error.message().contains("SECRET"));
        assert!(version_token(b" \n", "xcodes version").is_err());
    }

    #[test]
    fn versions_are_ordered_with_numeric_segments_and_the_newest_last() {
        assert_eq!(
            compare_versions("1.2.13", "1.3.5"),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_versions("1.3.5", "1.3.14"),
            std::cmp::Ordering::Less
        );
        assert_eq!(compare_versions("26.0", "26.0.1"), std::cmp::Ordering::Less);
        assert_eq!(
            compare_versions("26.0.1", "zulu-17.54.21"),
            std::cmp::Ordering::Less
        );
        let mut versions = [
            "1.3.14",
            "26.0.1",
            "1.2.13",
            "zulu-17.54.21",
            "26.0",
            "1.3.5",
        ];
        versions.sort_by(|left, right| compare_versions(left, right));
        assert_eq!(
            versions,
            [
                "1.2.13",
                "1.3.5",
                "1.3.14",
                "26.0",
                "26.0.1",
                "zulu-17.54.21",
            ]
        );
    }

    #[test]
    fn tools_are_sorted_and_a_tool_has_at_most_one_default() {
        let inventory = ManagerInventory::try_new(
            Manager::Mise,
            Executable::Missing,
            vec![
                tool("node", "22.19.0", true),
                tool("bun", "1.3.14", false),
                tool("bun", "1.3.5", false),
                tool("bun", "1.2.13", false),
            ],
        )
        .unwrap();
        let listed: Vec<_> = inventory
            .tools
            .iter()
            .map(|tool| (tool.name.as_str(), tool.version.as_str(), tool.default))
            .collect();
        assert_eq!(
            listed,
            vec![
                ("bun", "1.2.13", false),
                ("bun", "1.3.5", false),
                ("bun", "1.3.14", false),
                ("node", "22.19.0", true),
            ]
        );
        let error = ManagerInventory::try_new(
            Manager::Mise,
            Executable::Missing,
            vec![tool("node", "20.19.5", true), tool("node", "22.19.0", true)],
        )
        .unwrap_err();
        assert_eq!(error.code(), "tool_manager_output_invalid");
        assert!(
            ManagerInventory::try_new(
                Manager::Mise,
                Executable::Missing,
                vec![tool("node", "22.19.0", true), tool("rust", "1.99.0", true)],
            )
            .is_ok()
        );
    }

    #[test]
    fn the_same_xcode_version_is_ordered_by_bundle_path() {
        let inventory = ManagerInventory::try_new(
            Manager::Xcodes,
            Executable::Missing,
            vec![
                InstalledTool {
                    name: ToolName::new("xcode").unwrap(),
                    version: ToolVersion::new("16.4").unwrap(),
                    default: false,
                    detail: Some(Detail::Xcode {
                        build: "B".into(),
                        path: PathBuf::from("/Applications/Xcode-b.app"),
                    }),
                },
                InstalledTool {
                    name: ToolName::new("xcode").unwrap(),
                    version: ToolVersion::new("16.4").unwrap(),
                    default: false,
                    detail: Some(Detail::Xcode {
                        build: "A".into(),
                        path: PathBuf::from("/Applications/Xcode-a.app"),
                    }),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            detail_path(&inventory.tools[0]),
            Path::new("/Applications/Xcode-a.app")
        );
    }

    #[test]
    fn one_manager_failing_keeps_the_others() {
        let ok = Fixed {
            manager: Manager::Xcodes,
            result: Ok(ManagerInventory::try_new(
                Manager::Xcodes,
                Executable::Missing,
                vec![tool("xcode", "26.2", true)],
            )
            .unwrap()),
        };
        let bad = Fixed {
            manager: Manager::Mise,
            result: Err(InventoryFailure::timeout("mise ls --json")),
        };
        let inventory = gather(&[&bad, &ok]);
        assert_eq!(inventory.managers.len(), 2);
        assert!(matches!(
            &inventory.managers[0],
            ManagerReport::Failed {
                manager: Manager::Mise,
                failure,
            } if failure.code() == "tool_manager_timeout"
        ));
        match &inventory.managers[1] {
            ManagerReport::Read(read) => {
                assert_eq!(read.manager, Manager::Xcodes);
                assert_eq!(read.tools.len(), 1);
            }
            ManagerReport::Failed { .. } => panic!("xcodes should still be read"),
        }
    }

    #[test]
    fn json_uses_the_inventory_field_names() {
        let version = ToolVersion::new("2026.10.11").unwrap();
        let read = ManagerInventory::try_new(
            Manager::Mise,
            Executable::Found {
                path: PathBuf::from("/Users/x/.local/bin/mise"),
                version: version.clone(),
            },
            vec![tool("node", "22.19.0", true)],
        )
        .unwrap();
        let xcode = ManagerInventory::try_new(
            Manager::Xcodes,
            Executable::Missing,
            vec![InstalledTool {
                name: ToolName::new("xcode").unwrap(),
                version: ToolVersion::new("26.2").unwrap(),
                default: true,
                detail: Some(Detail::Xcode {
                    build: "17C52".into(),
                    path: PathBuf::from("/Applications/Xcode-26.2.0.app"),
                }),
            }],
        )
        .unwrap();
        let failed = InventoryFailure::timeout("mise ls --json");
        let inventory = Inventory {
            managers: vec![
                ManagerReport::Read(read),
                ManagerReport::Read(xcode),
                ManagerReport::Failed {
                    manager: Manager::Mise,
                    failure: failed.clone(),
                },
            ],
        };
        assert_eq!(
            serde_json::to_value(&inventory).unwrap(),
            serde_json::json!({
                "managers": [
                    {
                        "manager": "mise",
                        "state": "read",
                        "executable": {
                            "state": "found",
                            "path": "/Users/x/.local/bin/mise",
                            "version": "2026.10.11"
                        },
                        "tools": [{"name": "node", "version": "22.19.0", "default": true}]
                    },
                    {
                        "manager": "xcodes",
                        "state": "read",
                        "executable": {"state": "missing"},
                        "tools": [{
                            "name": "xcode",
                            "version": "26.2",
                            "default": true,
                            "build": "17C52",
                            "path": "/Applications/Xcode-26.2.0.app"
                        }]
                    },
                    {
                        "manager": "mise",
                        "state": "failed",
                        "error": {
                            "code": "tool_manager_timeout",
                            "message": failed.message(),
                            "hint": failed.hint()
                        }
                    }
                ]
            })
        );
        assert_eq!(
            serde_json::to_string(&Executable::Found {
                path: PathBuf::from("/Users/x/.local/bin/mise"),
                version,
            })
            .unwrap(),
            r#"{"state":"found","path":"/Users/x/.local/bin/mise","version":"2026.10.11"}"#
        );
        assert_eq!(
            serde_json::to_string(&Executable::Missing).unwrap(),
            r#"{"state":"missing"}"#
        );
        assert!(
            serde_json::from_value::<Inventory>(serde_json::to_value(&inventory).unwrap()).is_err()
        );
        let peer = Inventory {
            managers: inventory.managers[..2].to_vec(),
        };
        let parsed: Inventory =
            serde_json::from_value(serde_json::to_value(&peer).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::to_value(&peer).unwrap()
        );
    }

    #[test]
    fn a_peer_inventory_rejects_two_defaults_and_control_characters() {
        let two_defaults = serde_json::json!({
            "managers": [{
                "manager": "mise",
                "state": "read",
                "executable": {"state": "missing"},
                "tools": [
                    {"name": "node", "version": "20.0.0", "default": true},
                    {"name": "node", "version": "22.0.0", "default": true}
                ]
            }]
        });
        assert!(serde_json::from_value::<Inventory>(two_defaults).is_err());
        let control = serde_json::json!({
            "managers": [{
                "manager": "mise",
                "state": "failed",
                "error": {"code": "tool_manager_failed", "message": "line\nSECRET", "hint": "retry"}
            }]
        });
        assert!(serde_json::from_value::<Inventory>(control).is_err());
        let reversed = serde_json::json!({
            "managers": [
                {"manager": "xcodes", "state": "read", "executable": {"state": "missing"}, "tools": []},
                {"manager": "mise", "state": "read", "executable": {"state": "missing"}, "tools": []}
            ]
        });
        assert!(serde_json::from_value::<Inventory>(reversed).is_err());
        let only_mise = serde_json::json!({
            "managers": [
                {"manager": "mise", "state": "read", "executable": {"state": "missing"}, "tools": []}
            ]
        });
        assert!(serde_json::from_value::<Inventory>(only_mise).is_err());
        let relative = serde_json::json!({
            "managers": [
                {"manager": "mise", "state": "read", "executable": {"state": "missing"}, "tools": []},
                {
                    "manager": "xcodes",
                    "state": "read",
                    "executable": {"state": "missing"},
                    "tools": [{
                        "name": "xcode",
                        "version": "16.4",
                        "default": true,
                        "build": "16F6",
                        "path": "Xcode.app"
                    }]
                }
            ]
        });
        assert!(serde_json::from_value::<Inventory>(relative).is_err());
        let absolute = serde_json::json!({
            "managers": [
                {"manager": "mise", "state": "failed", "error": {"code": "tool_manager_failed", "message": "mise failed.", "hint": "retry"}},
                {
                    "manager": "xcodes",
                    "state": "read",
                    "executable": {"state": "missing"},
                    "tools": [{
                        "name": "xcode",
                        "version": "16.4",
                        "default": true,
                        "build": "16F6",
                        "path": "/Applications/Xcode.app"
                    }]
                }
            ]
        });
        assert!(serde_json::from_value::<Inventory>(absolute).is_ok());
    }
}
