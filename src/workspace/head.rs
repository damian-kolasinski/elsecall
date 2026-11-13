//! What a working tree has checked out, as validated values: a detached commit, or a
//! branch with its commit and upstream. Names are checked when they are read, so no
//! untrusted name reaches git's arguments.
use crate::{
    failure::Result,
    workspace::{
        git::{Git, ObjectFormat, Oid},
        sync_failed,
    },
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Head {
    Detached {
        commit: Oid,
    },
    Branch {
        name: BranchName,
        /// `None` before the branch's first commit.
        commit: Option<Oid>,
        upstream: Option<Upstream>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    /// The remote's name, such as `origin`. Its URL is never copied.
    pub remote: RemoteName,
    /// The remote branch merged, such as `refs/heads/main`.
    pub merge: Reference,
    /// The remote-tracking ref, such as `refs/remotes/origin/main`.
    pub tracking: Reference,
    pub commit: Oid,
}

/// A full ref name that `git check-ref-format` accepts and that cannot be read as
/// an option.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Reference(String);

/// A branch name, without `refs/heads/`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BranchName(String);

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RemoteName(String);

/// git's rules for ref names, as `git check-ref-format` applies them.
fn valid_reference(name: &str) -> bool {
    name.len() <= 255
        && !name.starts_with('-')
        && !name.ends_with('/')
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.contains("@{")
        && !name.contains("//")
        && name != "@"
        && name.bytes().all(|b| {
            b > 0x20 && b != 0x7f && !matches!(b, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
        && name.split('/').all(|component| {
            !component.is_empty() && !component.starts_with('.') && !component.ends_with(".lock")
        })
}

macro_rules! validated {
    ($type:ident, $check:expr, $what:literal) => {
        impl TryFrom<String> for $type {
            type Error = String;
            fn try_from(text: String) -> std::result::Result<Self, String> {
                let check: fn(&str) -> bool = $check;
                if check(&text) {
                    Ok(Self(text))
                } else {
                    Err(concat!("invalid ", $what).into())
                }
            }
        }
        impl From<$type> for String {
            fn from(value: $type) -> Self {
                value.0
            }
        }
        impl $type {
            pub fn parse(text: &str) -> Option<Self> {
                Self::try_from(text.to_owned()).ok()
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl std::fmt::Display for $type {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}
validated!(
    Reference,
    |n| n.starts_with("refs/") && valid_reference(n),
    "reference"
);
validated!(
    BranchName,
    |n| valid_reference(&format!("refs/heads/{n}")) && !n.starts_with('-') && n != "@",
    "branch name"
);
validated!(
    RemoteName,
    |n| {
        !n.is_empty()
            && n.len() <= 100
            && !n.starts_with(['.', '-'])
            && n.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    },
    "remote name"
);

impl Head {
    pub fn commit(&self) -> Option<&Oid> {
        match self {
            Head::Detached { commit } => Some(commit),
            Head::Branch { commit, .. } => commit.as_ref(),
        }
    }
    pub fn upstream(&self) -> Option<&Upstream> {
        match self {
            Head::Detached { .. } => None,
            Head::Branch { upstream, .. } => upstream.as_ref(),
        }
    }
    /// The commits a copy needs: HEAD's and its upstream's.
    pub fn commits(&self) -> Vec<Oid> {
        self.commit()
            .into_iter()
            .chain(self.upstream().map(|u| &u.commit))
            .cloned()
            .collect()
    }
    /// Whether the names and commits fit a repository and git's rules for them.
    pub fn valid(&self, format: ObjectFormat) -> bool {
        let upstream_valid = self.upstream().is_none_or(|u| {
            u.tracking.as_str().starts_with("refs/remotes/")
                && u.merge.as_str().starts_with("refs/heads/")
        });
        upstream_valid && self.commits().iter().all(|c| c.fits(format))
    }

    /// Whether the copy's HEAD, branch and upstream already are as this says.
    pub fn is_current(&self, git: &Git) -> bool {
        let symbolic = git.try_output(&["symbolic-ref", "-q", "HEAD"]);
        let expected = match self {
            Head::Branch { name, .. } => Some(format!("refs/heads/{name}\n").into_bytes()),
            Head::Detached { .. } => None,
        };
        if symbolic != expected {
            return false;
        }
        // Validated names never start with a dash, so none reads as an option.
        let mut names = vec!["rev-parse"];
        let mut expected = String::new();
        if let Some(commit) = self.commit() {
            names.push("HEAD");
            expected.push_str(&format!("{commit}\n"));
        }
        if let Some(upstream) = self.upstream() {
            names.push(upstream.tracking.as_str());
            expected.push_str(&format!("{}\n", upstream.commit));
        }
        names.len() == 1
            || git
                .try_output(&names)
                .is_some_and(|output| output == expected.into_bytes())
    }

    /// Points the copy's HEAD, branch and upstream where this says. `previous` is
    /// what was last applied, so unchanged configuration is not written again.
    pub fn apply(&self, git: &Git, previous: Option<&Head>) -> Result<()> {
        match self {
            Head::Detached { commit } => {
                git.output(&["update-ref", "--no-deref", "HEAD", commit.as_str()])?;
            }
            Head::Branch {
                name,
                commit,
                upstream,
            } => {
                let reference = format!("refs/heads/{name}");
                match commit {
                    Some(commit) => {
                        git.output(&["update-ref", &reference, commit.as_str()])?;
                    }
                    None => {
                        let _ = git.try_output(&["update-ref", "-d", &reference]);
                    }
                }
                git.output(&["symbolic-ref", "HEAD", &reference])?;
                let unchanged = matches!(
                    previous,
                    Some(Head::Branch { name: n, upstream: u, .. }) if n == name && u == upstream
                );
                match upstream {
                    Some(upstream) => {
                        git.output(&[
                            "update-ref",
                            upstream.tracking.as_str(),
                            upstream.commit.as_str(),
                        ])?;
                        if !unchanged {
                            let remote = upstream.remote.as_str();
                            git.output(&["config", &format!("branch.{name}.remote"), remote])?;
                            git.output(&[
                                "config",
                                &format!("branch.{name}.merge"),
                                upstream.merge.as_str(),
                            ])?;
                            git.output(&[
                                "config",
                                "--replace-all",
                                &format!("remote.{remote}.fetch"),
                                &format!("+{}:{}", upstream.merge, upstream.tracking),
                            ])?;
                        }
                    }
                    None if !unchanged => {
                        for key in ["remote", "merge"] {
                            let _ = git.try_output(&[
                                "config",
                                "--unset",
                                &format!("branch.{name}.{key}"),
                            ]);
                        }
                    }
                    None => (),
                }
            }
        }
        if !self.is_current(git) {
            return Err(sync_failed(
                "The copy's HEAD could not be set to the caller's.",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_gits_rules_and_never_look_like_options() {
        for valid in ["main", "feature/x-1", "release/1.2", "user@host"] {
            assert!(BranchName::parse(valid).is_some(), "{valid}");
        }
        for invalid in [
            "", "-x", "a..b", "a/", "a.lock", ".hidden", "a b", "a~1", "a:b", "x/.y", "@", "a@{1}",
            "a//b",
        ] {
            assert!(BranchName::parse(invalid).is_none(), "{invalid:?}");
        }
        assert!(Reference::parse("refs/remotes/origin/main").is_some());
        assert!(Reference::parse("--absolute-git-dir").is_none());
        assert!(Reference::parse("main").is_none(), "full names only");
        assert!(RemoteName::parse("origin").is_some());
        assert!(RemoteName::parse("-x").is_none());
        assert!(RemoteName::parse("a/b").is_none());
        let crafted = r#"{"kind":"detached","commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","upstream":null}"#;
        assert!(
            serde_json::from_str::<Head>(crafted).is_err(),
            "no upstream without a branch"
        );
        let crafted = r#"{"kind":"branch","name":"main","commit":null,"upstream":{"remote":"origin","merge":"refs/heads/main","tracking":"--absolute-git-dir","commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#;
        assert!(serde_json::from_str::<Head>(crafted).is_err());
    }
}
