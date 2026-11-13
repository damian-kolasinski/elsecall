//! The member's side of declared outputs: what they held before the command and
//! when it exited, cheaply, to tell which ones the run changed.
use crate::workspace::{
    confined::{Confined, Kind, Status},
    outputs::MAXIMUM_ENTRIES,
    path::WorkspacePath,
};
use std::{collections::BTreeMap, io};

/// How a path looked, cheaply: enough to tell that a run changed it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seen {
    File {
        inode: u64,
        size: u64,
        modified_ns: i64,
        changed_ns: i64,
        mode: u32,
    },
    Directory,
    Other,
}

/// What every declared output held, path by path; `None` for an output that could
/// not be read, which then counts as changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Survey(BTreeMap<WorkspacePath, Option<BTreeMap<WorkspacePath, Seen>>>);

fn seen(status: &Status) -> Seen {
    match status.kind {
        Kind::File => Seen::File {
            inode: status.inode,
            size: status.size,
            modified_ns: status.modified_ns,
            changed_ns: status.changed_ns,
            mode: status.mode,
        },
        Kind::Directory => Seen::Directory,
        Kind::Symlink | Kind::Other => Seen::Other,
    }
}

/// Every path at or under `root`, never following links.
fn walk(confined: &Confined, root: &WorkspacePath) -> io::Result<BTreeMap<WorkspacePath, Status>> {
    let mut found = BTreeMap::new();
    let Some(status) = confined.stat(root)? else {
        return Ok(found);
    };
    let mut pending = vec![(root.clone(), status)];
    while let Some((path, status)) = pending.pop() {
        if status.kind == Kind::Directory {
            for name in confined.list(Some(&path))?.unwrap_or_default() {
                let Some(child) = name.to_str().ok().and_then(|name| path.join(name)) else {
                    continue;
                };
                if let Some(status) = confined.stat(&child)? {
                    pending.push((child, status));
                }
            }
        }
        found.insert(path, status);
        if found.len() > MAXIMUM_ENTRIES {
            return Err(io::Error::other("an output holds too many files"));
        }
    }
    Ok(found)
}

pub fn survey(confined: &Confined, outputs: &[WorkspacePath]) -> Survey {
    Survey(
        outputs
            .iter()
            .map(|output| {
                let seen = walk(confined, output).ok().map(|found| {
                    found
                        .iter()
                        .map(|(path, status)| (path.clone(), seen(status)))
                        .collect()
                });
                (output.clone(), seen)
            })
            .collect(),
    )
}

impl Survey {
    /// The surveyed outputs.
    pub fn outputs(&self) -> Vec<WorkspacePath> {
        self.0.keys().cloned().collect()
    }

    /// Whether the survey saw `output` absent; not when it could not survey it.
    pub fn absent(&self, output: &WorkspacePath) -> bool {
        matches!(self.0.get(output), Some(Some(found)) if !found.contains_key(output))
    }
}

/// The outputs whose survey differs in `after`, or that could not be surveyed.
pub fn changed(before: &Survey, after: &Survey) -> Vec<WorkspacePath> {
    after
        .0
        .iter()
        .filter(|(output, seen)| seen.is_none() || before.0.get(*output) != Some(seen))
        .map(|(output, _)| output.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> WorkspacePath {
        WorkspacePath::parse(text).unwrap()
    }

    #[test]
    fn tells_changed_outputs_without_reading_them() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("logs")).unwrap();
        std::fs::write(root.join("logs/a"), "a").unwrap();
        std::fs::write(root.join("same"), "s").unwrap();
        let confined = Confined::open(root).unwrap();
        let outputs = [path("logs"), path("same"), path("new")];
        let before = survey(&confined, &outputs);
        assert!(before.absent(&path("new")) && !before.absent(&path("logs")));
        std::fs::write(root.join("logs/b"), "b").unwrap();
        std::fs::write(root.join("new"), "n").unwrap();
        let after = survey(&confined, &outputs);
        assert_eq!(changed(&before, &after), [path("logs"), path("new")]);
        // An output that could not be surveyed is changed, and never seen absent.
        use std::os::unix::fs::PermissionsExt;
        let locked = root.join("logs/locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = survey(&confined, &outputs);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!unreadable.absent(&path("logs")));
        assert!(changed(&unreadable, &unreadable).contains(&path("logs")));
    }
}
