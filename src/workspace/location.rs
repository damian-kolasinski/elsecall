//! Where a member keeps its copy of a caller's workspace.
//!
//! A **mapped** copy lives under the member's workspace root, at a place derived
//! from the calling member and the caller's absolute path, so the same workspace
//! always lands in the same, warm copy. A **same-path** copy lives at the caller's
//! absolute path. Either way, each caller repository's history is kept once, in an
//! object store under the workspace root that all its worktrees' copies share.
use crate::{
    failure::{Failure, Result},
    workspace::{digest::Digest, sync_failed},
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Setting {
    #[default]
    Mapped,
    SamePath,
}

/// A member's workspace settings.
#[derive(Clone, Debug)]
pub struct Locations {
    pub setting: Setting,
    /// Mapped copies and object stores live here.
    pub root: PathBuf,
    /// Same-path copies live at the caller's path under this directory: `/` in
    /// production; tests on one machine move it aside.
    pub same_path_base: PathBuf,
    /// Where copies' records are kept (inside the private state directory).
    pub state: PathBuf,
}

/// One copy's places.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyLocation {
    pub setting: Setting,
    pub root: PathBuf,
    /// The shared object store of the caller's repository.
    pub store: PathBuf,
    pub state_file: PathBuf,
    pub calling_member: String,
    pub caller_root: String,
}

/// A caller's absolute path, validated as untrusted input.
fn caller_path(text: &str) -> Result<&Path> {
    let path = Path::new(text);
    let normal = text.len() <= 1024
        && !text.contains('\0')
        && text.len() > 1
        && text.starts_with('/')
        && text[1..]
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..");
    if normal {
        Ok(path)
    } else {
        Err(Failure::new(
            "invalid_request",
            "The caller's workspace path must be absolute and normalized.",
            "Install the same Elsecall version on every member.",
        ))
    }
}

/// A readable, file-name-safe form of the workspace's last component.
fn label(root: &Path) -> String {
    let name: String = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let name = name.trim_matches(['-', '.']);
    if name.is_empty() {
        "workspace".into()
    } else {
        name.into()
    }
}

fn short(text: &str) -> String {
    Digest::of(text.as_bytes()).hex()[..16].to_owned()
}

impl Locations {
    pub fn locate(
        &self,
        calling_member: &str,
        caller_root: &str,
        common_dir: &str,
    ) -> Result<CopyLocation> {
        let root = caller_path(caller_root)?;
        caller_path(common_dir)?;
        let copy = match self.setting {
            Setting::Mapped => self.root.join(calling_member).join(format!(
                "{}-{}",
                label(root),
                short(caller_root)
            )),
            Setting::SamePath => {
                let copy = self
                    .same_path_base
                    .join(root.strip_prefix("/").expect("absolute"));
                for protected in [&self.root, &self.state] {
                    if copy.starts_with(protected) || protected.starts_with(&copy) {
                        return Err(sync_failed(format!(
                            "A same-path copy at {} would overlap this member's Elsecall directories.",
                            copy.display()
                        )));
                    }
                }
                copy
            }
        };
        Ok(CopyLocation {
            setting: self.setting,
            state_file: self.state.join(format!(
                "{}.state",
                &Digest::of(copy.as_os_str().as_encoded_bytes()).hex()[..32]
            )),
            root: copy,
            store: self
                .root
                .join(".stores")
                .join(calling_member)
                .join(format!("{}.git", short(common_dir))),
            calling_member: calling_member.into(),
            caller_root: caller_root.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locations(setting: Setting) -> Locations {
        Locations {
            setting,
            root: "/Users/devbox/.elsecall/workspaces".into(),
            same_path_base: "/".into(),
            state: "/Users/devbox/Library/Application Support/Elsecall/copies".into(),
        }
    }

    #[test]
    fn maps_deterministically_per_calling_member_and_path() {
        let mapped = locations(Setting::Mapped);
        let a = mapped
            .locate("mac-mini", "/Users/me/src/app wt", "/Users/me/src/app/.git")
            .unwrap();
        assert_eq!(
            a,
            mapped
                .locate("mac-mini", "/Users/me/src/app wt", "/Users/me/src/app/.git")
                .unwrap()
        );
        assert!(
            a.root
                .to_string_lossy()
                .starts_with("/Users/devbox/.elsecall/workspaces/mac-mini/app-wt-")
        );
        let other_member = mapped
            .locate("laptop", "/Users/me/src/app wt", "/Users/me/src/app/.git")
            .unwrap();
        assert_ne!(a.root, other_member.root);
        let other_path = mapped
            .locate("mac-mini", "/Users/me/src/app2", "/Users/me/src/app/.git")
            .unwrap();
        assert_ne!(a.root, other_path.root);
        assert_eq!(
            a.store, other_path.store,
            "worktrees of one repository share history"
        );
        assert_ne!(a.state_file, other_path.state_file);
    }

    #[test]
    fn same_path_uses_the_callers_path_and_refuses_overlaps() {
        let same = locations(Setting::SamePath);
        let copy = same
            .locate("mac-mini", "/Users/me/src/app", "/Users/me/src/app/.git")
            .unwrap();
        assert_eq!(copy.root, PathBuf::from("/Users/me/src/app"));
        for bad in [
            "/Users/devbox/.elsecall",
            "/Users/devbox/.elsecall/workspaces/x",
        ] {
            assert_eq!(
                same.locate("m", bad, "/x/.git").unwrap_err().code,
                "workspace_sync_failed"
            );
        }
        for invalid in ["relative", "/a/../b", "/", "/a/./b", "/a/"] {
            assert_eq!(
                same.locate("m", invalid, "/x/.git").unwrap_err().code,
                "invalid_request"
            );
        }
    }
}
