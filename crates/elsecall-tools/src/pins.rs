//! Pins: the tool versions a workspace asks for, read from the files that already
//! say so. See `docs/fleet-provisioning.md`.
//!
//! [`PinSource`] is the port: each adapter reads one convention (a mise project,
//! the master's global mise config, `.xcode-version`, `.elsecall-tools.toml`).
//! Reading parses files and never runs code a repository controls: no mise, no
//! templates, no hooks. [`report`] joins the sources, refuses conflicts and marks
//! which pins are exact, against what this machine has installed.
mod conflicts;
mod elsecall_tools_file;
pub(crate) mod exactness;
pub mod lookup;
mod mise_config;
mod mise_global;
mod mise_project;
pub(crate) mod report;
mod source_file;
mod tool_versions;
mod xcode_version;

use crate::tool_inventory::{ToolManager, ToolName, ToolVersion};
use elsecall_contract::failure::{Failure, Result};
use std::path::{Path, PathBuf};

/// How many characters a pinned version may have.
const MAX_VERSION: usize = 128;

/// Which convention a pin was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SourceKind {
    MiseProject,
    MiseGlobal,
    XcodeVersion,
    ElsecallTools,
}

impl SourceKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::MiseProject => "mise_project",
            Self::MiseGlobal => "mise_global",
            Self::XcodeVersion => "xcode_version",
            Self::ElsecallTools => "elsecall_tools",
        }
    }
}

/// One file a source read: relative to the workspace, or `~/…` for the master's
/// global config, so it reads the same on every member.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SourceFile {
    pub(crate) kind: SourceKind,
    pub(crate) file: String,
}

/// A simulator runtime's platform, as `simctl` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
/// Variants are in name order, which is how pins sort.
pub(crate) enum Platform {
    Ios,
    Tvos,
    Visionos,
    Watchos,
}

impl Platform {
    pub(crate) const ALL: [Self; 4] = [Self::Ios, Self::Tvos, Self::Visionos, Self::Watchos];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ios => "iOS",
            Self::Tvos => "tvOS",
            Self::Visionos => "visionOS",
            Self::Watchos => "watchOS",
        }
    }
}

/// What a pin asks to have installed. The manager follows from the variant.
/// Only mise has environments.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Target {
    Mise {
        tool: ToolName,
        environment: Option<Environment>,
    },
    Xcode,
    SimulatorRuntime(Platform),
}

impl Target {
    /// The manager and tool names inventory and `fleet-tools.toml` use.
    pub(crate) fn manager(&self) -> &'static str {
        match self {
            Self::Mise { .. } => "mise",
            Self::Xcode => "xcodes",
            Self::SimulatorRuntime(_) => "simulators",
        }
    }

    pub(crate) fn tool(&self) -> &str {
        match self {
            Self::Mise { tool, .. } => tool.as_str(),
            Self::Xcode => "xcode",
            Self::SimulatorRuntime(platform) => platform.as_str(),
        }
    }

    /// The mise environment whose files pinned this, if any.
    pub(crate) fn environment(&self) -> Option<&Environment> {
        match self {
            Self::Mise { environment, .. } => environment.as_ref(),
            Self::Xcode | Self::SimulatorRuntime(_) => None,
        }
    }
}

/// A mise environment (`MISE_ENV`), from a file such as `mise.signing.toml`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Environment(String);

impl Environment {
    /// Letters, digits, `-` and `_`; never `local`, which mise reserves.
    pub(crate) fn new(value: &str) -> Option<Self> {
        let valid = !value.is_empty()
            && value.len() <= 64
            && value != "local"
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
        valid.then(|| Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// One requested version, as written, and where it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pin {
    pub(crate) target: Target,
    pub(crate) version: ToolVersion,
    pub(crate) source: SourceFile,
}

/// What one source found: the files it read, even those that pin nothing, and
/// their pins in file order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Reading {
    pub(crate) files: Vec<SourceFile>,
    pub(crate) pins: Vec<Pin>,
}

/// The port: one convention for declaring what a workspace needs.
pub(crate) trait PinSource {
    /// The pins this source declares for `workspace`, a canonical directory. A
    /// missing file is no pins; an unreadable or malformed one is an error.
    fn read(&self, workspace: &Path) -> Result<Reading>;
}

/// Reads a workspace with the production adapters, resolving inexact pins
/// against this machine's mise and Xcode.
pub(crate) fn read_workspace(workspace: &Path) -> Result<report::PinReport> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            Failure::new(
                "pin_source_unreadable",
                "HOME is not set, so the global mise config cannot be found.",
                "Run elsecall from a login shell, where HOME names your home folder.",
            )
        })?;
    let workspace = workspace_root(workspace, &home)?;
    let global = mise_global::MiseGlobal::new(home);
    let sources: [&dyn PinSource; 4] = [
        &mise_project::MiseProject,
        &global,
        &xcode_version::XcodeVersionFile,
        &elsecall_tools_file::ElsecallToolsFile,
    ];
    let managers: [&dyn ToolManager; 2] = [&crate::mise_cli::MiseCli, &crate::xcodes::XcodesCli];
    report::read(&workspace, &sources, &managers)
}

/// What a workspace's own files pin, without the global layer, unjudged: what
/// a member reports for the plan. `home` is not a workspace.
pub(crate) fn workspace_pins(workspace: &Path, home: &Path) -> Result<Reading> {
    let workspace = workspace_root(workspace, home)?;
    let sources: [&dyn PinSource; 3] = [
        &mise_project::MiseProject,
        &xcode_version::XcodeVersionFile,
        &elsecall_tools_file::ElsecallToolsFile,
    ];
    let mut reading = Reading::default();
    for source in sources {
        let read = source.read(&workspace)?;
        reading.files.extend(read.files);
        reading.pins.extend(read.pins);
    }
    reading.pins = conflicts::without_conflicts(reading.pins)?;
    Ok(reading)
}

/// What this machine's global mise config pins: the master's, for the plan.
pub(crate) fn global_pins(home: &Path) -> Result<Reading> {
    mise_global::MiseGlobal::new(home.to_owned()).read(home)
}

/// The canonical workspace: a directory that can be listed, and not the home
/// folder, whose mise config is the global layer rather than a project's.
pub(crate) fn workspace_root(path: &Path, home: &Path) -> Result<PathBuf> {
    let root = std::fs::canonicalize(path)
        .ok()
        .filter(|root| std::fs::read_dir(root).is_ok())
        .ok_or_else(|| {
            Failure::new(
                "invalid_request",
                "The workspace path does not name a readable directory.",
                "Pass a repository directory, such as elsecall tools pins ~/Developer/acme-ios.",
            )
        })?;
    if std::fs::canonicalize(home).is_ok_and(|home| home == root) {
        return Err(Failure::new(
            "invalid_request",
            "The home folder is not a workspace; its mise config is the global one.",
            "Pass a repository directory, such as elsecall tools pins ~/Developer/acme-ios.",
        ));
    }
    Ok(root)
}

/// A file that exists but cannot be read as a pin source.
pub(crate) fn unreadable(file: &str, reason: &str) -> Failure {
    Failure::new(
        "pin_source_unreadable",
        format!("{file}: {reason}."),
        "Make the file a regular UTF-8 text file inside the workspace, or remove it.",
    )
}

/// A file whose contents break a rule. Names the key and the rule, never a value.
pub(crate) fn invalid(file: &str, key: &str, rule: &str) -> Failure {
    let place = if key.is_empty() {
        file.to_owned()
    } else {
        format!("{file}: {key}")
    };
    Failure::new(
        "pin_source_invalid",
        format!("{place}: {rule}."),
        "Fix the named key. Values are omitted from this error.",
    )
}

/// A requested version from a file: plain text, not a template, not too long.
pub(crate) fn literal_version(file: &str, key: &str, value: &str) -> Result<ToolVersion> {
    if ["{{", "{%", "{#"].iter().any(|open| value.contains(open)) {
        return Err(invalid(
            file,
            key,
            "versions must be literal; Elsecall does not render templates",
        ));
    }
    if value.chars().count() > MAX_VERSION {
        return Err(invalid(file, key, "versions are at most 128 characters"));
    }
    ToolVersion::new(value).ok_or_else(|| {
        invalid(
            file,
            key,
            "versions cannot be empty or contain whitespace or control characters",
        )
    })
}

/// A version of digits and dots with at least `segments` parts, as Xcode and
/// simulator runtimes write them (`26.2`).
pub(crate) fn dotted_version(value: &str, segments: usize) -> Option<ToolVersion> {
    let parts: Vec<&str> = value.split('.').collect();
    let valid = value.len() <= 32
        && parts.len() >= segments
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    valid.then(|| ToolVersion::new(value)).flatten()
}

#[cfg(test)]
mod tests {
    use crate::pins::*;

    #[test]
    fn a_workspace_is_a_listable_directory_other_than_home() {
        let home = tempfile::tempdir().unwrap();
        let workspace = home.path().join("acme-ios");
        std::fs::create_dir(&workspace).unwrap();
        assert!(workspace_root(&workspace, home.path()).is_ok());
        for path in [home.path().to_owned(), workspace.join("missing")] {
            let error = workspace_root(&path, home.path()).unwrap_err();
            assert_eq!(error.code, "invalid_request");
        }
        let file = workspace.join("mise.toml");
        std::fs::write(&file, "").unwrap();
        assert!(workspace_root(&file, home.path()).is_err());
    }

    #[test]
    fn environments_are_plain_names_and_never_local() {
        assert!(Environment::new("signing").is_some());
        assert!(Environment::new("ci_2-x").is_some());
        for value in ["", "local", "a.b", "a b", "ä"] {
            assert!(Environment::new(value).is_none(), "{value}");
        }
    }

    #[test]
    fn literal_versions_refuse_templates_and_never_echo_them() {
        let error = literal_version("mise.toml", "tools.node", "{{ env.SECRET }}").unwrap_err();
        assert_eq!(error.code, "pin_source_invalid");
        assert!(!error.message.contains("SECRET"), "{}", error.message);
        assert!(literal_version("mise.toml", "tools.node", "22 1").is_err());
        assert!(literal_version("mise.toml", "tools.node", &"1".repeat(129)).is_err());
        assert_eq!(
            literal_version("mise.toml", "tools.node", "22.19.0")
                .unwrap()
                .as_str(),
            "22.19.0"
        );
    }

    #[test]
    fn dotted_versions_need_digits_and_enough_segments() {
        assert!(dotted_version("26.2", 2).is_some());
        assert!(dotted_version("26.0.1", 2).is_some());
        assert!(dotted_version("26", 1).is_some());
        for value in ["26", "26.", ".2", "26.2b", "26 .2", "26-2"] {
            assert!(dotted_version(value, 2).is_none(), "{value}");
        }
    }

    #[test]
    fn targets_use_inventory_manager_and_tool_names() {
        let node = Target::Mise {
            tool: ToolName::new("node").unwrap(),
            environment: None,
        };
        assert_eq!((node.manager(), node.tool()), ("mise", "node"));
        assert_eq!(
            (Target::Xcode.manager(), Target::Xcode.tool()),
            ("xcodes", "xcode")
        );
        let ios = Target::SimulatorRuntime(Platform::Ios);
        assert_eq!((ios.manager(), ios.tool()), ("simulators", "iOS"));
    }
}
