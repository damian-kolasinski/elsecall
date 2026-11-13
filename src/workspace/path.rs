//! Paths inside a workspace, validated once so the rest of the code can trust them.
use crate::failure::{Failure, Result};
use serde::{Deserialize, Serialize};
use std::fmt;

const MAXIMUM_LENGTH: usize = 4096;
const MAXIMUM_COMPONENT: usize = 255;

/// A normalized relative path inside a workspace: `/`-separated components that are
/// never empty, `.`, `..` or git's own directory, in any case or with characters
/// macOS ignores. It therefore cannot name anything outside the workspace or inside
/// its git directory, however it is joined.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WorkspacePath(String);

/// Code points HFS+ and APFS lookups may ignore, which git also refuses around `.git`.
fn ignorable(c: char) -> bool {
    matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{206a}'..='\u{206f}' | '\u{feff}')
}

fn is_git_directory(component: &str) -> bool {
    let visible: String = component.chars().filter(|c| !ignorable(*c)).collect();
    let trimmed = visible.trim_end_matches([' ', '.']);
    trimmed.eq_ignore_ascii_case(".git") || trimmed.eq_ignore_ascii_case("git~1")
}

impl WorkspacePath {
    pub fn parse(text: &str) -> Option<Self> {
        if text.is_empty() || text.len() > MAXIMUM_LENGTH || text.contains('\0') {
            return None;
        }
        let valid = text.split('/').all(|component| {
            !component.is_empty()
                && component != "."
                && component != ".."
                && component.len() <= MAXIMUM_COMPONENT
                && !is_git_directory(component)
        });
        valid.then(|| Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// The directory holding this path, or `None` at the workspace root.
    pub fn parent(&self) -> Option<WorkspacePath> {
        self.0
            .rsplit_once('/')
            .map(|(parent, _)| Self(parent.into()))
    }

    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().expect("paths have a component")
    }

    pub fn join(&self, name: &str) -> Option<WorkspacePath> {
        Self::parse(&format!("{}/{name}", self.0))
    }

    /// Whether `self` is `other` or lies inside it.
    pub fn is_within(&self, other: &WorkspacePath) -> bool {
        self.0 == other.0
            || (self.0.len() > other.0.len()
                && self.0.starts_with(&other.0)
                && self.0.as_bytes()[other.0.len()] == b'/')
    }
}

impl TryFrom<String> for WorkspacePath {
    type Error = Failure;
    fn try_from(text: String) -> Result<Self> {
        Self::parse(&text).ok_or_else(|| {
            Failure::new(
                "invalid_request",
                "A workspace path must be relative, normalized and outside .git.",
                "Use a path such as build/logs inside the working tree.",
            )
        })
    }
}
impl From<WorkspacePath> for String {
    fn from(path: WorkspacePath) -> Self {
        path.0
    }
}
impl fmt::Display for WorkspacePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_normalized_paths_outside_git() {
        for valid in [
            "a",
            "a/b.txt",
            ".github/x",
            "build/.gitkeep",
            "x/.gitignore",
            "a b/c",
        ] {
            assert!(WorkspacePath::parse(valid).is_some(), "{valid}");
        }
        for invalid in [
            "",
            "/etc/passwd",
            "a/",
            "a//b",
            "./a",
            "a/../b",
            "..",
            ".git",
            ".git/hooks/pre-commit",
            "x/.GIT/config",
            "x/.Git",
            "x/.git.",
            "x/.git ",
            "x/.g\u{200c}it/config",
            "GIT~1/config",
            "a\0b",
        ] {
            assert!(WorkspacePath::parse(invalid).is_none(), "{invalid:?}");
        }
        assert!(WorkspacePath::parse(&"a".repeat(256)).is_none());
    }

    #[test]
    fn relates_paths() {
        let path = WorkspacePath::parse("build/logs/test.log").unwrap();
        let build = WorkspacePath::parse("build").unwrap();
        assert!(path.is_within(&build));
        assert!(build.is_within(&build));
        assert!(!WorkspacePath::parse("buildx/a").unwrap().is_within(&build));
        assert_eq!(path.parent().unwrap().as_str(), "build/logs");
        assert_eq!(path.file_name(), "test.log");
        assert!(build.parent().is_none());
    }
}
