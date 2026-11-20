//! When two files pin one tool.
//!
//! Two workspace files that pin the same target (a mise tool in one mise
//! environment, Xcode, a runtime) conflict, unless both are mise project files
//! asking for the same versions with the same install options, which is one pin
//! credited to the first. A
//! conflict is never settled by precedence: a reader of either file alone would
//! be misled. The master's global config is a layer under the project, as mise
//! treats it, so a global pin never conflicts with a project one; both stay.
use crate::{
    pins::{InstallOptions, Pin, SourceFile, SourceKind, Target},
    tool_inventory::ToolVersion,
};
use elsecall_contract::failure::{Failure, Result};
use std::collections::BTreeMap;

/// `pins` without repeated identical project pins, or the first conflict.
pub(super) fn without_conflicts(pins: Vec<Pin>) -> Result<Vec<Pin>> {
    type Requested<'a> = Vec<(&'a ToolVersion, &'a InstallOptions)>;
    let mut groups: BTreeMap<&Target, Vec<(&SourceFile, Requested)>> = BTreeMap::new();
    for pin in pins.iter().filter(|pin| !global(pin)) {
        let files = groups.entry(&pin.target).or_default();
        match files.iter_mut().find(|(source, _)| *source == &pin.source) {
            Some((_, versions)) => versions.push((&pin.version, &pin.options)),
            None => files.push((&pin.source, vec![(&pin.version, &pin.options)])),
        }
    }
    let mut owners: BTreeMap<Target, SourceFile> = BTreeMap::new();
    for (target, files) in &groups {
        let (first, first_versions) = &files[0];
        for (other, versions) in &files[1..] {
            let repeated = first.kind == SourceKind::MiseProject
                && other.kind == SourceKind::MiseProject
                && versions == first_versions;
            if !repeated {
                return Err(conflict(target, first, other));
            }
        }
        owners.insert((*target).clone(), (*first).clone());
    }
    Ok(pins
        .into_iter()
        .filter(|pin| global(pin) || owners.get(&pin.target) == Some(&pin.source))
        .collect())
}

fn global(pin: &Pin) -> bool {
    pin.source.kind == SourceKind::MiseGlobal
}

fn conflict(target: &Target, first: &SourceFile, second: &SourceFile) -> Failure {
    let environment = target
        .environment()
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

#[cfg(test)]
mod tests {
    use crate::{
        pins::{Environment, conflicts::*},
        tool_inventory::ToolName,
    };

    fn pin(kind: SourceKind, file: &str, target: Target, version: &str) -> Pin {
        Pin {
            target,
            version: ToolVersion::new(version).unwrap(),
            source: SourceFile {
                kind,
                file: file.to_owned(),
            },
            options: InstallOptions::default(),
        }
    }

    fn node(environment: Option<&str>) -> Target {
        Target::Mise {
            tool: ToolName::new("node").unwrap(),
            environment: environment.map(|value| Environment::new(value).unwrap()),
        }
    }

    fn project(file: &str, version: &str) -> Pin {
        pin(SourceKind::MiseProject, file, node(None), version)
    }

    fn files(pins: &[Pin]) -> Vec<&str> {
        pins.iter().map(|pin| pin.source.file.as_str()).collect()
    }

    #[test]
    fn identical_project_pins_are_one_from_the_first_file() {
        let kept = without_conflicts(vec![
            project(".tool-versions", "22.19.0"),
            project(".tool-versions", "20.19.5"),
            project("mise.toml", "22.19.0"),
            project("mise.toml", "20.19.5"),
        ])
        .unwrap();
        assert_eq!(files(&kept), [".tool-versions", ".tool-versions"]);
    }

    #[test]
    fn the_same_version_with_other_install_options_conflicts() {
        let table: toml::Table = toml::from_str("version = \"22.19.0\"\nbin = \"node\"").unwrap();
        let mut optioned = project("mise.toml", "22.19.0");
        optioned.options =
            InstallOptions::from_table("mise.toml", "tools.node", &table, &Default::default())
                .unwrap();
        let error =
            without_conflicts(vec![project(".tool-versions", "22.19.0"), optioned]).unwrap_err();
        assert_eq!(error.code, "pins_conflict");
    }

    #[test]
    fn different_project_pins_conflict_without_their_values() {
        let cases: [(&[&str], &[&str]); 3] = [
            (&["22.19.0"], &["24.0.1"]),
            (&["22.19.0"], &["22.19.0", "20.19.5"]),
            (&["22.19.0", "20.19.5"], &["20.19.5", "22.19.0"]),
        ];
        for (first, second) in cases {
            let mut pins: Vec<Pin> = first
                .iter()
                .map(|version| project(".tool-versions", version))
                .collect();
            pins.extend(second.iter().map(|version| project("mise.toml", version)));
            let error = without_conflicts(pins).unwrap_err();
            assert_eq!(error.code, "pins_conflict");
            assert_eq!(
                error.message,
                "mise node is pinned in both .tool-versions and mise.toml."
            );
            assert!(!error.message.contains("22.19.0"));
        }
    }

    #[test]
    fn environments_and_the_global_layer_are_separate() {
        let kept = without_conflicts(vec![
            project("mise.toml", "22.19.0"),
            pin(
                SourceKind::MiseProject,
                "mise.ci.toml",
                node(Some("ci")),
                "24.0.1",
            ),
            pin(
                SourceKind::MiseGlobal,
                "~/.config/mise/config.toml",
                node(None),
                "20.19.5",
            ),
        ])
        .unwrap();
        assert_eq!(kept.len(), 3);

        let error = without_conflicts(vec![
            pin(
                SourceKind::MiseProject,
                "mise.ci.toml",
                node(Some("ci")),
                "24.0.1",
            ),
            pin(
                SourceKind::MiseProject,
                ".mise.ci.toml",
                node(Some("ci")),
                "24.0.2",
            ),
        ])
        .unwrap_err();
        assert_eq!(
            error.message,
            "mise node is pinned in both mise.ci.toml and .mise.ci.toml for MISE_ENV=ci."
        );
    }

    #[test]
    fn another_source_repeating_a_pin_conflicts_even_when_equal() {
        let error = without_conflicts(vec![
            pin(
                SourceKind::XcodeVersion,
                ".xcode-version",
                Target::Xcode,
                "26.2",
            ),
            pin(
                SourceKind::ElsecallTools,
                ".elsecall-tools.toml",
                Target::Xcode,
                "26.2",
            ),
        ])
        .unwrap_err();
        assert_eq!(error.code, "pins_conflict");
        assert_eq!(
            error.message,
            "xcodes xcode is pinned in both .xcode-version and .elsecall-tools.toml."
        );
    }
}
