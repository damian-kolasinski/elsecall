//! The `[tools]` table of a mise config file, read as data.
//!
//! The whole file must be TOML; only `[tools]` is looked at. `[env]`, `[hooks]`,
//! `[tasks]` and the rest are never evaluated. A tool's value is a version, a
//! list of versions, a table with `version`, or a list mixing both. A table
//! whose `os` leaves out Apple Silicon macOS (`macos`, `macos/arm64`) pins
//! nothing here: every member runs on it. Its other options, such as
//! `postinstall`, are ignored: reading never runs them. Versions must be
//! literal, so a template is an error rather than something to render. A
//! version repeated for one tool is one pin. The asdf names mise accepts for
//! its own tools (`nodejs`, `golang`) are read as the names inventory reports.
use crate::{
    pins::{Environment, Pin, Reading, SourceFile, Target, invalid, literal_version},
    tool_inventory::{ToolName, ToolVersion},
};
use elsecall_contract::failure::Result;

/// More versions than any real config pins; a bound on untrusted input.
const MAX_PINS: usize = 1024;

/// One tool's requested versions in one file, in the order written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ToolRequests {
    pub(super) tool: ToolName,
    pub(super) versions: Vec<ToolVersion>,
}

/// The tools `text` pins, by tool name. `file` names the file in errors.
pub(super) fn tools(text: &str, file: &str) -> Result<Vec<ToolRequests>> {
    let root: toml::Table =
        toml::from_str(text).map_err(|_| invalid(file, "", "the file must be TOML"))?;
    let Some(tools) = root.get("tools") else {
        return Ok(Vec::new());
    };
    let Some(tools) = tools.as_table() else {
        return Err(invalid(file, "tools", "expected a table"));
    };
    let mut requests = Vec::new();
    let mut count = 0;
    for (name, value) in tools {
        let tool = tool_name(name).ok_or_else(|| {
            invalid(
                file,
                "tools",
                "tool names cannot be empty or contain whitespace or control characters",
            )
        })?;
        let key = format!("tools.{name}");
        let versions = tool_versions(file, &key, value)?;
        count += versions.len();
        if count > MAX_PINS {
            return Err(invalid(file, "tools", "at most 1024 versions in one file"));
        }
        add(&mut requests, tool, versions);
    }
    Ok(requests)
}

/// A tool name as written, with mise's asdf aliases replaced by its own names.
pub(super) fn tool_name(name: &str) -> Option<ToolName> {
    let name = match name {
        "nodejs" => "node",
        "golang" => "go",
        other => other,
    };
    ToolName::new(name)
}

/// Adds `versions` to `tool`'s request, keeping the first of any repeat.
pub(super) fn add(requests: &mut Vec<ToolRequests>, tool: ToolName, versions: Vec<ToolVersion>) {
    let index = match requests.iter().position(|request| request.tool == tool) {
        Some(index) => index,
        None => {
            requests.push(ToolRequests {
                tool,
                versions: Vec::new(),
            });
            requests.len() - 1
        }
    };
    let request = &mut requests[index];
    for version in versions {
        if !request.versions.contains(&version) {
            request.versions.push(version);
        }
    }
    if request.versions.is_empty() {
        requests.remove(index);
    }
}

/// One pin per requested version, in the order written.
pub(super) fn push_pins(
    requests: Vec<ToolRequests>,
    environment: Option<Environment>,
    source: &SourceFile,
    reading: &mut Reading,
) {
    for request in requests {
        for version in request.versions {
            reading.pins.push(Pin {
                target: Target::Mise {
                    tool: request.tool.clone(),
                    environment: environment.clone(),
                },
                version,
                source: source.clone(),
            });
        }
    }
}

fn tool_versions(file: &str, key: &str, value: &toml::Value) -> Result<Vec<ToolVersion>> {
    match value {
        toml::Value::Array(items) => {
            let mut versions = Vec::new();
            for item in items {
                if let Some(version) = one_version(file, key, item)? {
                    versions.push(version);
                }
            }
            Ok(versions)
        }
        other => Ok(one_version(file, key, other)?.into_iter().collect()),
    }
}

fn one_version(file: &str, key: &str, value: &toml::Value) -> Result<Option<ToolVersion>> {
    match value {
        toml::Value::String(text) => literal_version(file, key, text).map(Some),
        toml::Value::Table(table) => {
            if !for_macos(file, key, table.get("os"))? {
                return Ok(None);
            }
            match table.get("version") {
                Some(toml::Value::String(text)) => literal_version(file, key, text).map(Some),
                _ => Err(invalid(file, key, "a tool table needs a version string")),
            }
        }
        _ => Err(invalid(
            file,
            key,
            "expected a version, a list of versions, or a table with a version",
        )),
    }
}

/// Whether a tool table applies on an Apple Silicon Mac: no `os`, or an `os`
/// naming macOS with no architecture or with `arm64`.
fn for_macos(file: &str, key: &str, os: Option<&toml::Value>) -> Result<bool> {
    let Some(os) = os else {
        return Ok(true);
    };
    let names = match os {
        toml::Value::String(name) => vec![name.as_str()],
        toml::Value::Array(items) => items
            .iter()
            .map(|item| item.as_str())
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| invalid(file, key, "os must list operating system names"))?,
        _ => return Err(invalid(file, key, "os must list operating system names")),
    };
    Ok(names.iter().any(|name| {
        let (system, architecture) = name.split_once('/').unwrap_or((name, "arm64"));
        matches!(system, "macos" | "darwin") && matches!(architecture, "arm64" | "aarch64")
    }))
}

#[cfg(test)]
mod tests {
    use crate::pins::mise_config::*;

    fn pinned(text: &str) -> Vec<(String, Vec<String>)> {
        tools(text, "mise.toml")
            .unwrap()
            .into_iter()
            .map(|request| {
                (
                    request.tool.as_str().to_owned(),
                    request
                        .versions
                        .iter()
                        .map(|version| version.as_str().to_owned())
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn strings_lists_and_tables_are_pins_and_other_tables_are_ignored() {
        let text = r#"
            min_version = "2026.1.0"
            [tools]
            python = "3.10.4"
            bun = "latest"
            node = ["22.19.0", "20.19.5"]
            "github:nextest-rs/nextest" = { version = "0.9.150", version_prefix = "cargo-nextest-" }
            "packslip:example/acme" = { version = "0.4.0", postinstall = "touch /tmp/never" }
            [env]
            TOKEN = "{{ exec(command='cat ~/.secret') }}"
            [hooks]
            enter = "touch /tmp/never"
            [tasks.build]
            run = "make"
        "#;
        assert_eq!(
            pinned(text),
            [
                ("bun".into(), vec!["latest".into()]),
                ("github:nextest-rs/nextest".into(), vec!["0.9.150".into()]),
                ("node".into(), vec!["22.19.0".into(), "20.19.5".into()]),
                ("packslip:example/acme".into(), vec!["0.4.0".into()]),
                ("python".into(), vec!["3.10.4".into()]),
            ]
        );
    }

    #[test]
    fn a_file_without_tools_pins_nothing() {
        assert!(pinned("[settings]\njobs = 1\n").is_empty());
        assert!(pinned("").is_empty());
        assert!(pinned("[tools]\nnode = []\n").is_empty());
    }

    #[test]
    fn a_table_for_other_systems_pins_nothing_on_macos() {
        let text = r#"
            [tools]
            linuxonly = { version = "1.0.0", os = ["linux"] }
            both = { version = "2.0.0", os = ["linux", "macos"] }
            darwin = { version = "3.0.0", os = "darwin" }
            mixed = ["4.0.0", { version = "5.0.0", os = ["windows"] }]
            arm = { version = "6.0.0", os = ["macos/arm64"] }
            intel = { version = "7.0.0", os = ["macos/x64", "linux/arm64"] }
        "#;
        assert_eq!(
            pinned(text),
            [
                ("arm".into(), vec!["6.0.0".into()]),
                ("both".into(), vec!["2.0.0".into()]),
                ("darwin".into(), vec!["3.0.0".into()]),
                ("mixed".into(), vec!["4.0.0".into()]),
            ]
        );
    }

    #[test]
    fn aliases_take_mise_names_and_repeats_are_one_pin() {
        let text = r#"
            [tools]
            nodejs = ["22.19.0", "22.19.0"]
            node = ["20.19.5", "22.19.0"]
            golang = "1.24.1"
        "#;
        assert_eq!(
            pinned(text),
            [
                ("go".into(), vec!["1.24.1".into()]),
                ("node".into(), vec!["20.19.5".into(), "22.19.0".into()]),
            ]
        );
    }

    #[test]
    fn malformed_files_name_the_key_and_never_the_value() {
        let cases = [
            ("not toml = = SECRET", "mise.toml: the file must be TOML."),
            ("tools = \"SECRET\"", "mise.toml: tools: expected a table."),
            (
                "[tools]\nnode = 22",
                "mise.toml: tools.node: expected a version, a list of versions, or a table with a version.",
            ),
            (
                "[tools]\nnode = { os = [\"macos\"], SECRET = true }",
                "mise.toml: tools.node: a tool table needs a version string.",
            ),
            (
                "[tools]\nnode = \"{{ vars.SECRET }}\"",
                "mise.toml: tools.node: versions must be literal; Elsecall does not render templates.",
            ),
            (
                "[tools]\nnode = { version = \"1\", os = [1] }",
                "mise.toml: tools.node: os must list operating system names.",
            ),
            (
                "[tools]\n\"no de\" = \"SECRET\"",
                "mise.toml: tools: tool names cannot be empty or contain whitespace or control characters.",
            ),
        ];
        for (text, message) in cases {
            let error = tools(text, "mise.toml").unwrap_err();
            assert_eq!(error.code, "pin_source_invalid");
            assert_eq!(error.message, message, "{text}");
            assert!(!error.hint.contains("SECRET"));
        }
    }

    #[test]
    fn a_file_with_too_many_versions_is_refused() {
        let mut text = String::from("[tools]\n");
        for index in 0..=MAX_PINS {
            text.push_str(&format!("tool{index} = \"1.0.{index}\"\n"));
        }
        let error = tools(&text, "mise.toml").unwrap_err();
        assert!(error.message.contains("at most 1024"));
    }
}
