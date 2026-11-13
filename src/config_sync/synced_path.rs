//! Paths the fleet mirrors, written relative to home as `~/…`, and the rules that
//! keep them away from places mirroring must never write.
//!
//! The rules are lexical and case-insensitive, as APFS is: a path is refused when it
//! is home itself, or when it is, lies inside, or contains a forbidden location.
//! Components are printable ASCII, so case-insensitive comparison is exact: APFS
//! folds many non-ASCII letters onto ASCII ones (`ſ` onto `s`), which a lexical
//! rule could not follow. Members check the same rules again on arrival, and
//! against where the path really resolves on their disk (see `disk`).
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    path::{Path, PathBuf},
};

const MAXIMUM_LENGTH: usize = 1024;
const MAXIMUM_COMPONENT: usize = 255;

/// Locations no synced path may be, lie inside, or contain, relative to home.
pub const FORBIDDEN: &[&str] = &[
    ".ssh",
    ".gnupg",
    "Library/Keychains",
    "Library/LaunchAgents",
    "Library/Application Support/Elsecall",
    ".elsecall",
    ".local/bin",
];

/// A validated home-relative path, such as `~/.config/acme-ios`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SyncedPath {
    /// Components below home, `/`-separated, never empty.
    relative: String,
}

/// A component as a case-insensitive file system compares it. Synced paths are
/// ASCII; other locations (a workspace root) may not be, and are compared with
/// ASCII case folded only, which a synced path cannot spell differently.
fn folded(component: &str) -> String {
    component.to_ascii_lowercase()
}

fn folded_components(relative: &str) -> Vec<String> {
    relative.split('/').map(folded).collect()
}

/// Whether one component list is a prefix of the other: the paths overlap.
fn overlaps(a: &[String], b: &[String]) -> bool {
    a.iter().zip(b).all(|(x, y)| x == y)
}

impl SyncedPath {
    /// Parses `~/…`. The error says which rule the path breaks.
    pub fn parse(text: &str) -> Result<Self, String> {
        let Some(relative) = text.strip_prefix("~/") else {
            return Err(format!(
                "{text:?} must start with ~/ (paths are relative to each member's home)"
            ));
        };
        let relative = relative.trim_end_matches('/');
        if relative.is_empty() {
            return Err("home itself cannot be synced".into());
        }
        if relative.len() > MAXIMUM_LENGTH {
            return Err(format!("{text:?} is too long"));
        }
        if !relative.bytes().all(|b| (0x20..0x7f).contains(&b)) {
            return Err(format!(
                "{text:?} must be printable ASCII (file systems fold other letters onto ASCII ones)"
            ));
        }
        for component in relative.split('/') {
            if component.is_empty()
                || component == "."
                || component == ".."
                || component.len() > MAXIMUM_COMPONENT
                || folded(component).trim_end_matches(['.', ' ']).is_empty()
            {
                return Err(format!(
                    "{text:?} must be a plain path below home, without empty, . or .. components"
                ));
            }
        }
        let path = Self {
            relative: relative.to_owned(),
        };
        if let Some(forbidden) = path.forbidden_overlap(FORBIDDEN.iter().copied()) {
            return Err(format!(
                "{text:?} overlaps ~/{forbidden}, which Elsecall never syncs"
            ));
        }
        Ok(path)
    }

    /// The first of `locations` (home-relative) this path is, lies inside or contains.
    pub fn forbidden_overlap<'a>(
        &self,
        locations: impl IntoIterator<Item = &'a str>,
    ) -> Option<&'a str> {
        let own = folded_components(&self.relative);
        locations
            .into_iter()
            .find(|location| overlaps(&own, &folded_components(location)))
    }

    /// The forbidden location (fixed or among `extra`) that `relative`, a path below
    /// home as the file system really names it, is, lies inside or contains; `""`
    /// when it is home itself. It may be any text, since it comes from the disk.
    pub fn resolved_overlap(relative: &str, extra: &[String]) -> Option<String> {
        let relative = relative.trim_matches('/');
        if relative.is_empty() {
            return Some(String::new());
        }
        let own = folded_components(relative);
        FORBIDDEN
            .iter()
            .copied()
            .chain(extra.iter().map(String::as_str))
            .find(|location| overlaps(&own, &folded_components(location)))
            .map(str::to_owned)
    }

    /// Whether the two paths are the same or one contains the other.
    pub fn overlaps(&self, other: &SyncedPath) -> bool {
        overlaps(
            &folded_components(&self.relative),
            &folded_components(&other.relative),
        )
    }

    /// The components below home.
    pub fn relative(&self) -> &str {
        &self.relative
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.relative.split('/')
    }

    /// The last component.
    pub fn file_name(&self) -> &str {
        self.relative.rsplit('/').next().expect("never empty")
    }

    /// The components of the directory holding this path, below home.
    pub fn parent_components(&self) -> Vec<&str> {
        let mut all: Vec<&str> = self.components().collect();
        all.pop();
        all
    }

    /// Where this path lives under `home`.
    pub fn resolve(&self, home: &Path) -> PathBuf {
        home.join(&self.relative)
    }

    /// `~/relative/inner`, for naming a file inside this path in output.
    pub fn display_file(&self, inner: Option<&str>) -> String {
        match inner {
            Some(inner) => format!("~/{}/{inner}", self.relative),
            None => self.to_string(),
        }
    }
}

impl fmt::Display for SyncedPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "~/{}", self.relative)
    }
}
impl TryFrom<String> for SyncedPath {
    type Error = String;
    fn try_from(text: String) -> Result<Self, String> {
        Self::parse(&text)
    }
}
impl From<SyncedPath> for String {
    fn from(path: SyncedPath) -> String {
        path.to_string()
    }
}

/// Checks a declared set: valid paths, none overlapping another, at most `limit`.
pub fn check_set(paths: &[SyncedPath], limit: usize) -> Result<(), String> {
    if paths.len() > limit {
        return Err(format!("at most {limit} paths may be synced"));
    }
    for (index, path) in paths.iter().enumerate() {
        if let Some(other) = paths[index + 1..].iter().find(|o| o.overlaps(path)) {
            return Err(format!(
                "{path} and {other} overlap; declare only one of them"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_paths_below_home() {
        let path = SyncedPath::parse("~/.config/acme-ios").unwrap();
        assert_eq!(path.to_string(), "~/.config/acme-ios");
        assert_eq!(path.relative(), ".config/acme-ios");
        assert_eq!(path.file_name(), "acme-ios");
        assert_eq!(path.parent_components(), [".config"]);
        assert_eq!(
            SyncedPath::parse("~/.config/demo/").unwrap().to_string(),
            "~/.config/demo"
        );
        assert_eq!(
            path.display_file(Some("a/b.json")),
            "~/.config/acme-ios/a/b.json"
        );
        // Serialized as written, and validated when read back.
        let json = serde_json::to_string(&path).unwrap();
        assert_eq!(json, "\"~/.config/acme-ios\"");
        assert!(serde_json::from_str::<SyncedPath>("\"~/.ssh\"").is_err());
    }

    #[test]
    fn refuses_home_traversal_and_dangerous_locations() {
        for refused in [
            "~",
            "~/",
            "/etc/hosts",
            "relative",
            "~user/x",
            "~/a/../b",
            "~/./a",
            "~/a//b",
            "~/..",
            "~/...",
            "~/ .",
            "~/.ssh",
            "~/.SSH/config",
            "~/.ssh/known_hosts",
            "~/.gnupg",
            "~/Library",
            "~/library/keychains/login.keychain-db",
            "~/Library/LaunchAgents/x.plist",
            "~/Library/Application Support",
            "~/Library/Application Support/Elsecall/config.toml",
            "~/.elsecall",
            "~/.elsecall/sync-overrides/x",
            "~/.local",
            "~/.local/bin/elsecall",
            "~/.s\u{200b}sh",
            // APFS folds U+017F LATIN SMALL LETTER LONG S onto "s".
            "~/.\u{17f}sh",
            "~/Library/Application Support/El\u{17f}ecall",
            "~/.config/caf\u{e9}",
            "~/a\tb",
        ] {
            assert!(SyncedPath::parse(refused).is_err(), "{refused}");
        }
        // Neighbours of forbidden places are fine.
        for accepted in [
            "~/.ssh-config-backup",
            "~/Library/Preferences/x.plist",
            "~/.local/share/app",
        ] {
            assert!(SyncedPath::parse(accepted).is_ok(), "{accepted}");
        }
    }

    #[test]
    fn sets_refuse_overlaps_and_size() {
        let a = SyncedPath::parse("~/.config/a").unwrap();
        let inner = SyncedPath::parse("~/.CONFIG/a/b").unwrap();
        let b = SyncedPath::parse("~/.config/ab").unwrap();
        assert!(check_set(&[a.clone(), b.clone()], 16).is_ok());
        assert!(check_set(&[a.clone(), inner], 16).is_err());
        assert!(check_set(&[a.clone(), a.clone()], 16).is_err());
        assert!(check_set(&[a, b], 1).is_err());
        let root = SyncedPath::parse("~/workspace").unwrap();
        assert_eq!(
            root.forbidden_overlap(["workspace/copies"]),
            Some("workspace/copies")
        );
    }
}
