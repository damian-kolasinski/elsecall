//! Simulator runtimes on this Mac, read through `simctl`.
//!
//! `simctl` ships inside Xcode, so it is found through the selected developer
//! directory: `/usr/bin/xcrun --find simctl`. When that fails, as on a Mac with
//! only the Command Line Tools, the executable is `missing` and no runtimes are
//! listed; that is not a failure. `ELSECALL_SIMCTL` replaces the lookup with an
//! absolute path, under the same rules as the other managers' overrides: a
//! non-empty value that is not absolute fails the manager, and an absolute path
//! that is not a file makes the executable missing.
//!
//! The executable's version is the CoreSimulator project version that
//! `simctl --version` prints (`@(#)PROGRAM:simctl  PROJECT:CoreSimulator-1051.17.7`
//! is `1051.17.7`). The runtimes are `simctl runtime list -j`: an object keyed by
//! runtime identifier, whose entries name a `platformIdentifier`, a `version`, a
//! `build` and a `state`. Only `Ready` runtimes are installed, and other entries
//! are skipped however incomplete they are. A platform outside
//! iOS, tvOS, watchOS and visionOS is skipped. The tool is the platform as
//! `simctl` names it (`iOS`), the version is the runtime's, and the build is kept
//! beside it. No runtime is a default. Output is not logged.
use crate::tool_inventory::{
    Detail, Executable, InstalledTool, InventoryFailure, Manager, ManagerInventory, ToolManager,
    ToolName, ToolVersion,
};
use elsecall_host::command_capture::{self, Program};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

const TIMEOUT: Duration = Duration::from_secs(30);
const FIND_TIMEOUT: Duration = Duration::from_secs(10);
const XCRUN: &str = "/usr/bin/xcrun";

#[derive(Debug, Default)]
pub struct SimctlRuntimes;

impl ToolManager for SimctlRuntimes {
    fn manager(&self) -> Manager {
        Manager::Simulators
    }

    fn inventory(&self) -> Result<ManagerInventory, InventoryFailure> {
        let Some(simctl) = simctl_program()? else {
            return ManagerInventory::try_new(Manager::Simulators, Executable::Missing, Vec::new());
        };
        let version = project_version(&run(&simctl, &["--version"], "simctl --version")?)?;
        let tools = runtimes(&run(
            &simctl,
            &["runtime", "list", "-j"],
            "simctl runtime list -j",
        )?)?;
        ManagerInventory::try_new(
            Manager::Simulators,
            Executable::Found {
                path: simctl,
                version,
            },
            tools,
        )
    }
}

fn simctl_program() -> Result<Option<PathBuf>, InventoryFailure> {
    match command_capture::classify(command_capture::path_override("ELSECALL_SIMCTL")) {
        command_capture::OverridePath::Absolute(path) => Ok(path.is_file().then_some(path)),
        command_capture::OverridePath::NotAbsolute => {
            Err(InventoryFailure::not_absolute("ELSECALL_SIMCTL"))
        }
        command_capture::OverridePath::Unset => found_by_xcrun(),
    }
}

/// `xcrun --find simctl`. A non-zero exit means no Xcode is selected.
fn found_by_xcrun() -> Result<Option<PathBuf>, InventoryFailure> {
    let captured = match command_capture::run(Program {
        program: Path::new(XCRUN),
        arguments: &["--find", "simctl"],
        directory: Path::new("/"),
        timeout: FIND_TIMEOUT,
        remove_env: &[],
    }) {
        Ok(captured) => captured,
        Err(command_capture::CaptureFailure::TimedOut) => {
            return Err(InventoryFailure::timeout("xcrun --find simctl"));
        }
        Err(_) => return Ok(None),
    };
    if !captured.status.success() {
        return Ok(None);
    }
    let path = std::str::from_utf8(&captured.stdout)
        .ok()
        .and_then(|text| text.lines().next())
        .map(str::trim)
        .filter(|line| line.starts_with('/'))
        .map(PathBuf::from)
        .ok_or_else(|| {
            InventoryFailure::output_invalid("xcrun --find simctl", "the path was not absolute")
        })?;
    Ok(Some(path))
}

fn run(program: &Path, arguments: &[&str], label: &str) -> Result<Vec<u8>, InventoryFailure> {
    match command_capture::run(Program {
        program,
        arguments,
        directory: Path::new("/"),
        timeout: TIMEOUT,
        remove_env: &[],
    }) {
        Ok(captured) if captured.status.success() => Ok(captured.stdout),
        Ok(captured) => Err(InventoryFailure::failed(&match captured.status.code() {
            Some(code) => format!("{label} exited with status {code}"),
            None => format!("{label} was stopped by a signal"),
        })),
        Err(command_capture::CaptureFailure::TimedOut) => Err(InventoryFailure::timeout(label)),
        Err(command_capture::CaptureFailure::TooLarge) => Err(InventoryFailure::output_invalid(
            label,
            "the output was larger than 8 MiB",
        )),
        Err(command_capture::CaptureFailure::Start(_)) => Err(InventoryFailure::failed(&format!(
            "{label} could not be started"
        ))),
    }
}

/// The version after `CoreSimulator-` in `simctl --version`.
fn project_version(stdout: &[u8]) -> Result<ToolVersion, InventoryFailure> {
    let invalid = || {
        InventoryFailure::output_invalid("simctl --version", "no CoreSimulator version was found")
    };
    let text = std::str::from_utf8(stdout).map_err(|_| invalid())?;
    let start = text.find("CoreSimulator-").ok_or_else(invalid)? + "CoreSimulator-".len();
    let token = text[start..]
        .split_whitespace()
        .next()
        .ok_or_else(invalid)?;
    ToolVersion::new(token).ok_or_else(invalid)
}

#[derive(Deserialize)]
struct RawRuntime {
    #[serde(rename = "platformIdentifier", default)]
    platform: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    build: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

fn runtimes(stdout: &[u8]) -> Result<Vec<InstalledTool>, InventoryFailure> {
    let listing: BTreeMap<String, RawRuntime> = serde_json::from_slice(stdout).map_err(|_| {
        InventoryFailure::output_invalid(
            "simctl runtime list -j",
            "it was not an object of runtimes",
        )
    })?;
    let mut tools = Vec::new();
    for runtime in listing.into_values() {
        if runtime.state.as_deref() != Some("Ready") {
            continue;
        }
        let Some(platform) = runtime.platform.as_deref().and_then(platform_name) else {
            continue;
        };
        let (Some(version), Some(build)) = (
            runtime.version.and_then(ToolVersion::new),
            runtime
                .build
                .filter(|build| ToolVersion::new(build.as_str()).is_some()),
        ) else {
            return Err(InventoryFailure::output_invalid(
                "simctl runtime list -j",
                "a runtime's version or build was empty or contained whitespace",
            ));
        };
        tools.push(InstalledTool {
            name: ToolName::new(platform).expect("platform names are tool names"),
            version,
            default: false,
            detail: Some(Detail::Runtime { build }),
        });
    }
    Ok(tools)
}

/// The platform `simctl` and `.elsecall-tools.toml` name for a simulator platform.
fn platform_name(identifier: &str) -> Option<&'static str> {
    match identifier {
        "com.apple.platform.iphonesimulator" => Some("iOS"),
        "com.apple.platform.appletvsimulator" => Some("tvOS"),
        "com.apple.platform.watchsimulator" => Some("watchOS"),
        "com.apple.platform.xrsimulator" => Some("visionOS"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::simulator_runtimes::*;

    #[test]
    fn the_version_is_the_core_simulator_project() {
        let version =
            project_version(b"@(#)PROGRAM:simctl  PROJECT:CoreSimulator-1051.17.7\n").unwrap();
        assert_eq!(version.as_str(), "1051.17.7");
        let error = project_version(b"simctl SECRET").unwrap_err();
        assert_eq!(error.code(), "tool_manager_output_invalid");
        assert!(!error.message().contains("SECRET"));
    }

    #[test]
    fn ready_runtimes_of_known_platforms_are_installed() {
        let listing = br#"{
          "A": {"platformIdentifier": "com.apple.platform.iphonesimulator", "version": "26.2",
                "build": "23C54", "state": "Ready", "kind": "Patchable Cryptex Disk Image"},
          "B": {"platformIdentifier": "com.apple.platform.watchsimulator", "version": "26.0",
                "build": "23R353", "state": "Ready"},
          "C": {"platformIdentifier": "com.apple.platform.iphonesimulator", "version": "18.6",
                "build": "22G86", "state": "Deleting"},
          "D": {"platformIdentifier": "com.apple.platform.macosx", "version": "1.0",
                "build": "1A1", "state": "Ready"},
          "E": {"platformIdentifier": "com.apple.platform.iphonesimulator", "state": "Downloading"}
        }"#;
        let tools = runtimes(listing).unwrap();
        let named: Vec<(&str, &str)> = tools
            .iter()
            .map(|tool| (tool.name.as_str(), tool.version.as_str()))
            .collect();
        assert_eq!(named, [("iOS", "26.2"), ("watchOS", "26.0")]);
        assert_eq!(
            tools[0].detail,
            Some(Detail::Runtime {
                build: "23C54".into()
            })
        );
        assert!(tools.iter().all(|tool| !tool.default));
    }

    #[test]
    fn a_listing_that_is_not_runtimes_is_invalid_output() {
        let ready_without_build =
            br#"{"A": {"platformIdentifier": "com.apple.platform.iphonesimulator",
                                             "version": "26.2", "state": "Ready"}}"#;
        for listing in [&b"[]"[..], ready_without_build, b"not json"] {
            let error = runtimes(listing).unwrap_err();
            assert_eq!(error.code(), "tool_manager_output_invalid");
        }
        let blank = br#"{"A": {"platformIdentifier": "com.apple.platform.iphonesimulator",
                               "version": "26 2", "build": "23C54", "state": "Ready"}}"#;
        assert!(runtimes(blank).is_err());
    }

    #[test]
    fn platforms_use_simctl_names() {
        assert_eq!(
            platform_name("com.apple.platform.appletvsimulator"),
            Some("tvOS")
        );
        assert_eq!(
            platform_name("com.apple.platform.xrsimulator"),
            Some("visionOS")
        );
        assert_eq!(platform_name("com.apple.platform.iphoneos"), None);
    }
}
