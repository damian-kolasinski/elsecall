//! A calling member's sync state for its own workspaces, in `workspaces/` in its
//! state directory: per workspace a stat cache, what it last pushed to each member,
//! and the path the workspace's files stand for.
//!
//! It answers the members holding copies, and is cleaned up when a workspace is
//! deleted, when a member removed its copy, or when a member left the fleet.
use crate::{
    housekeeping::exchange::{Answer, Question, WorkspaceAnswer, WorkspaceState, valid_path},
    scheduling::timestamp::Timestamp,
    workspace::push_cache::{self, CacheFiles, StateFile},
};
use serde::Serialize;
use std::{
    collections::HashSet,
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Duration,
};

/// A removal is taken to postdate a push only if it came this much later, so a
/// push the member had not seen when it listed its copies is never forgotten.
const CLOCK_MARGIN: Duration = Duration::from_secs(60);

pub struct CallerState {
    directory: PathBuf,
}

/// What a tidy removed, or would remove.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Tidied {
    pub files: usize,
    pub bytes: u64,
}

/// Whether the workspace at `path` still exists here. A workspace whose nearest
/// remaining directory is where volumes are mounted, or a mount point left behind
/// without its volume, may be on a volume that is away: it is not known to be gone.
pub fn workspace_state(path: &str) -> WorkspaceState {
    match std::fs::symlink_metadata(path) {
        Ok(_) => WorkspaceState::Present,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let nearest = Path::new(path).ancestors().skip(1).find(|a| a.exists());
            match nearest {
                Some(nearest) if !where_volumes_mount(nearest) => WorkspaceState::Gone,
                _ => WorkspaceState::Unknown,
            }
        }
        Err(_) => WorkspaceState::Unknown,
    }
}

fn where_volumes_mount(directory: &Path) -> bool {
    const MOUNTS: [&str; 5] = ["/", "/Volumes", "/Network", "/net", "/System/Volumes"];
    if MOUNTS.iter().any(|m| directory == Path::new(m)) {
        return true;
    }
    // An empty mount point left in /Volumes sits on /Volumes' own volume.
    directory.parent() == Some(Path::new("/Volumes"))
        && std::fs::metadata(directory)
            .ok()
            .zip(std::fs::metadata("/Volumes").ok())
            .is_some_and(|(mount, volumes)| mount.dev() == volumes.dev())
}

fn modified(path: &Path) -> Option<Timestamp> {
    std::fs::symlink_metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .map(Timestamp::from)
}

impl CallerState {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// Forget only caches registered for this removed synthetic session.
    pub fn forget_rehearsal(&self, session: &str) -> crate::failure::Result<()> {
        let files = push_cache::list(&self.directory);
        let ids: HashSet<_> = files
            .iter()
            .filter_map(|(file, _)| match file {
                StateFile::Registry { id }
                    if push_cache::registered_root(&self.directory, id).is_some_and(|root| {
                        crate::rehearsal::protocol::scratch_of(session, &root)
                    }) =>
                {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect();
        for (file, path) in files {
            let id = match file {
                StateFile::Stat { id }
                | StateFile::Pushed { id, .. }
                | StateFile::Registry { id } => id,
            };
            if ids.contains(&id) {
                match std::fs::remove_file(path) {
                    Ok(()) => (),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                    Err(e) => {
                        return Err(crate::failure::Failure::io(
                            "Remove rehearsal sync cache",
                            e,
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Answers `asker`, an enrolled member, about the workspaces it holds copies
    /// of, and forgets what was pushed to the copies it removed. Only workspaces
    /// this member pushed to `asker` are answered; others are unknown to it.
    pub fn answer(&self, asker: &str, question: &Question) -> Answer {
        let mut answer = Answer::default();
        for path in &question.held {
            let files = CacheFiles::new(&self.directory, path);
            let state = if files.pushed(asker).exists() {
                if !question.dry_run {
                    files.register();
                }
                workspace_state(path)
            } else {
                WorkspaceState::Unknown
            };
            answer.workspaces.push(WorkspaceAnswer {
                path: path.clone(),
                state,
            });
        }
        for removed in &question.removed {
            let pushed = CacheFiles::new(&self.directory, &removed.path).pushed(asker);
            // A deleted workspace cannot be pushed again, so its push goes
            // whenever it was made.
            let stale = pushed.exists()
                && (workspace_state(&removed.path) == WorkspaceState::Gone
                    || modified(&pushed)
                        .is_some_and(|written| written.after(CLOCK_MARGIN) < removed.removed_at));
            if stale && (question.dry_run || std::fs::remove_file(&pushed).is_ok()) {
                answer.forgotten += 1;
            }
        }
        answer
    }

    /// Removes the stat caches of deleted workspaces, what was pushed to members
    /// no longer in the fleet (`members`, when known), what a deleted workspace
    /// pushed longer than `stale_after` ago, by when its copy is gone anyway, and
    /// the record of a deleted workspace once nothing else of it is left.
    pub fn tidy(&self, members: Option<&[String]>, stale_after: Duration, dry_run: bool) -> Tidied {
        let mut tidied = Tidied::default();
        let remove = |path: &Path, tidied: &mut Tidied| {
            let bytes = std::fs::symlink_metadata(path).map_or(0, |m| m.blocks() * 512);
            if dry_run || std::fs::remove_file(path).is_ok() {
                tidied.files += 1;
                tidied.bytes += bytes;
            }
        };
        let files = push_cache::list(&self.directory);
        let mut removed: HashSet<&Path> = HashSet::new();
        // Pushes to members that left the fleet.
        if let Some(members) = members {
            for (file, path) in &files {
                if let StateFile::Pushed { member, .. } = file
                    && !members.contains(member)
                {
                    remove(path, &mut tidied);
                    removed.insert(path);
                }
            }
        }
        // Deleted workspaces.
        for (file, registry) in &files {
            let StateFile::Registry { id } = file else {
                continue;
            };
            let gone = push_cache::registered_root(&self.directory, id)
                .filter(|root| valid_path(root))
                .is_some_and(|root| workspace_state(&root) == WorkspaceState::Gone);
            if !gone {
                continue;
            }
            let stale = Timestamp::now()
                .millis()
                .saturating_sub(stale_after.as_millis() as u64);
            for (other, path) in &files {
                let old_push = matches!(other, StateFile::Pushed { id: of, .. } if of == id)
                    && !removed.contains(path.as_path())
                    && modified(path).is_some_and(|at| at.millis() < stale);
                if *other == (StateFile::Stat { id: id.clone() }) || old_push {
                    remove(path, &mut tidied);
                    removed.insert(path);
                }
            }
            let pushes_left = files.iter().any(|(other, path)| {
                matches!(other, StateFile::Pushed { id: of, .. } if of == id)
                    && !removed.contains(path.as_path())
            });
            if !pushes_left {
                remove(registry, &mut tidied);
            }
        }
        tidied
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::housekeeping::exchange::RemovedCopy;

    fn state() -> (tempfile::TempDir, CallerState, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let caches = dir.path().join("workspaces");
        std::fs::create_dir(&caches).unwrap();
        (dir, CallerState::new(caches.clone()), caches)
    }

    fn pushed(caches: &Path, root: &str, member: &str) -> PathBuf {
        let files = CacheFiles::new(caches, root);
        std::fs::write(files.pushed(member), "pushed").unwrap();
        std::fs::write(files.stat(), "stat").unwrap();
        files.pushed(member)
    }

    #[test]
    fn only_workspaces_pushed_to_the_asker_are_answered() {
        let (dir, state, caches) = state();
        let present = dir.path().join("present");
        std::fs::create_dir(&present).unwrap();
        let present = present.to_str().unwrap().to_owned();
        let gone = dir.path().join("gone").to_str().unwrap().to_owned();
        let elsewhere = dir.path().join("elsewhere").to_str().unwrap().to_owned();
        pushed(&caches, &present, "bravo");
        pushed(&caches, &gone, "bravo");
        pushed(&caches, &elsewhere, "charlie");
        let question = Question {
            held: vec![present.clone(), gone.clone(), elsewhere.clone()],
            ..Question::default()
        };
        let states: Vec<WorkspaceState> = state
            .answer("bravo", &question)
            .workspaces
            .iter()
            .map(|w| w.state)
            .collect();
        assert_eq!(
            states,
            [
                WorkspaceState::Present,
                WorkspaceState::Gone,
                WorkspaceState::Unknown
            ]
        );
        assert_eq!(
            state.answer("charlie", &question).workspaces[1].state,
            WorkspaceState::Unknown,
            "charlie was never pushed the deleted workspace"
        );
        assert!(CacheFiles::new(&caches, &gone).registry().exists());
    }

    #[test]
    fn removals_forget_only_older_pushes_to_the_member_that_removed() {
        let (dir, state, caches) = state();
        let to_bravo = pushed(&caches, "/w/app", "bravo");
        let to_charlie = pushed(&caches, "/w/app", "charlie");
        let soon = Question {
            removed: vec![RemovedCopy {
                path: "/w/app".into(),
                removed_at: Timestamp::now(),
            }],
            ..Question::default()
        };
        assert_eq!(
            state.answer("bravo", &soon).forgotten,
            0,
            "within the margin"
        );
        let later = Question {
            removed: vec![RemovedCopy {
                path: "/w/app".into(),
                removed_at: Timestamp::now().after(Duration::from_secs(120)),
            }],
            ..Question::default()
        };
        let dry = Question {
            dry_run: true,
            ..later.clone()
        };
        assert_eq!(state.answer("bravo", &dry).forgotten, 1);
        assert!(to_bravo.exists());
        assert_eq!(state.answer("bravo", &later).forgotten, 1);
        assert!(!to_bravo.exists());
        assert!(to_charlie.exists(), "another member's push stays");

        // A deleted workspace cannot be pushed again: even a fresh push goes.
        let gone = dir.path().join("deleted").to_str().unwrap().to_owned();
        let fresh = pushed(&caches, &gone, "bravo");
        let removal = Question {
            removed: vec![RemovedCopy {
                path: gone,
                removed_at: Timestamp::now(),
            }],
            ..Question::default()
        };
        assert_eq!(state.answer("bravo", &removal).forgotten, 1);
        assert!(!fresh.exists());
    }

    #[test]
    fn tidy_removes_what_deleted_workspaces_and_departed_members_left() {
        let (dir, state, caches) = state();
        let kept_root = dir.path().join("kept");
        std::fs::create_dir(&kept_root).unwrap();
        let kept = kept_root.to_str().unwrap().to_owned();
        let gone = dir.path().join("gone").to_str().unwrap().to_owned();
        for root in [&kept, &gone] {
            pushed(&caches, root, "bravo");
            CacheFiles::new(&caches, root).register();
        }
        let departed = pushed(&caches, &kept, "delta");
        let members = ["alpha".to_owned(), "bravo".to_owned()];

        let day = Duration::from_secs(86_400);
        let planned = state.tidy(Some(&members), day, true);
        assert_eq!(planned.files, 2, "delta's push and the deleted stat cache");
        assert!(departed.exists());

        state.tidy(Some(&members), day, false);
        assert!(!departed.exists());
        let gone_files = CacheFiles::new(&caches, &gone);
        assert!(!gone_files.stat().exists());
        assert!(
            gone_files.pushed("bravo").exists(),
            "bravo has not removed it yet"
        );
        assert!(gone_files.registry().exists(), "kept while a push is left");
        let kept_files = CacheFiles::new(&caches, &kept);
        assert!(kept_files.stat().exists() && kept_files.pushed("bravo").exists());

        // Age the file explicitly: two operations can share a millisecond, so
        // a zero idle period does not prove its modification time is earlier.
        std::fs::File::options()
            .write(true)
            .open(gone_files.pushed("bravo"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
            .unwrap();
        state.tidy(None, day, false);
        assert!(!gone_files.pushed("bravo").exists());
        assert!(!gone_files.registry().exists());
        assert!(kept_files.registry().exists());
    }
}
