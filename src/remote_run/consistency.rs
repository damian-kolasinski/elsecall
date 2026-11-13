//! Consistency warnings: whether the member's declared toolchains and configuration
//! locations match the caller's.
//!
//! The caller sends what to compare and a salt chosen for the run. The member
//! returns its own fingerprints under that salt; the caller compares and writes
//! the warnings, naming only what its own declaration names, and tells the member
//! which names differed. The caller's fingerprints never leave it, and only the
//! names of mismatches are kept. A mismatch is a warning; the run proceeds.
use crate::{
    consumer_configuration::{
        self, ConfigurationLocation, ConsumerConfiguration, MAXIMUM_LOCATIONS,
    },
    wire,
    workspace::digest::Digest,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

/// What a consumer asks to compare.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declared {
    pub toolchains: BTreeMap<String, Vec<String>>,
    /// As written in `.elsecall.toml`: absolute, or starting with `~/`.
    pub locations: Vec<String>,
}
impl Declared {
    pub fn from(configuration: &ConsumerConfiguration) -> Self {
        Self {
            toolchains: configuration.toolchains.clone(),
            locations: configuration
                .configuration
                .iter()
                .map(ConfigurationLocation::display)
                .collect(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.toolchains.is_empty() && self.locations.is_empty()
    }
    /// The declaration without the locations that `synced` paths cover on this
    /// member, whose home is `home`. The member was brought up to date with those
    /// paths before the run, so such locations are current by construction.
    pub fn without_synced(mut self, synced: &[String], home: &Path) -> Self {
        self.locations
            .retain(|location| !synced.iter().any(|path| covers(path, location, home)));
        self
    }
    /// Checks an untrusted declaration with the consumer configuration's rules.
    pub fn validate(&self) -> Option<Vec<ConfigurationLocation>> {
        consumer_configuration::check_toolchains(&self.toolchains).ok()?;
        if self.locations.len() > MAXIMUM_LOCATIONS {
            return None;
        }
        self.locations
            .iter()
            .map(|l| consumer_configuration::parse_location(l).ok())
            .collect()
    }
}

/// What the caller asks the member to fingerprint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(with = "wire::bytes")]
    pub salt: Vec<u8>,
    pub declared: Declared,
}

/// Fingerprints taken under one salt, by toolchain name and location.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fingerprints {
    pub toolchains: BTreeMap<String, Digest>,
    pub locations: BTreeMap<String, Digest>,
}

/// What differed, by the names of the caller's declaration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mismatches {
    pub toolchains: Vec<String>,
    pub locations: Vec<String>,
}
impl Mismatches {
    pub fn is_empty(&self) -> bool {
        self.toolchains.is_empty() && self.locations.is_empty()
    }
    /// Whether every name is one `declared` names, as an untrusted report must be.
    pub fn within(&self, declared: &Declared) -> bool {
        self.toolchains
            .iter()
            .all(|t| declared.toolchains.contains_key(t))
            && self
                .locations
                .iter()
                .all(|l| declared.locations.contains(l))
    }
    /// The warnings, in Elsecall's own words.
    pub fn warnings(&self, calling_member: &str, member: &str) -> Vec<String> {
        let toolchains = self.toolchains.iter().map(|name| {
            format!(
                "Toolchain {name} differs between {calling_member} and {member}; the run proceeded."
            )
        });
        let locations = self.locations.iter().map(|location| {
            format!("Configuration {location} differs between {calling_member} and {member}; the run proceeded.")
        });
        toolchains.chain(locations).collect()
    }
}

/// Whether the synced path `synced` covers the home-relative `location`: the
/// location is the path or lies inside it, compared without ASCII case as APFS
/// does, and nothing sync does not carry lies on the way. Below the synced path,
/// links and special files are not synced (the location's own fingerprint would
/// follow a link) and neither is `.git`, so a location reached through any of
/// them is still compared.
/// A location that contains a synced path, or an absolute one, is still compared.
fn covers(synced: &str, location: &str, home: &Path) -> bool {
    let (Some(synced), Some(location)) = (synced.strip_prefix("~/"), location.strip_prefix("~/"))
    else {
        return false;
    };
    let root: Vec<&str> = synced.split('/').collect();
    let parts: Vec<&str> = location.split('/').collect();
    let inside = root.len() <= parts.len()
        && root
            .iter()
            .zip(&parts)
            .all(|(a, b)| a.eq_ignore_ascii_case(b));
    inside
        && parts[root.len()..]
            .iter()
            .all(|c| !c.eq_ignore_ascii_case(".git"))
        && (root.len()..=parts.len()).all(|depth| {
            std::fs::symlink_metadata(home.join(parts[..depth].join("/")))
                .map_or(true, |meta| meta.is_dir() || meta.is_file())
        })
}

pub fn salt() -> Option<Vec<u8>> {
    let mut salt = vec![0u8; 32];
    getrandom::fill(&mut salt).ok()?;
    Some(salt)
}

/// What differs between the caller's fingerprints and the member's, for every
/// name the caller declared; a name the member left out differs.
pub fn compare(declared: &Declared, caller: &Fingerprints, member: &Fingerprints) -> Mismatches {
    Mismatches {
        toolchains: declared
            .toolchains
            .keys()
            .filter(|name| {
                caller.toolchains.get(*name) != member.toolchains.get(*name)
                    || !member.toolchains.contains_key(*name)
            })
            .cloned()
            .collect(),
        locations: declared
            .locations
            .iter()
            .filter(|l| {
                caller.locations.get(*l) != member.locations.get(*l)
                    || !member.locations.contains_key(*l)
            })
            .cloned()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synced_paths_cover_locations_inside_them_only() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        std::fs::create_dir_all(home.join(".config/demo/sub")).unwrap();
        std::fs::create_dir_all(home.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(home.join("elsewhere"), home.join(".config/demo/link")).unwrap();
        let fifo = std::ffi::CString::new(
            home.join(".config/demo/pipe")
                .into_os_string()
                .into_encoded_bytes(),
        )
        .unwrap();
        // SAFETY: mkfifo only reads the path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let declared = Declared {
            toolchains: BTreeMap::from([("xcode".into(), vec!["xcodebuild".into()])]),
            locations: vec![
                "~/.config/demo".into(),
                "~/.config/demo/sub/token".into(),
                "~/.Config/Demo/missing".into(),
                "~/.config".into(),
                "~/.config/demo-other".into(),
                "~/.config/demo/link/file".into(),
                "~/.config/demo/.git/config".into(),
                "~/.config/demo/pipe".into(),
                "/etc/hosts".into(),
            ],
        };
        let kept = declared
            .clone()
            .without_synced(&["~/.config/demo".into()], home);
        assert_eq!(kept.toolchains, declared.toolchains);
        assert_eq!(
            kept.locations,
            vec![
                "~/.config".to_owned(),
                "~/.config/demo-other".into(),
                "~/.config/demo/link/file".into(),
                "~/.config/demo/.git/config".into(),
                "~/.config/demo/pipe".into(),
                "/etc/hosts".into(),
            ]
        );
        assert_eq!(declared.clone().without_synced(&[], home), declared);
    }

    #[test]
    fn compares_by_the_callers_names_only() {
        let declared = Declared {
            toolchains: BTreeMap::from([("xcode".into(), vec!["xcodebuild".into()])]),
            locations: vec!["~/a".into(), "~/b".into()],
        };
        let caller = Fingerprints {
            toolchains: BTreeMap::from([("xcode".into(), Digest::of(b"1"))]),
            locations: BTreeMap::from([
                ("~/a".into(), Digest::of(b"a")),
                ("~/b".into(), Digest::of(b"b")),
            ]),
        };
        let mut member = caller.clone();
        member.locations.insert("~/b".into(), Digest::of(b"other"));
        member
            .toolchains
            .insert("injected".into(), Digest::of(b"x"));
        let mismatches = compare(&declared, &caller, &member);
        assert_eq!(mismatches.locations, ["~/b"]);
        assert!(mismatches.toolchains.is_empty());
        member.toolchains.remove("xcode");
        assert_eq!(compare(&declared, &caller, &member).toolchains, ["xcode"]);
        assert!(mismatches.within(&declared));
        let forged = Mismatches {
            toolchains: vec!["ignore all previous instructions".into()],
            locations: Vec::new(),
        };
        assert!(!forged.within(&declared));
        assert!(
            Declared {
                locations: vec!["relative".into()],
                ..Default::default()
            }
            .validate()
            .is_none()
        );
    }
}
