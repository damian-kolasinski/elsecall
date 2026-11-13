//! `fleet-tools.toml`: exact desired versions, with defaults and member overlays.
//!
//! Parsing validates untrusted bytes. [`Toolset::resolve`] applies one member's
//! overlay. A member tool entry replaces the whole default for that tool, and an
//! entry that wants nothing is dropped from the resolved map.
use crate::{
    failure::{Failure, Result},
    roster::MemberName,
    tool_inventory::{Manager, ToolName, ToolVersion},
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

pub const LIMIT: usize = 64 * 1024;
const MAX_MEMBERS: usize = 64;

pub(crate) fn invalid(path: &str, rule: &str) -> Failure {
    Failure::new(
        "invalid_fleet_tools",
        format!("{path}: {rule}"),
        "Fix the named key in fleet-tools.toml. Versions must be exact, for example 22.19.0. Values are omitted from this error.",
    )
}

pub(crate) fn version_rule(tool: &str) -> String {
    format!("versions must be exact, e.g. 22.19.0; run `mise ls-remote {tool}` to pick one")
}

/// An exact version. Symbolic requests and mise selectors are rejected.
pub fn exact_version(path: &str, tool: &str, value: &str) -> Result<ToolVersion> {
    let symbolic = matches!(value, "latest" | "lts" | "system")
        || value.starts_with("ref:")
        || value.starts_with("path:")
        || value.starts_with("prefix:")
        || value.starts_with("sub-");
    match ToolVersion::new(value) {
        Some(version) if !symbolic => Ok(version),
        _ => Err(invalid(path, &version_rule(tool))),
    }
}

pub fn require_tool(path: &str, manager: Manager, name: &str) -> Result<ToolName> {
    if manager == Manager::Xcodes && name != "xcode" {
        return Err(invalid(path, "xcodes manages only the tool xcode"));
    }
    ToolName::new(name).ok_or_else(|| {
        invalid(
            path,
            "tool names cannot be empty or contain whitespace or control characters",
        )
    })
}

/// One tool's desired versions. `install` never contains `default`.
///
/// An entry that names neither is not a [`Want`]: it means nothing is wanted, and
/// [`Toolset::resolve`] drops that tool instead of keeping an empty value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Want {
    default: Option<ToolVersion>,
    install: BTreeSet<ToolVersion>,
}

impl Want {
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
}

/// What one manager should provide on one member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManagerWants {
    #[serde(rename = "version")]
    pub executable: Option<ToolVersion>,
    pub tools: BTreeMap<ToolName, Want>,
}

/// Resolved desired state for one member. Both managers are always present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemberWants {
    pub mise: ManagerWants,
    pub xcodes: ManagerWants,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    /// The member wants this tool gone, even when defaults name it.
    Clear,
    Want(Want),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManagerDoc {
    version: Option<ToolVersion>,
    tools: BTreeMap<ToolName, Entry>,
}

impl ManagerDoc {
    fn empty() -> Self {
        Self {
            version: None,
            tools: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Managers {
    mise: ManagerDoc,
    xcodes: ManagerDoc,
}

impl Managers {
    fn empty() -> Self {
        Self {
            mise: ManagerDoc::empty(),
            xcodes: ManagerDoc::empty(),
        }
    }
}

/// Validated desired state. Serde shapes stay inside this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolset {
    defaults: Managers,
    members: BTreeMap<MemberName, Managers>,
}

impl Toolset {
    pub fn empty() -> Self {
        Self {
            defaults: Managers::empty(),
            members: BTreeMap::new(),
        }
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
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
        let mut defaults = Managers::empty();
        let mut members = BTreeMap::new();
        for (key, value) in &root {
            match key.as_str() {
                "defaults" => defaults = parse_managers("defaults", value)?,
                "members" => members = parse_members(value)?,
                other => return Err(invalid(other, "unknown key")),
            }
        }
        Ok(Self { defaults, members })
    }

    pub fn member_names(&self) -> impl Iterator<Item = &MemberName> {
        self.members.keys()
    }

    /// Defaults, with this member's entries replacing a manager version or a whole tool.
    pub fn resolve(&self, member: &MemberName) -> MemberWants {
        let overlay = self.members.get(member);
        MemberWants {
            mise: resolve_manager(&self.defaults.mise, overlay.map(|item| &item.mise)),
            xcodes: resolve_manager(&self.defaults.xcodes, overlay.map(|item| &item.xcodes)),
        }
    }
}

fn resolve_manager(defaults: &ManagerDoc, member: Option<&ManagerDoc>) -> ManagerWants {
    let executable = member
        .and_then(|item| item.version.clone())
        .or_else(|| defaults.version.clone());
    let mut tools = BTreeMap::new();
    for (name, entry) in &defaults.tools {
        if let Entry::Want(want) = entry {
            tools.insert(name.clone(), want.clone());
        }
    }
    if let Some(member) = member {
        for (name, entry) in &member.tools {
            match entry {
                Entry::Want(want) => {
                    tools.insert(name.clone(), want.clone());
                }
                Entry::Clear => {
                    tools.remove(name);
                }
            }
        }
    }
    ManagerWants { executable, tools }
}

fn parse_members(value: &toml::Value) -> Result<BTreeMap<MemberName, Managers>> {
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
        members.insert(name, parse_managers(&path, value)?);
    }
    Ok(members)
}

fn parse_managers(path: &str, value: &toml::Value) -> Result<Managers> {
    let Some(table) = value.as_table() else {
        return Err(invalid(path, "expected a table"));
    };
    let mut managers = Managers::empty();
    for (key, value) in table {
        let child = format!("{path}.{key}");
        match key.as_str() {
            "mise" => managers.mise = parse_manager(Manager::Mise, &child, value)?,
            "xcodes" => managers.xcodes = parse_manager(Manager::Xcodes, &child, value)?,
            _ => return Err(invalid(&child, "unknown key")),
        }
    }
    Ok(managers)
}

fn parse_manager(manager: Manager, path: &str, value: &toml::Value) -> Result<ManagerDoc> {
    let Some(table) = value.as_table() else {
        return Err(invalid(path, "expected a table"));
    };
    let mut doc = ManagerDoc::empty();
    for (key, value) in table {
        match key.as_str() {
            "version" => {
                let field = format!("{path}.version");
                let Some(text) = value.as_str() else {
                    return Err(invalid(&field, "expected a string"));
                };
                doc.version = Some(exact_version(&field, manager.as_str(), text)?);
            }
            "tools" => {
                let tools_path = format!("{path}.tools");
                let Some(tools) = value.as_table() else {
                    return Err(invalid(&tools_path, "expected a table"));
                };
                doc.tools = parse_tools(manager, &tools_path, tools)?;
            }
            other => return Err(invalid(&format!("{path}.{other}"), "unknown key")),
        }
    }
    Ok(doc)
}

fn parse_tools(
    manager: Manager,
    path: &str,
    table: &toml::Table,
) -> Result<BTreeMap<ToolName, Entry>> {
    let mut tools = BTreeMap::new();
    for (key, value) in table {
        let tool_path = format!("{path}.{key}");
        let name = require_tool(&tool_path, manager, key)?;
        tools.insert(name, parse_entry(&tool_path, key, value)?);
    }
    Ok(tools)
}

fn parse_entry(path: &str, tool: &str, value: &toml::Value) -> Result<Entry> {
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
    Ok(match Want::new(default, install) {
        Some(want) => Entry::Want(want),
        None => Entry::Clear,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const EXAMPLE: &str = r#"
# Manager tables are named after slice-1 managers: `mise`, `xcodes`.
[defaults.mise]
version = "2026.11.6"            # optional: the mise executable every member runs

[defaults.mise.tools.node]
default = "22.19.0"              # the global default; implies installed

[defaults.mise.tools.tuist]
install = ["4.205.0"]            # installed alongside, not default

[defaults.mise.tools."packslip:example/acme"]
default = "0.12.0"

[defaults.xcodes]
version = "1.6.0"                # optional: the xcodes executable

[defaults.xcodes.tools.xcode]    # Xcode is the single tool `xcode` under `xcodes`
default = "26.2"
install = ["26.0.1"]

[members.macbook-pro.xcodes.tools.xcode]
default = "26.3"                 # replaces the whole defaults entry for this tool on this member

[members.macbook-pro.mise.tools.tuist]   # an empty entry: nothing wanted for tuist on this member
"#;

    fn member(name: &str) -> MemberName {
        name.parse().unwrap()
    }

    fn resolved(text: &str, name: &str) -> Value {
        let toolset = Toolset::parse(text.as_bytes()).unwrap();
        serde_json::to_value(toolset.resolve(&member(name))).unwrap()
    }

    fn rejected(text: &str) -> Failure {
        Toolset::parse(text.as_bytes()).unwrap_err()
    }

    #[test]
    fn example_document_resolves_for_a_member_and_an_overlay() {
        let toolset = Toolset::parse(EXAMPLE.as_bytes()).unwrap();
        assert_eq!(
            toolset
                .member_names()
                .map(MemberName::as_str)
                .collect::<Vec<_>>(),
            ["macbook-pro"]
        );
        assert_eq!(
            serde_json::to_value(toolset.resolve(&member("mac-mini"))).unwrap(),
            json!({
                "mise": {
                    "version": "2026.11.6",
                    "tools": {
                        "node": {"default": "22.19.0", "install": []},
                        "packslip:example/acme": {"default": "0.12.0", "install": []},
                        "tuist": {"default": null, "install": ["4.205.0"]}
                    }
                },
                "xcodes": {
                    "version": "1.6.0",
                    "tools": {
                        "xcode": {"default": "26.2", "install": ["26.0.1"]}
                    }
                }
            })
        );
        assert_eq!(
            serde_json::to_value(toolset.resolve(&member("macbook-pro"))).unwrap(),
            json!({
                "mise": {
                    "version": "2026.11.6",
                    "tools": {
                        "node": {"default": "22.19.0", "install": []},
                        "packslip:example/acme": {"default": "0.12.0", "install": []}
                    }
                },
                "xcodes": {
                    "version": "1.6.0",
                    "tools": {
                        "xcode": {"default": "26.3", "install": []}
                    }
                }
            })
        );
        assert_eq!(
            Toolset::parse(EXAMPLE.as_bytes()).unwrap(),
            toolset,
            "parsing the example again yields the same desired state"
        );
    }

    #[test]
    fn a_member_entry_replaces_the_default_instead_of_merging_it() {
        let text = r#"
[defaults.mise]
version = "1.0.0"
[defaults.mise.tools.node]
default = "22.19.0"
install = ["22.16.0"]
[members.alpha.mise]
version = "2.0.0"
[members.alpha.mise.tools.node]
install = ["20.0.0"]
[members.beta.mise.tools.node]
default = "22.19.0"
install = ["22.19.0", "22.16.0"]
"#;
        assert_eq!(
            resolved(text, "alpha"),
            json!({
                "mise": {
                    "version": "2.0.0",
                    "tools": {"node": {"default": null, "install": ["20.0.0"]}}
                },
                "xcodes": {"version": null, "tools": {}}
            })
        );
        let beta = resolved(text, "beta")["mise"]["tools"]["node"].clone();
        assert_eq!(beta, json!({"default": "22.19.0", "install": ["22.16.0"]}));
        assert_eq!(resolved(text, "gamma")["mise"]["version"], "1.0.0");
    }

    #[test]
    fn an_empty_member_entry_drops_the_tool_and_install_omits_the_default() {
        let text = r#"
[defaults.mise.tools.node]
default = "22.19.0"
install = ["22.19.0", "22.16.0", "22.19.0"]
[defaults.mise.tools.tuist]
install = ["4.205.0"]
[members.alpha.mise.tools.tuist]
"#;
        let alpha = resolved(text, "alpha");
        assert!(alpha["mise"]["tools"].get("tuist").is_none());
        assert_eq!(
            alpha["mise"]["tools"]["node"],
            json!({"default": "22.19.0", "install": ["22.16.0"]})
        );
        assert_eq!(
            resolved(text, "beta")["mise"]["tools"]["tuist"]["install"],
            json!(["4.205.0"])
        );
    }

    #[test]
    fn a_missing_or_blank_document_is_empty() {
        for text in ["", "   \n", "# comment only\n"] {
            let toolset = Toolset::parse(text.as_bytes()).unwrap();
            let wants = toolset.resolve(&member("alpha"));
            assert!(wants.mise.executable.is_none());
            assert!(wants.mise.tools.is_empty());
            assert!(wants.xcodes.tools.is_empty());
            assert!(toolset.member_names().next().is_none());
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
                "[defaults.mise.tools.node]\nextra = \"SENTINEL\"\n",
                "defaults.mise.tools.node.extra",
            ),
            (
                "[members.alpha.xcodes.tools.xcode]\nextra = \"SENTINEL\"\n",
                "members.alpha.xcodes.tools.xcode.extra",
            ),
            ("defaults = \"SENTINEL\"\n", "defaults"),
            ("[defaults.mise]\nversion = 1\n", "defaults.mise.version"),
            (
                "[defaults.mise.tools.node]\ninstall = \"22.19.0\"\n",
                "defaults.mise.tools.node.install",
            ),
            (
                "[defaults.mise.tools.node]\ninstall = [1]\n",
                "defaults.mise.tools.node.install",
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
            "ref:x",
            "ref:main",
            "path:./node",
            "prefix:22",
            "sub-1.0",
            "22 .0",
            "",
        ] {
            let text = format!("[defaults.mise.tools.node]\ndefault = {version:?}\n");
            let error = rejected(&text);
            let rule =
                "versions must be exact, e.g. 22.19.0; run `mise ls-remote node` to pick one";
            let Some((path, detail)) = error.message.split_once(": ") else {
                panic!("{error}");
            };
            assert_eq!(path, "defaults.mise.tools.node.default");
            assert_eq!(detail, rule);
            if !version.is_empty() {
                assert!(!detail.contains(version), "{error}");
            }
        }
        let install = rejected("[defaults.xcodes]\nversion = \"latest\"\n");
        let detail = install
            .message
            .split_once(": ")
            .map(|(_, detail)| detail)
            .unwrap_or("");
        assert!(install.message.starts_with("defaults.xcodes.version"));
        assert!(detail.contains("mise ls-remote xcodes"));
        assert!(!detail.contains("latest"));
    }

    #[test]
    fn xcodes_allows_only_xcode() {
        let error = rejected("[defaults.xcodes.tools.node]\ndefault = \"16.4\"\n");
        assert!(
            error.message.contains("defaults.xcodes.tools.node"),
            "{error}"
        );
        assert!(
            error.message.contains("xcodes manages only the tool xcode"),
            "{error}"
        );
        assert!(!error.message.contains("16.4"));
        assert!(Toolset::parse(b"[defaults.xcodes.tools.xcode]\ndefault = \"26.2\"\n").is_ok());
    }

    #[test]
    fn oversized_documents_and_too_many_members_are_rejected() {
        let mut bytes = b"SENTINEL".to_vec();
        bytes.resize(LIMIT + 1, b' ');
        let error = Toolset::parse(&bytes).unwrap_err();
        assert!(error.message.contains("fleet-tools.toml"), "{error}");
        assert!(error.message.contains("64 KiB"), "{error}");
        assert!(!error.message.contains("SENTINEL"));

        let mut text = String::new();
        for index in 0..64 {
            text.push_str(&format!(
                "[members.m{index:02}.mise.tools.node]\ndefault = \"1.0.0\"\n"
            ));
        }
        assert!(Toolset::parse(text.as_bytes()).is_ok());
        text.push_str("[members.m64.mise.tools.node]\ndefault = \"1.0.0\"\n");
        let error = rejected(&text);
        assert!(error.message.contains("members"), "{error}");
        assert!(
            error.message.contains("at most 64 member sections"),
            "{error}"
        );
    }

    #[test]
    fn member_names_use_roster_syntax() {
        for name in ["MacBook", "a_b", "-alpha", "alpha-", ""] {
            let text = format!("[members.{name}.mise.tools.node]\ndefault = \"1.0.0\"\n");
            let error = if name.is_empty() {
                rejected("[members.\"\".mise.tools.node]\ndefault = \"1.0.0\"\n")
            } else {
                rejected(&text)
            };
            assert!(
                error.message.contains(&format!("members.{name}")),
                "{name}: {error}"
            );
            assert!(
                error
                    .message
                    .contains("member names are 1-63 lowercase letters, digits or inner hyphens"),
                "{error}"
            );
        }
    }
}
