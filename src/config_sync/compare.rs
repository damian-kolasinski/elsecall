//! What a member should hold and how it differs from what it holds: overrides laid
//! over the mirrored tree, and each file classed by fingerprints alone.
use crate::{
    config_sync::manifest::{FileKey, FileNode, Node, Tree},
    workspace::path::WorkspacePath,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The tree a member holds at a synced path: the mirrored tree with the member's
/// override files laid over it. Override files always win, and an override that
/// is a single file replaces the whole path. Returns the keys overrides decided.
pub fn overlay(mirrored: &Tree, overrides: &Tree) -> (Tree, BTreeSet<FileKey>) {
    match overrides {
        Tree::Absent => (mirrored.clone(), BTreeSet::new()),
        Tree::File(file) => (Tree::File(*file), BTreeSet::from([None])),
        Tree::Directory(laid) => {
            let mut entries = match mirrored {
                Tree::Directory(entries) => entries.clone(),
                _ => BTreeMap::new(),
            };
            let mut decided = BTreeSet::new();
            for (path, node) in laid {
                let Node::File(file) = node else { continue };
                put(&mut entries, path, *file);
                decided.insert(Some(path.clone()));
            }
            (Tree::Directory(entries), decided)
        }
    }
}

/// Puts `file` at `path`, replacing whatever is there or under it, and making each
/// directory above it a directory.
fn put(entries: &mut BTreeMap<WorkspacePath, Node>, path: &WorkspacePath, file: FileNode) {
    entries.retain(|existing, _| !existing.is_within(path));
    let mut ancestor = path.parent();
    while let Some(directory) = ancestor {
        entries.insert(directory.clone(), Node::Directory);
        ancestor = directory.parent();
    }
    entries.insert(path.clone(), Node::File(file));
}

/// The mirrored tree a master issues: its own files, except where its own
/// overrides decide, where the previously issued entries stay.
pub fn issued_from(live: &Tree, overridden: &BTreeSet<FileKey>, previous: &Tree) -> Tree {
    if overridden.is_empty() {
        return live.clone();
    }
    if overridden.contains(&None) {
        return previous.clone();
    }
    let mut entries = match live {
        Tree::Directory(entries) => entries.clone(),
        _ => BTreeMap::new(),
    };
    let before = match previous {
        Tree::Directory(entries) => entries.clone(),
        _ => BTreeMap::new(),
    };
    for key in overridden.iter().flatten() {
        // What the fleet held there decides: from a file above the key, if the
        // previous tree had one, else from the key itself, with all within it.
        let mut root = key.clone();
        let mut ancestor = key.parent();
        while let Some(directory) = ancestor {
            if matches!(before.get(&directory), Some(Node::File(_))) {
                root = directory.clone();
            }
            ancestor = directory.parent();
        }
        entries.retain(|existing, _| !existing.is_within(&root));
        let restored: Vec<(WorkspacePath, Node)> = before
            .iter()
            .filter(|(path, _)| path.is_within(&root))
            .map(|(path, node)| (path.clone(), *node))
            .collect();
        if restored.is_empty() {
            continue;
        }
        let mut ancestor = root.parent();
        while let Some(directory) = ancestor {
            entries.insert(directory.clone(), Node::Directory);
            ancestor = directory.parent();
        }
        entries.extend(restored);
    }
    Tree::Directory(entries)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    /// The member will get a file it lacks.
    Added,
    /// The member's file will be replaced with the master's.
    Changed,
    /// The member's file will be removed: the master has none.
    Removed,
    /// Same content, other permissions.
    ModeChanged,
    /// The member changed a file sync wrote; it is backed up, then the master wins.
    LocallyEdited,
    /// The member's override decides this file.
    Overridden,
}

/// How one file of a synced path differs, by fingerprints only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDifference {
    pub key: FileKey,
    pub change: Change,
    /// The member's current content is kept as a backup before it is replaced.
    pub backup: bool,
    pub desired: Option<FileNode>,
    pub live: Option<FileNode>,
}

/// Compares what a member should hold (`desired`) with what it holds (`live`),
/// given what sync last wrote there (`written`; `None` if it never synced the
/// path, so everything there is the member's own). Unchanged files are left out.
pub fn differences(
    desired: &Tree,
    overridden: &BTreeSet<FileKey>,
    live: &Tree,
    written: Option<&Tree>,
) -> Vec<FileDifference> {
    let desired_files = desired.files();
    let live_files = live.files();
    let written_files = written.map(Tree::files);
    let keys: BTreeSet<&FileKey> = desired_files.keys().chain(live_files.keys()).collect();
    let mut differences = Vec::new();
    for key in keys {
        let wanted = desired_files.get(key).copied();
        let held = live_files.get(key).copied();
        let wrote = written_files.as_ref().and_then(|w| w.get(key).copied());
        // Content sync did not put there, and would not, is the member's own.
        let own = held.is_some_and(|held| {
            Some(held.fingerprint) != wrote.map(|w| w.fingerprint)
                && Some(held.fingerprint) != wanted.map(|w| w.fingerprint)
        });
        let change = match (wanted, held) {
            _ if overridden.contains(key) => Change::Overridden,
            (Some(_), None) => Change::Added,
            (None, Some(_)) => Change::Removed,
            (Some(w), Some(h)) if w.fingerprint != h.fingerprint => {
                if own && written.is_some() && wrote.is_some() {
                    Change::LocallyEdited
                } else {
                    Change::Changed
                }
            }
            (Some(w), Some(h)) if w.mode != h.mode => Change::ModeChanged,
            _ => continue,
        };
        differences.push(FileDifference {
            key: key.clone(),
            change,
            backup: own && wanted != held,
            desired: wanted,
            live: held,
        });
    }
    differences
}

/// Whether `live` holds exactly `desired`, modes included.
pub fn holds(live: &Tree, desired: &Tree) -> bool {
    live == desired
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_sync::fingerprint::SyncKey;

    fn path(text: &str) -> WorkspacePath {
        WorkspacePath::parse(text).unwrap()
    }
    fn file(key: &SyncKey, content: &str) -> FileNode {
        FileNode {
            mode: 0o600,
            size: content.len() as u64,
            fingerprint: key.fingerprint(content.as_bytes()),
        }
    }
    fn tree(entries: &[(&str, Option<FileNode>)]) -> Tree {
        Tree::Directory(
            entries
                .iter()
                .map(|(p, f)| (path(p), f.map(Node::File).unwrap_or(Node::Directory)))
                .collect(),
        )
    }

    #[test]
    fn overrides_win_and_reshape_the_tree() {
        let key = SyncKey::generate().unwrap();
        let mirrored = tree(&[
            ("a", Some(file(&key, "fleet a"))),
            ("dir", None),
            ("dir/x", Some(file(&key, "x"))),
        ]);
        let overrides = tree(&[
            ("a", Some(file(&key, "mine"))),
            ("dir", Some(file(&key, "dir is a file here"))),
            ("new", None),
            ("new/n", Some(file(&key, "n"))),
        ]);
        let (desired, decided) = overlay(&mirrored, &overrides);
        assert_eq!(
            desired,
            tree(&[
                ("a", Some(file(&key, "mine"))),
                ("dir", Some(file(&key, "dir is a file here"))),
                ("new", None),
                ("new/n", Some(file(&key, "n"))),
            ])
        );
        assert_eq!(decided.len(), 3);
        let (single, decided) = overlay(&mirrored, &Tree::File(file(&key, "whole")));
        assert_eq!(single, Tree::File(file(&key, "whole")));
        assert_eq!(decided, BTreeSet::from([None]));
        assert_eq!(overlay(&mirrored, &Tree::Absent).0, mirrored);
    }

    #[test]
    fn a_master_keeps_issued_values_where_its_overrides_decide() {
        let key = SyncKey::generate().unwrap();
        let previous = tree(&[
            ("a", Some(file(&key, "fleet"))),
            ("b", Some(file(&key, "b"))),
        ]);
        let live = tree(&[
            ("a", Some(file(&key, "master's own"))),
            ("b", Some(file(&key, "b2"))),
            ("o", Some(file(&key, "override only"))),
        ]);
        let decided = BTreeSet::from([Some(path("a")), Some(path("o"))]);
        assert_eq!(
            issued_from(&live, &decided, &previous),
            tree(&[
                ("a", Some(file(&key, "fleet"))),
                ("b", Some(file(&key, "b2")))
            ])
        );
        assert_eq!(issued_from(&live, &BTreeSet::new(), &previous), live);

        // An override that changes a path's shape keeps every fleet entry it covers.
        let previous = tree(&[("sub", None), ("sub/token", Some(file(&key, "fleet")))]);
        let live = tree(&[("sub", Some(file(&key, "override")))]);
        let decided = BTreeSet::from([Some(path("sub"))]);
        assert_eq!(issued_from(&live, &decided, &previous), previous);
        let previous = tree(&[("sub", None), ("sub/token", Some(file(&key, "fleet")))]);
        let live = tree(&[
            ("sub", None),
            ("sub/token", None),
            ("sub/token/x", Some(file(&key, "override"))),
        ]);
        let decided = BTreeSet::from([Some(path("sub/token/x"))]);
        assert_eq!(issued_from(&live, &decided, &previous), previous);
    }

    #[test]
    fn classes_files_by_fingerprints() {
        let key = SyncKey::generate().unwrap();
        let desired = tree(&[
            ("same", Some(file(&key, "s"))),
            ("added", Some(file(&key, "a"))),
            ("changed", Some(file(&key, "new"))),
            ("edited", Some(file(&key, "fleet"))),
            ("mode", Some(file(&key, "m"))),
            ("over", Some(file(&key, "o"))),
        ]);
        let written = tree(&[
            ("same", Some(file(&key, "s"))),
            ("changed", Some(file(&key, "old"))),
            ("edited", Some(file(&key, "fleet-old"))),
            ("gone", Some(file(&key, "g"))),
            ("mode", Some(file(&key, "m"))),
        ]);
        let live = tree(&[
            ("same", Some(file(&key, "s"))),
            ("changed", Some(file(&key, "old"))),
            ("edited", Some(file(&key, "local change"))),
            ("gone", Some(file(&key, "g"))),
            ("extra", Some(file(&key, "made here"))),
            (
                "mode",
                Some(FileNode {
                    mode: 0o700,
                    ..file(&key, "m")
                }),
            ),
            ("over", Some(file(&key, "o"))),
        ]);
        let overridden = BTreeSet::from([Some(path("over"))]);
        let found: Vec<(String, Change, bool)> =
            differences(&desired, &overridden, &live, Some(&written))
                .into_iter()
                .map(|d| (d.key.unwrap().as_str().to_owned(), d.change, d.backup))
                .collect();
        assert_eq!(
            found,
            [
                ("added".into(), Change::Added, false),
                ("changed".into(), Change::Changed, false),
                ("edited".into(), Change::LocallyEdited, true),
                ("extra".into(), Change::Removed, true),
                ("gone".into(), Change::Removed, false),
                ("mode".into(), Change::ModeChanged, false),
                ("over".into(), Change::Overridden, false),
            ]
        );
        // Never synced before: everything there is the member's own.
        let first: Vec<(Change, bool)> = differences(&desired, &BTreeSet::new(), &live, None)
            .into_iter()
            .filter(|d| d.key == Some(path("changed")) || d.key == Some(path("extra")))
            .map(|d| (d.change, d.backup))
            .collect();
        assert_eq!(first, [(Change::Changed, true), (Change::Removed, true)]);
    }
}
