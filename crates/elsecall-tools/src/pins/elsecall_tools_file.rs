//! `.elsecall-tools.toml`: what a repository needs that no other convention can
//! say. Simulator runtimes come first:
//!
//! ```toml
//! [simulators]
//! runtimes = ["iOS 26.2"]
//! ```
//!
//! The schema is strict: unknown keys are refused, without echoing them, so a
//! pin another source owns (a mise tool, the Xcode version) cannot be repeated
//! here. Versions are exact from the start, since this file is Elsecall's own. `.elsecall.toml`, the
//! legacy workload file, is a different file and is not read.
use crate::pins::{
    Pin, PinSource, Platform, Reading, SourceFile, SourceKind, Target, dotted_version, invalid,
    source_file::read_workspace_file,
};
use elsecall_contract::failure::Result;
use std::path::Path;

const FILE: &str = ".elsecall-tools.toml";
const MAX_RUNTIMES: usize = 64;

pub(super) struct ElsecallToolsFile;

impl PinSource for ElsecallToolsFile {
    fn read(&self, workspace: &Path) -> Result<Reading> {
        let mut reading = Reading::default();
        let Some(text) = read_workspace_file(workspace, FILE)? else {
            return Ok(reading);
        };
        let source = SourceFile {
            kind: SourceKind::ElsecallTools,
            file: FILE.to_owned(),
        };
        for (platform, version) in runtimes(&text)? {
            reading.pins.push(Pin {
                target: Target::SimulatorRuntime(platform),
                version,
                source: source.clone(),
            });
        }
        reading.files.push(source);
        Ok(reading)
    }
}

fn runtimes(text: &str) -> Result<Vec<(Platform, crate::tool_inventory::ToolVersion)>> {
    let root: toml::Table =
        toml::from_str(text).map_err(|_| invalid(FILE, "", "the file must be TOML"))?;
    let mut found = Vec::new();
    for (key, value) in &root {
        if key != "simulators" {
            return Err(invalid(
                FILE,
                "",
                "an unknown key; the file holds only [simulators]",
            ));
        }
        let Some(table) = value.as_table() else {
            return Err(invalid(FILE, "simulators", "expected a table"));
        };
        for (key, value) in table {
            if key != "runtimes" {
                return Err(invalid(
                    FILE,
                    "simulators",
                    "an unknown key; [simulators] holds only runtimes",
                ));
            }
            let Some(items) = value.as_array() else {
                return Err(invalid(FILE, "simulators.runtimes", "expected a list"));
            };
            if items.len() > MAX_RUNTIMES {
                return Err(invalid(FILE, "simulators.runtimes", "at most 64 runtimes"));
            }
            for item in items {
                let runtime = item.as_str().and_then(runtime).ok_or_else(|| {
                    invalid(
                        FILE,
                        "simulators.runtimes",
                        "each runtime is a platform and an exact version, such as \"iOS 26.2\"; platforms are iOS, tvOS, visionOS and watchOS",
                    )
                })?;
                if found.contains(&runtime) {
                    return Err(invalid(
                        FILE,
                        "simulators.runtimes",
                        "a runtime is listed twice",
                    ));
                }
                found.push(runtime);
            }
        }
    }
    Ok(found)
}

fn runtime(text: &str) -> Option<(Platform, crate::tool_inventory::ToolVersion)> {
    let (platform, version) = text.split_once(' ')?;
    let platform = Platform::ALL
        .into_iter()
        .find(|candidate| candidate.as_str() == platform)?;
    Some((platform, dotted_version(version, 2)?))
}

#[cfg(test)]
mod tests {
    use crate::pins::elsecall_tools_file::*;
    use std::fs;

    fn read(text: &str) -> Result<Reading> {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::write(root.join(FILE), text).unwrap();
        ElsecallToolsFile.read(&root)
    }

    #[test]
    fn runtimes_are_simulator_pins() {
        let reading =
            read("[simulators]\nruntimes = [\"iOS 26.2\", \"watchOS 26.2\", \"iOS 18.6\"]\n")
                .unwrap();
        let pins: Vec<(&str, &str, &str)> = reading
            .pins
            .iter()
            .map(|pin| {
                (
                    pin.target.manager(),
                    pin.target.tool(),
                    pin.version.as_str(),
                )
            })
            .collect();
        assert_eq!(
            pins,
            [
                ("simulators", "iOS", "26.2"),
                ("simulators", "watchOS", "26.2"),
                ("simulators", "iOS", "18.6"),
            ]
        );
        assert_eq!(reading.files[0].kind, SourceKind::ElsecallTools);
        assert!(read("").unwrap().pins.is_empty());
        assert!(read("[simulators]\n").unwrap().pins.is_empty());
    }

    #[test]
    fn the_schema_is_strict_and_errors_never_echo_values() {
        for (text, message) in [
            (
                "[xcode]\nversion = \"26.2\"\n",
                ".elsecall-tools.toml: an unknown key; the file holds only [simulators].",
            ),
            (
                "[simulators]\nruntime = [\"iOS 26.2\"]\n",
                ".elsecall-tools.toml: simulators: an unknown key; [simulators] holds only runtimes.",
            ),
            (
                "[simulators]\nruntimes = [\"iOS 26.2\", \"iOS 26.2\"]\n",
                ".elsecall-tools.toml: simulators.runtimes: a runtime is listed twice.",
            ),
            (
                "[simulators]\nruntimes = \"iOS 26.2\"\n",
                ".elsecall-tools.toml: simulators.runtimes: expected a list.",
            ),
            (
                "simulators = 1",
                ".elsecall-tools.toml: simulators: expected a table.",
            ),
        ] {
            let error = read(text).unwrap_err();
            assert_eq!(error.code, "pin_source_invalid");
            assert_eq!(error.message, message);
        }
        for runtime in [
            "iOS 26",
            "ios 26.2",
            "iOS latest",
            "iOS  26.2",
            "SECRET 26.2",
            "iOS",
        ] {
            let error = read(&format!("[simulators]\nruntimes = [\"{runtime}\"]\n")).unwrap_err();
            assert!(error.message.contains("exact version"), "{runtime}");
            assert!(!error.message.contains("SECRET"));
        }
    }
}
