//! `fleet-tools.toml`: exceptions to the pins, for every member or one.
//!
//! Pins say what members need (`docs/fleet-provisioning.md`); this file holds
//! only the exceptions. A tool entry under `[defaults]` must be empty: it leaves
//! that tool out on every member. A tool entry under `[members.<name>]` is
//! either empty, leaving the tool out on that member, or names `default` and
//! `install` versions that member keeps instead of what the pins ask for. A
//! member's entry replaces the defaults' entry for that tool.
//!
//! The file used to be the fleet's desired state (fleet-tools Slice 2b-1). A
//! document in that shape, with versions under `[defaults]` or a manager's
//! executable `version`, is refused with a rule naming the key, never read with
//! a different meaning. Parsing validates untrusted bytes, and errors never
//! echo values.
use crate::tool_inventory::{Manager, ToolName, ToolVersion, version_selector};
use elsecall_contract::failure::{Failure, Result};
use elsecall_peers::roster::MemberName;
use serde::{Serialize, ser::SerializeMap};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const LIMIT: usize = 64 * 1024;
const MAX_MEMBERS: usize = 64;
const PLATFORMS: [&str; 4] = ["iOS", "tvOS", "visionOS", "watchOS"];

pub(crate) fn invalid(path: &str, rule: &str) -> Failure {
    Failure::new(
        "invalid_fleet_tools",
        format!("{path}: {rule}"),
        "Fix the named key in fleet-tools.toml, which holds only exceptions to the pins (docs/fleet-tools.md). Versions must be exact, for example 22.19.0. Values are omitted from this error.",
    )
}

pub(crate) fn version_rule(tool: &str) -> String {
    format!("versions must be exact, e.g. 22.19.0; run `mise ls-remote {tool}` to pick one")
}

/// An exact version. Symbolic requests and mise selectors are rejected.
pub(crate) fn exact_version(path: &str, tool: &str, value: &str) -> Result<ToolVersion> {
    match ToolVersion::new(value) {
        Some(version) if !version_selector(value) => Ok(version),
        _ => Err(invalid(path, &version_rule(tool))),
    }
}

pub(crate) fn require_tool(path: &str, manager: Manager, name: &str) -> Result<ToolName> {
    match manager {
        Manager::Xcodes if name != "xcode" => {
            return Err(invalid(path, "xcodes manages only the tool xcode"));
        }
        Manager::Simulators if !PLATFORMS.contains(&name) => {
            return Err(invalid(
                path,
                "simulators tools are platforms: iOS, tvOS, visionOS or watchOS",
            ));
        }
        _ => {}
    }
    ToolName::new(name).ok_or_else(|| {
        invalid(
            path,
            "tool names cannot be empty or contain whitespace or control characters",
        )
    })
}

/// The manager a table is named after.
pub(crate) fn manager_named(name: &str) -> Option<Manager> {
    match name {
        "mise" => Some(Manager::Mise),
        "xcodes" => Some(Manager::Xcodes),
        "simulators" => Some(Manager::Simulators),
        _ => None,
    }
}

/// The key of a tool entry, as the file spells it.
pub(crate) fn entry_key(member: Option<&MemberName>, manager: Manager, tool: &ToolName) -> String {
    let scope = match member {
        Some(name) => format!("members.{}", name.as_str()),
        None => "defaults".to_owned(),
    };
    let tool = tool.as_str();
    let bare = tool
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if bare {
        format!("{scope}.{}.tools.{tool}", manager.as_str())
    } else {
        format!("{scope}.{}.tools.{tool:?}", manager.as_str())
    }
}

/// The versions a member keeps instead of what the pins ask for. `install` never
/// contains `default`, and at least one version is named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Versions {
    default: Option<ToolVersion>,
    install: BTreeSet<ToolVersion>,
}

impl Versions {
    fn new(
        default: Option<ToolVersion>,
        install: impl IntoIterator<Item = ToolVersion>,
    ) -> Option<Self> {
        let mut install: BTreeSet<_> = install.into_iter().collect();
        if let Some(default) = &default {
            install.remove(default);
        }
        if default.is_none() && install.is_empty() {
            None
        } else {
            Some(Self { default, install })
        }
    }

    pub(crate) fn default(&self) -> Option<&ToolVersion> {
        self.default.as_ref()
    }

    pub(crate) fn install(&self) -> impl Iterator<Item = &ToolVersion> {
        self.install.iter()
    }

    /// Every version, the default first.
    pub(crate) fn versions(&self) -> impl Iterator<Item = &ToolVersion> {
        self.default.iter().chain(self.install.iter())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Exception {
    /// Nothing the pins ask for this tool is planned.
    LeaveOut,
    /// These versions are planned instead of the pinned ones.
    Versions(Versions),
}

/// One exception as it applies to a member, with the key that sets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemberException {
    pub(crate) manager: Manager,
    pub(crate) tool: ToolName,
    pub(crate) key: String,
    pub(crate) exception: Exception,
}

impl Serialize for MemberException {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("manager", &self.manager)?;
        map.serialize_entry("tool", &self.tool)?;
        map.serialize_entry("key", &self.key)?;
        match &self.exception {
            Exception::LeaveOut => map.serialize_entry("kind", "leave_out")?,
            Exception::Versions(versions) => {
                map.serialize_entry("kind", "versions")?;
                map.serialize_entry("default", &versions.default)?;
                map.serialize_entry("install", &versions.install)?;
            }
        }
        map.end()
    }
}

type Entries = BTreeMap<(Manager, ToolName), Exception>;

/// Validated exceptions. Serde shapes stay inside this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Toolset {
    /// Only [`Exception::LeaveOut`].
    defaults: Entries,
    members: BTreeMap<MemberName, Entries>,
}

impl Toolset {
    pub(crate) fn empty() -> Self {
        Self {
            defaults: BTreeMap::new(),
            members: BTreeMap::new(),
        }
    }

    pub(crate) fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > LIMIT {
            return Err(invalid("fleet-tools.toml", "the document exceeds 64 KiB"));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| invalid("fleet-tools.toml", "the document must be UTF-8 TOML"))?;
        if text.trim().is_empty() {
            return Ok(Self::empty());
        }
        let root: toml::Table = toml::from_str(text).map_err(|_| {
            invalid(
                "fleet-tools.toml",
                "the document must be TOML with only [defaults] and [members.<name>]",
            )
        })?;
        let mut toolset = Self::empty();
        for (key, value) in &root {
            match key.as_str() {
                "defaults" => toolset.defaults = parse_scope("defaults", value, false)?,
                "members" => toolset.members = parse_members(value)?,
                other => return Err(invalid(other, "unknown key")),
            }
        }
        Ok(toolset)
    }

    pub(crate) fn member_names(&self) -> impl Iterator<Item = &MemberName> {
        self.members.keys()
    }

    /// The exceptions that apply to `member`: its own entries, and the defaults'
    /// for the tools it has no entry for. Ordered by manager, then tool.
    pub(crate) fn exceptions(&self, member: &MemberName) -> Vec<MemberException> {
        let own = self.members.get(member);
        let mut applied: BTreeMap<&(Manager, ToolName), MemberException> = BTreeMap::new();
        for (entry, exception) in &self.defaults {
            applied.insert(entry, exception_of(None, entry, exception));
        }
        for (entry, exception) in own.into_iter().flatten() {
            applied.insert(entry, exception_of(Some(member), entry, exception));
        }
        applied.into_values().collect()
    }
}

fn exception_of(
    member: Option<&MemberName>,
    (manager, tool): &(Manager, ToolName),
    exception: &Exception,
) -> MemberException {
    MemberException {
        manager: *manager,
        tool: tool.clone(),
        key: entry_key(member, *manager, tool),
        exception: exception.clone(),
    }
}

fn parse_members(value: &toml::Value) -> Result<BTreeMap<MemberName, Entries>> {
    let Some(table) = value.as_table() else {
        return Err(invalid("members", "expected a table"));
    };
    if table.len() > MAX_MEMBERS {
        return Err(invalid("members", "at most 64 member sections"));
    }
    let mut members = BTreeMap::new();
    for (key, value) in table {
        let path = format!("members.{key}");
        let name = MemberName::try_from(key.clone()).map_err(|_| {
            invalid(
                &path,
                "member names are 1-63 lowercase letters, digits or inner hyphens",
            )
        })?;
        members.insert(name, parse_scope(&path, value, true)?);
    }
    Ok(members)
}

/// `[defaults]` or one `[members.<name>]`. Only a member may name versions.
fn parse_scope(path: &str, value: &toml::Value, member: bool) -> Result<Entries> {
    let Some(table) = value.as_table() else {
        return Err(invalid(path, "expected a table"));
    };
    let mut entries = Entries::new();
    for (key, value) in table {
        let child = format!("{path}.{key}");
        let manager = manager_named(key).ok_or_else(|| invalid(&child, "unknown key"))?;
        let Some(manager_table) = value.as_table() else {
            return Err(invalid(&child, "expected a table"));
        };
        for (field, value) in manager_table {
            match field.as_str() {
                "tools" => {
                    let tools_path = format!("{child}.tools");
                    let Some(tools) = value.as_table() else {
                        return Err(invalid(&tools_path, "expected a table"));
                    };
                    for (tool, value) in tools {
                        let tool_path = format!("{tools_path}.{tool}");
                        let name = require_tool(&tool_path, manager, tool)?;
                        if !member && value.as_table().is_some_and(|entry| !entry.is_empty()) {
                            return Err(invalid(
                                &tool_path,
                                "a [defaults] entry only leaves a tool out and names no versions; pin versions in the workspace, or give one member its own",
                            ));
                        }
                        let exception = parse_entry(&tool_path, tool, value)?;
                        if !member && exception != Exception::LeaveOut {
                            return Err(invalid(
                                &tool_path,
                                "a [defaults] entry only leaves a tool out and names no versions; pin versions in the workspace, or give one member its own",
                            ));
                        }
                        entries.insert((manager, name), exception);
                    }
                }
                "version" => {
                    return Err(invalid(
                        &format!("{child}.version"),
                        "fleet-tools.toml holds only exceptions to the pins and no longer sets a manager's executable",
                    ));
                }
                other => return Err(invalid(&format!("{child}.{other}"), "unknown key")),
            }
        }
    }
    Ok(entries)
}

fn parse_entry(path: &str, tool: &str, value: &toml::Value) -> Result<Exception> {
    let Some(table) = value.as_table() else {
        return Err(invalid(path, "expected a table"));
    };
    let mut default = None;
    let mut install = Vec::new();
    for (key, value) in table {
        match key.as_str() {
            "default" => {
                let field = format!("{path}.default");
                let Some(text) = value.as_str() else {
                    return Err(invalid(&field, "expected a string"));
                };
                default = Some(exact_version(&field, tool, text)?);
            }
            "install" => {
                let field = format!("{path}.install");
                let Some(array) = value.as_array() else {
                    return Err(invalid(&field, "expected an array of strings"));
                };
                for item in array {
                    let Some(text) = item.as_str() else {
                        return Err(invalid(&field, "expected a string"));
                    };
                    install.push(exact_version(&field, tool, text)?);
                }
            }
            other => return Err(invalid(&format!("{path}.{other}"), "unknown key")),
        }
    }
    match Versions::new(default, install) {
        Some(versions) => Ok(Exception::Versions(versions)),
        None if table.is_empty() => Ok(Exception::LeaveOut),
        None => Err(invalid(
            path,
            "an entry names at least one version, or is empty to leave the tool out",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const EXAMPLE: &str = r#"
[defaults.mise.tools.sentry-cli]          # leave this pin out everywhere

[members.macbook-pro.xcodes.tools.xcode]
default = "26.3"                          # this member intentionally differs

[members.macbook-pro.mise.tools.sentry-cli]
install = ["3.8.0"]                       # and keeps a tool the others leave out

[members.mac-mini.simulators.tools.iOS]   # no iOS runtimes on this member

[members.mac-mini.mise.tools."packslip:example/acme"]
"#;

    fn member(name: &str) -> MemberName {
        name.parse().unwrap()
    }

    fn exceptions(text: &str, name: &str) -> Value {
        let toolset = Toolset::parse(text.as_bytes()).unwrap();
        serde_json::to_value(toolset.exceptions(&member(name))).unwrap()
    }

    fn rejected(text: &str) -> Failure {
        Toolset::parse(text.as_bytes()).unwrap_err()
    }

    #[test]
    fn the_example_applies_defaults_and_each_members_own_entries() {
        assert_eq!(
            exceptions(EXAMPLE, "macbook-pro"),
            json!([
                {"manager": "mise", "tool": "sentry-cli", "key": "members.macbook-pro.mise.tools.sentry-cli",
                 "kind": "versions", "default": null, "install": ["3.8.0"]},
                {"manager": "xcodes", "tool": "xcode", "key": "members.macbook-pro.xcodes.tools.xcode",
                 "kind": "versions", "default": "26.3", "install": []},
            ])
        );
        assert_eq!(
            exceptions(EXAMPLE, "mac-mini"),
            json!([
                {"manager": "mise", "tool": "packslip:example/acme",
                 "key": "members.mac-mini.mise.tools.\"packslip:example/acme\"", "kind": "leave_out"},
                {"manager": "mise", "tool": "sentry-cli", "key": "defaults.mise.tools.sentry-cli",
                 "kind": "leave_out"},
                {"manager": "simulators", "tool": "iOS", "key": "members.mac-mini.simulators.tools.iOS",
                 "kind": "leave_out"},
            ])
        );
        assert_eq!(
            exceptions(EXAMPLE, "another-mac"),
            json!([{"manager": "mise", "tool": "sentry-cli", "key": "defaults.mise.tools.sentry-cli",
                    "kind": "leave_out"}])
        );
        let toolset = Toolset::parse(EXAMPLE.as_bytes()).unwrap();
        assert_eq!(
            toolset
                .member_names()
                .map(MemberName::as_str)
                .collect::<Vec<_>>(),
            ["mac-mini", "macbook-pro"]
        );
    }

    #[test]
    fn install_omits_the_default() {
        let text = "[members.alpha.mise.tools.node]\ndefault = \"22.19.0\"\ninstall = [\"22.19.0\", \"22.16.0\"]\n";
        assert_eq!(
            exceptions(text, "alpha")[0],
            json!({"manager": "mise", "tool": "node", "key": "members.alpha.mise.tools.node",
                   "kind": "versions", "default": "22.19.0", "install": ["22.16.0"]})
        );
    }

    #[test]
    fn a_missing_or_blank_document_is_empty() {
        for text in ["", "   \n", "# comment only\n"] {
            let toolset = Toolset::parse(text.as_bytes()).unwrap();
            assert!(toolset.exceptions(&member("alpha")).is_empty());
            assert!(toolset.member_names().next().is_none());
        }
    }

    #[test]
    fn the_desired_state_shape_is_refused_by_key() {
        let cases = [
            (
                "[defaults.mise.tools.node]\ndefault = \"22.19.0\"\n",
                "defaults.mise.tools.node",
                "only leaves a tool out",
            ),
            (
                "[defaults.xcodes.tools.xcode]\ninstall = [\"26.0.1\"]\n",
                "defaults.xcodes.tools.xcode",
                "only leaves a tool out",
            ),
            (
                "[defaults.mise.tools.node]\ninstall = []\n",
                "defaults.mise.tools.node",
                "only leaves a tool out",
            ),
            (
                "[members.alpha.mise.tools.node]\ninstall = []\n",
                "members.alpha.mise.tools.node",
                "names at least one version",
            ),
            (
                "[defaults.mise]\nversion = \"2026.11.6\"\n",
                "defaults.mise.version",
                "no longer sets a manager's executable",
            ),
            (
                "[members.alpha.xcodes]\nversion = \"1.6.0\"\n",
                "members.alpha.xcodes.version",
                "no longer sets a manager's executable",
            ),
        ];
        for (text, path, rule) in cases {
            let error = rejected(text);
            assert_eq!(error.code, "invalid_fleet_tools");
            assert!(error.message.starts_with(path), "{text}: {error}");
            assert!(error.message.contains(rule), "{text}: {error}");
            assert!(!error.message.contains("22.19.0") && !error.message.contains("2026.11.6"));
        }
    }

    #[test]
    fn unknown_keys_are_rejected_at_every_level() {
        let cases = [
            ("[other]\nkey = \"x\"\n", "other"),
            ("[defaults.nope]\n", "defaults.nope"),
            (
                "[defaults.mise]\nextra = \"SENTINEL\"\n",
                "defaults.mise.extra",
            ),
            (
                "[defaults.mise.tools]\nnode = \"22\"\n",
                "defaults.mise.tools.node",
            ),
            (
                "[members.alpha.mise.tools.node]\nextra = \"SENTINEL\"\n",
                "members.alpha.mise.tools.node.extra",
            ),
            ("defaults = \"SENTINEL\"\n", "defaults"),
            (
                "[members.alpha.mise.tools.node]\ninstall = \"22.19.0\"\n",
                "members.alpha.mise.tools.node.install",
            ),
            (
                "[members.alpha.mise.tools.node]\ninstall = [1]\n",
                "members.alpha.mise.tools.node.install",
            ),
        ];
        for (text, path) in cases {
            let error = rejected(text);
            assert_eq!(error.code, "invalid_fleet_tools");
            assert!(error.message.contains(path), "{text}: {error}");
            assert!(!error.message.contains("SENTINEL"), "{error}");
            assert!(!error.hint.contains("SENTINEL"));
        }
    }

    #[test]
    fn symbolic_and_prefixed_versions_are_rejected() {
        for version in [
            "latest",
            "lts",
            "system",
            "ref:main",
            "path:./node",
            "prefix:22",
            "sub-1.0",
            "22 .0",
            "",
        ] {
            let text = format!("[members.alpha.mise.tools.node]\ndefault = {version:?}\n");
            let error = rejected(&text);
            let Some((path, detail)) = error.message.split_once(": ") else {
                panic!("{error}");
            };
            assert_eq!(path, "members.alpha.mise.tools.node.default");
            assert_eq!(
                detail,
                "versions must be exact, e.g. 22.19.0; run `mise ls-remote node` to pick one"
            );
            if !version.is_empty() {
                assert!(!detail.contains(version), "{error}");
            }
        }
    }

    #[test]
    fn xcodes_allows_only_xcode_and_simulators_only_platforms() {
        let error = rejected("[members.alpha.xcodes.tools.node]\ndefault = \"16.4\"\n");
        assert!(
            error.message.contains("members.alpha.xcodes.tools.node"),
            "{error}"
        );
        assert!(
            error.message.contains("xcodes manages only the tool xcode"),
            "{error}"
        );
        assert!(!error.message.contains("16.4"));
        let error = rejected("[defaults.simulators.tools.ios]\n");
        assert!(error.message.contains("platforms: iOS"), "{error}");
        assert!(Toolset::parse(b"[defaults.simulators.tools.watchOS]\n").is_ok());
    }

    #[test]
    fn oversized_documents_and_too_many_members_are_rejected() {
        let mut bytes = b"SENTINEL".to_vec();
        bytes.resize(LIMIT + 1, b' ');
        let error = Toolset::parse(&bytes).unwrap_err();
        assert!(error.message.contains("64 KiB"), "{error}");
        assert!(!error.message.contains("SENTINEL"));
        let mut text = String::new();
        for index in 0..64 {
            text.push_str(&format!("[members.m{index:02}.mise.tools.node]\n"));
        }
        assert!(Toolset::parse(text.as_bytes()).is_ok());
        text.push_str("[members.m64.mise.tools.node]\n");
        assert!(
            rejected(&text)
                .message
                .contains("at most 64 member sections")
        );
    }

    #[test]
    fn member_names_use_roster_syntax() {
        for name in ["MacBook", "a_b", "-alpha", "alpha-"] {
            let error = rejected(&format!("[members.{name}.mise.tools.node]\n"));
            assert!(
                error.message.contains(&format!("members.{name}")),
                "{name}: {error}"
            );
            assert!(error.message.contains("member names are 1-63"), "{error}");
        }
    }
}
