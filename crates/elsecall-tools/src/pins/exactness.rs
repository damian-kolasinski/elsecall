//! Whether a pin names one exact version, and what an inexact one resolves to.
//!
//! Elsecall installs exactly what is pinned, so a request that could mean
//! several versions is reported as inexact rather than guessed. For one, the
//! resolved version is what mise picks without the network: the newest version
//! installed on this machine that matches. Versions compare by the inventory's
//! rules (`tool_inventory::compare_versions`).
//!
//! - mise: `latest` is inexact and resolves to the newest installed version.
//!   Other selectors (`lts`, `system`, `ref:`, `path:`, `prefix:`, `sub-`) are
//!   inexact and not resolved. A plain version that an installed version
//!   extends (`22` when `22.19.0` is installed, `zulu-17`) is inexact and
//!   resolves to the newest such version. Any other plain version is exact.
//! - Xcode: two or more segments are exact (`26.2`, `26.0.1`), as Xcode
//!   releases are named. One segment (`26`) is inexact and resolves to the
//!   newest installed `26.x`.
//! - Simulator runtimes: always exact; their file accepts nothing else.
use crate::{
    pins::Target,
    tool_inventory::{ToolVersion, compare_versions, version_extends, version_selector},
};
use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Exactness {
    Exact,
    /// `resolved` is the newest installed match, when this machine has one.
    Inexact {
        resolved: Option<ToolVersion>,
    },
}

/// Classifies `version` for `target`, given the versions of that tool this
/// machine has installed.
pub(super) fn classify(
    target: &Target,
    version: &ToolVersion,
    installed: &[ToolVersion],
) -> Exactness {
    let text = version.as_str();
    match target {
        Target::Mise(_) if text == "latest" => Exactness::Inexact {
            resolved: newest(installed.iter()),
        },
        Target::Mise(_) if version_selector(text) => Exactness::Inexact { resolved: None },
        Target::Mise(_) => {
            let same = installed
                .iter()
                .any(|candidate| compare_versions(candidate.as_str(), text) == Ordering::Equal);
            let extending = installed
                .iter()
                .filter(|candidate| version_extends(candidate.as_str(), text));
            match newest(extending) {
                Some(resolved) if !same => Exactness::Inexact {
                    resolved: Some(resolved),
                },
                _ => Exactness::Exact,
            }
        }
        Target::Xcode if !text.contains('.') => Exactness::Inexact {
            resolved: newest(
                installed
                    .iter()
                    .filter(|candidate| version_extends(candidate.as_str(), text)),
            ),
        },
        Target::Xcode | Target::SimulatorRuntime(_) => Exactness::Exact,
    }
}

fn newest<'a>(versions: impl Iterator<Item = &'a ToolVersion>) -> Option<ToolVersion> {
    versions
        .max_by(|left, right| compare_versions(left.as_str(), right.as_str()))
        .cloned()
}

#[cfg(test)]
mod tests {
    use crate::{
        pins::{Platform, exactness::*},
        tool_inventory::ToolName,
    };

    fn versions(values: &[&str]) -> Vec<ToolVersion> {
        values
            .iter()
            .map(|value| ToolVersion::new(*value).unwrap())
            .collect()
    }

    fn mise(version: &str, installed: &[&str]) -> Exactness {
        classify(
            &Target::Mise(ToolName::new("node").unwrap()),
            &ToolVersion::new(version).unwrap(),
            &versions(installed),
        )
    }

    fn inexact(resolved: Option<&str>) -> Exactness {
        Exactness::Inexact {
            resolved: resolved.map(|value| ToolVersion::new(value).unwrap()),
        }
    }

    #[test]
    fn latest_resolves_to_the_newest_installed_version() {
        assert_eq!(
            mise("latest", &["1.2.13", "1.3.14", "1.3.5"]),
            inexact(Some("1.3.14"))
        );
        assert_eq!(mise("latest", &[]), inexact(None));
    }

    #[test]
    fn other_selectors_are_inexact_and_unresolved() {
        for selector in [
            "lts",
            "system",
            "ref:main",
            "path:/opt/node",
            "prefix:22",
            "sub-1:latest",
        ] {
            assert_eq!(mise(selector, &["22.19.0"]), inexact(None), "{selector}");
        }
    }

    #[test]
    fn a_partial_version_resolves_to_the_newest_installed_match() {
        let installed = ["20.19.5", "22.16.0", "22.19.0", "24.0.1"];
        assert_eq!(mise("22", &installed), inexact(Some("22.19.0")));
        assert_eq!(mise("22.16", &installed), inexact(Some("22.16.0")));
        assert_eq!(
            mise("zulu-17", &["zulu-17.54.21", "openjdk-20"]),
            inexact(Some("zulu-17.54.21"))
        );
    }

    #[test]
    fn a_plain_version_is_exact_unless_an_installed_one_extends_it() {
        assert_eq!(mise("22.19.0", &["22.19.0"]), Exactness::Exact);
        assert_eq!(mise("22.19.0", &[]), Exactness::Exact);
        assert_eq!(mise("22", &[]), Exactness::Exact);
        assert_eq!(mise("1.7", &["1.7", "1.7.1"]), Exactness::Exact);
        assert_eq!(mise("22.19.0", &["22.19.01"]), Exactness::Exact);
    }

    #[test]
    fn xcode_versions_follow_release_names() {
        let installed = versions(&["16.4", "26.0.1", "26.2"]);
        let xcode = |version: &str| {
            classify(
                &Target::Xcode,
                &ToolVersion::new(version).unwrap(),
                &installed,
            )
        };
        assert_eq!(xcode("26.2"), Exactness::Exact);
        assert_eq!(xcode("26.3"), Exactness::Exact);
        assert_eq!(xcode("26.0"), Exactness::Exact);
        assert_eq!(xcode("26"), inexact(Some("26.2")));
        assert_eq!(xcode("27"), inexact(None));
        let runtime = classify(
            &Target::SimulatorRuntime(Platform::Ios),
            &ToolVersion::new("26.2").unwrap(),
            &[],
        );
        assert_eq!(runtime, Exactness::Exact);
    }
}
