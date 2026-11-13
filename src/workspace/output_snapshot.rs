//! The declared outputs as they were when the command exited.
//!
//! The moment the command exits, every output the run changed is cloned, file by
//! file, into the run's staging area, so later changes to the outputs, as by
//! processes the command left behind, do not reach what comes back. The outputs
//! are then described, with digests, from that snapshot and sent from it. A path
//! that vanished, could not be read or would not fit is left out with its cause;
//! it never keeps the others from coming back. The staging area goes with the
//! snapshot.
use crate::workspace::{
    confined::{Confined, Create, Kind},
    digest::Digest,
    file_clone,
    not_returned::{Cause, Shortfall},
    output_staging::OutputStaging,
    output_survey::{self, Survey},
    outputs::{Changed, Content, MAXIMUM_BYTES, MAXIMUM_ENTRIES, OutputFile},
    path::WorkspacePath,
};
use std::{fs::File, io};

/// Most entries and bytes one run returns.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub entries: usize,
    pub bytes: u64,
}

impl Limits {
    pub const RETURNED: Limits = Limits {
        entries: MAXIMUM_ENTRIES,
        bytes: MAXIMUM_BYTES,
    };

    /// Takes `entries` and `bytes` out of what is left; `false`, leaving it as it
    /// was, when they do not fit.
    fn take(&mut self, entries: usize, bytes: u64) -> bool {
        if entries > self.entries || bytes > self.bytes {
            return false;
        }
        self.entries -= entries;
        self.bytes -= bytes;
        true
    }
}

/// What the run's outputs are once its command exited.
pub enum AtExit {
    /// The run declared none.
    NoOutputs,
    /// No snapshot could be taken, and why.
    Unavailable(String),
    Taken(Snapshot),
}

/// One changed output as cloned into the staging area.
enum Taken {
    /// The run removed it.
    Removed,
    /// A link or special file, which is never returned.
    Skipped,
    File {
        mode: u32,
    },
    Directory {
        directories: Vec<WorkspacePath>,
        files: Vec<(WorkspacePath, u32)>,
        skipped: Vec<WorkspacePath>,
    },
}

/// The declared outputs the run changed, as they were at exit.
pub struct Snapshot {
    staging: OutputStaging,
    changes: Vec<Changed>,
    shortfall: Shortfall,
}

impl Snapshot {
    /// Snapshots the outputs in `before` that changed since, into `staging`.
    pub fn take(
        workspace: &Confined,
        before: &Survey,
        staging: OutputStaging,
        limits: Limits,
    ) -> Self {
        let after = output_survey::survey(workspace, &before.outputs());
        let mut shortfall = Shortfall::default();
        // Never returnable, so noted whether or not the run changed anything there.
        let outside =
            |output: &WorkspacePath| after.absent(output) && workspace.link_on_the_way(output);
        for output in before.outputs() {
            if outside(&output) {
                shortfall.add(&output, &output, Cause::OutsideWorkspace, None);
            }
        }
        // Everything is cloned first, so the snapshot is as close to the exit as it
        // can be; reading it for digests can take its time. The budget bounds what
        // is cloned, as copies on volumes that cannot clone take space.
        let mut taken = Vec::new();
        let mut budget = limits;
        for output in output_survey::changed(before, &after) {
            if outside(&output) {
                continue;
            }
            let clone = take(
                workspace,
                staging.area(),
                &output,
                after.absent(&output),
                &mut budget,
                &mut shortfall,
            );
            if let Some(clone) = clone {
                taken.push((output, clone));
            }
        }
        let mut budget = limits;
        let changes = taken
            .into_iter()
            .filter_map(|(output, clone)| {
                describe(staging.area(), &output, clone, &mut budget, &mut shortfall)
            })
            .collect();
        Self {
            staging,
            changes,
            shortfall,
        }
    }

    /// The changed outputs, described from the snapshot.
    pub fn changes(&self) -> &[Changed] {
        &self.changes
    }

    /// What could not be snapshotted or described, except links, which the
    /// description lists itself.
    pub fn shortfall(&self) -> &Shortfall {
        &self.shortfall
    }

    /// Opens a described file of the snapshot for sending.
    pub fn open(&self, file: &OutputFile) -> io::Result<File> {
        self.staging
            .area()
            .open_file(&file.path)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
}

fn cause_of_opening(error: &io::Error) -> Cause {
    match error.raw_os_error() {
        Some(libc::ENOENT) => Cause::Vanished,
        _ => Cause::Unreadable,
    }
}

/// Clones `path`, a regular file of the workspace, to the same path in `staging`.
fn clone_file(
    workspace: &Confined,
    staging: &Confined,
    output: &WorkspacePath,
    path: &WorkspacePath,
    mode: u32,
    shortfall: &mut Shortfall,
) -> bool {
    let source = match workspace.open_file(path) {
        Ok(Some(source)) => source,
        // Gone, or replaced by something that is not a regular file.
        Ok(None) => {
            shortfall.add(output, path, Cause::Vanished, None);
            return false;
        }
        Err(e) => {
            shortfall.add_error(output, path, cause_of_opening(&e), &e);
            return false;
        }
    };
    let cloned = staging
        .locate_for_writing(path)
        .and_then(|(directory, name)| file_clone::clone_file(&source, &directory, &name, mode));
    match cloned {
        Ok(()) => true,
        Err(e) => {
            shortfall.add_error(output, path, Cause::TransferError, &e);
            false
        }
    }
}

/// Clones one changed output into `staging`, within `budget`; `None` when nothing
/// of it can be returned, with the cause in `shortfall`. `absent` is whether the
/// survey at exit saw it absent: only then did the run remove it.
fn take(
    workspace: &Confined,
    staging: &Confined,
    output: &WorkspacePath,
    absent: bool,
    budget: &mut Limits,
    shortfall: &mut Shortfall,
) -> Option<Taken> {
    let status = match workspace.stat(output) {
        Ok(Some(status)) => status,
        Ok(None) if absent => return Some(Taken::Removed),
        Ok(None) => {
            shortfall.add(output, output, Cause::Vanished, None);
            return None;
        }
        Err(e) => {
            shortfall.add_error(output, output, Cause::Unreadable, &e);
            return None;
        }
    };
    match status.kind {
        Kind::Symlink | Kind::Other => Some(Taken::Skipped),
        Kind::File if !budget.take(1, status.size) => {
            shortfall.add(output, output, Cause::TooLarge, None);
            None
        }
        Kind::File => clone_file(workspace, staging, output, output, status.mode, shortfall)
            .then_some(Taken::File {
                mode: status.mode & 0o777,
            }),
        Kind::Directory => take_directory(workspace, staging, output, budget, shortfall),
    }
}

fn take_directory(
    workspace: &Confined,
    staging: &Confined,
    output: &WorkspacePath,
    budget: &mut Limits,
    shortfall: &mut Shortfall,
) -> Option<Taken> {
    let mut left = *budget;
    let too_large = |shortfall: &mut Shortfall| {
        shortfall.add(output, output, Cause::TooLarge, None);
        let _ = staging.remove(output);
        None
    };
    let mut directories = Vec::new();
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    let mut pending = vec![output.clone()];
    while let Some(directory) = pending.pop() {
        let names = match workspace.list(Some(&directory)) {
            Ok(Some(names)) => names,
            Ok(None) => {
                shortfall.add(output, &directory, Cause::Vanished, None);
                if &directory == output {
                    return None;
                }
                continue;
            }
            Err(e) => {
                shortfall.add_error(output, &directory, cause_of_opening(&e), &e);
                if &directory == output {
                    return None;
                }
                continue;
            }
        };
        // Listed, so it is part of the snapshot, even if empty.
        if let Err(e) = staging.directory(Some(&directory), Create::Directories) {
            shortfall.add_error(output, &directory, Cause::TransferError, &e);
            if &directory == output {
                return None;
            }
            continue;
        }
        if &directory != output {
            directories.push(directory.clone());
        }
        for name in names {
            // A name no workspace path can hold, as one that is not UTF-8.
            let Some(path) = name.to_str().ok().and_then(|name| directory.join(name)) else {
                shortfall.add(
                    output,
                    &directory,
                    Cause::Unreadable,
                    Some("an entry's name is not a valid workspace path".into()),
                );
                continue;
            };
            if !left.take(1, 0) {
                return too_large(shortfall);
            }
            match workspace.stat(&path) {
                Ok(Some(status)) => match status.kind {
                    Kind::Directory => pending.push(path),
                    Kind::File if !left.take(0, status.size) => return too_large(shortfall),
                    Kind::File => {
                        if clone_file(workspace, staging, output, &path, status.mode, shortfall) {
                            files.push((path, status.mode & 0o777));
                        }
                    }
                    Kind::Symlink | Kind::Other => skipped.push(path),
                },
                Ok(None) => shortfall.add(output, &path, Cause::Vanished, None),
                Err(e) => shortfall.add_error(output, &path, Cause::Unreadable, &e),
            }
        }
    }
    *budget = left;
    Some(Taken::Directory {
        directories,
        files,
        skipped,
    })
}

/// Describes one cloned output with digests, within what is left of `budget`.
fn describe(
    staging: &Confined,
    output: &WorkspacePath,
    taken: Taken,
    budget: &mut Limits,
    shortfall: &mut Shortfall,
) -> Option<Changed> {
    let (directory, directories, files, skipped) = match taken {
        Taken::Removed => {
            return Some(Changed {
                output: output.clone(),
                content: None,
            });
        }
        Taken::Skipped => (false, Vec::new(), Vec::new(), vec![output.clone()]),
        Taken::File { mode } => (false, Vec::new(), vec![(output.clone(), mode)], Vec::new()),
        Taken::Directory {
            directories,
            files,
            skipped,
        } => (true, directories, files, skipped),
    };
    let mut sized = Vec::new();
    for (path, mode) in files {
        match staging.stat(&path) {
            Ok(Some(status)) => sized.push((path, mode, status.size)),
            _ => shortfall.add(output, &path, Cause::TransferError, None),
        }
    }
    let bytes: u64 = sized.iter().map(|(_, _, size)| size).sum();
    let entries = directories.len() + sized.len() + skipped.len();
    if !budget.take(entries, bytes) {
        shortfall.add(output, output, Cause::TooLarge, None);
        let _ = staging.remove(output);
        return None;
    }
    let mut content = Content {
        directory,
        directories,
        files: Vec::new(),
        skipped,
    };
    for (path, mode, _) in sized {
        let described = staging
            .open_file(&path)
            .and_then(|file| file.ok_or_else(|| io::Error::from(io::ErrorKind::NotFound)))
            .and_then(|file| Digest::of_reader(&file));
        match described {
            Ok((digest, size)) => content.files.push(OutputFile {
                path,
                mode,
                size,
                digest,
            }),
            Err(e) => shortfall.add_error(output, &path, Cause::Unreadable, &e),
        }
    }
    // A single file that could not be read leaves nothing to return.
    if !directory && content.files.is_empty() && content.skipped.is_empty() {
        return None;
    }
    Some(Changed {
        output: output.clone(),
        content: Some(content),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn path(text: &str) -> WorkspacePath {
        WorkspacePath::parse(text).unwrap()
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        root: std::path::PathBuf,
        areas: std::path::PathBuf,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("copy");
        let areas = temp.path().join("areas");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&areas).unwrap();
        Fixture {
            _temp: temp,
            root,
            areas,
        }
    }

    impl Fixture {
        fn staging(&self) -> OutputStaging {
            OutputStaging::create(Confined::open(&self.areas).unwrap(), "r-1").unwrap()
        }
        fn write(&self, relative: &str, content: &str) {
            let file = self.root.join(relative);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, content).unwrap();
        }
    }

    fn files(snapshot: &Snapshot) -> Vec<String> {
        let mut files: Vec<String> = snapshot
            .changes()
            .iter()
            .filter_map(|c| c.content.as_ref())
            .flat_map(|c| &c.files)
            .map(|f| {
                let content = std::io::read_to_string(snapshot.open(f).unwrap()).unwrap();
                format!("{}={content}", f.path)
            })
            .collect();
        files.sort();
        files
    }

    #[test]
    fn later_changes_never_reach_the_snapshot_and_staging_goes_with_it() {
        let f = fixture();
        let workspace = Confined::open(&f.root).unwrap();
        let outputs = [path("out"), path("result.txt"), path("same.txt")];
        f.write("same.txt", "unchanged");
        let before = output_survey::survey(&workspace, &outputs);
        f.write("out/a", "a");
        f.write("out/deep/b", "b");
        f.write("result.txt", "r");
        let snapshot = Snapshot::take(&workspace, &before, f.staging(), Limits::RETURNED);
        // What a leftover process does after the exit.
        std::fs::remove_dir_all(f.root.join("out")).unwrap();
        f.write("result.txt", "rewritten");
        assert_eq!(
            files(&snapshot),
            ["out/a=a", "out/deep/b=b", "result.txt=r"]
        );
        assert!(snapshot.shortfall().is_empty());
        let out = &snapshot.changes()[0];
        assert_eq!(
            out.content.as_ref().unwrap().directories,
            [path("out/deep")]
        );
        assert_eq!(
            snapshot.changes().len(),
            2,
            "unchanged outputs are left out"
        );
        assert!(f.areas.join("r-1").exists());
        drop(snapshot);
        assert_eq!(std::fs::read_dir(&f.areas).unwrap().count(), 0);
    }

    #[test]
    fn a_file_that_vanishes_during_the_snapshot_never_stops_the_others() {
        let f = fixture();
        let workspace = Confined::open(&f.root).unwrap();
        let outputs = [path("out")];
        let before = output_survey::survey(&workspace, &outputs);
        f.write("out/kept", "k");
        f.write("out/gone", "g");
        let staging = f.staging();
        let mut shortfall = Shortfall::default();
        // Listed, then gone before it was cloned.
        let after = output_survey::survey(&workspace, &outputs);
        assert_eq!(output_survey::changed(&before, &after), [path("out")]);
        std::fs::remove_file(f.root.join("out/gone")).unwrap();
        let removed_entry = clone_file(
            &workspace,
            staging.area(),
            &path("out"),
            &path("out/gone"),
            0o644,
            &mut shortfall,
        );
        assert!(!removed_entry);
        let mut budget = Limits::RETURNED;
        let taken = take(
            &workspace,
            staging.area(),
            &path("out"),
            false,
            &mut budget,
            &mut shortfall,
        )
        .unwrap();
        let mut budget = Limits::RETURNED;
        let changed = describe(
            staging.area(),
            &path("out"),
            taken,
            &mut budget,
            &mut shortfall,
        )
        .unwrap();
        assert_eq!(changed.content.unwrap().files[0].path, path("out/kept"));
        assert_eq!(
            shortfall.warnings(),
            ["out/gone was not returned: it vanished while the snapshot at exit was taken."]
        );
        // An output gone between the survey and its clone vanished; it was not
        // removed by the run, so the caller's version stays.
        std::fs::remove_dir_all(f.root.join("out")).unwrap();
        let mut shortfall = Shortfall::default();
        let mut budget = Limits::RETURNED;
        assert!(
            take(
                &workspace,
                staging.area(),
                &path("out"),
                false,
                &mut budget,
                &mut shortfall
            )
            .is_none()
        );
        assert_eq!(shortfall.entries()[0].cause, Cause::Vanished);
        // Seen absent at exit: the run removed it.
        let removed = take(
            &workspace,
            staging.area(),
            &path("out"),
            true,
            &mut budget,
            &mut shortfall,
        );
        assert!(matches!(removed, Some(Taken::Removed)));
    }

    #[test]
    fn unreadable_linked_outside_and_too_large_paths_are_left_out_with_causes() {
        let f = fixture();
        let outside = tempfile::tempdir().unwrap();
        let workspace = Confined::open(&f.root).unwrap();
        let outputs = [path("logs"), path("via/out.txt"), path("big"), path("link")];
        std::fs::create_dir_all(f.root.join("via")).unwrap();
        let before = output_survey::survey(&workspace, &outputs);
        f.write("logs/readable", "r");
        f.write("logs/locked", "secret");
        std::fs::set_permissions(
            f.root.join("logs/locked"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        std::os::unix::fs::symlink("/etc", f.root.join("logs/escape")).unwrap();
        std::fs::remove_dir(f.root.join("via")).unwrap();
        std::os::unix::fs::symlink(outside.path(), f.root.join("via")).unwrap();
        std::fs::write(outside.path().join("out.txt"), "outside").unwrap();
        f.write("big/one", "12345");
        f.write("big/two", "12345");
        std::os::unix::fs::symlink("/tmp", f.root.join("link")).unwrap();
        let limits = Limits {
            entries: 100,
            bytes: 8,
        };
        let staging = f.staging();
        let staged = f.areas.join("r-1");
        let snapshot = Snapshot::take(&workspace, &before, staging, limits);
        let mut warnings = snapshot.shortfall().warnings();
        warnings.sort();
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].starts_with("big was not returned: the run's outputs would exceed"));
        assert!(
            warnings[1].starts_with("logs/locked was not returned: it could not be read on the member (Permission denied"),
            "{warnings:?}"
        );
        assert!(
            warnings[2]
                .starts_with("via/out.txt was not returned: a directory on its way is a link")
        );
        assert!(
            !staged.join("big").exists(),
            "an output too large is not kept in staging"
        );
        let outputs: Vec<_> = snapshot
            .changes()
            .iter()
            .map(|c| c.output.as_str())
            .collect();
        assert_eq!(outputs, ["link", "logs"]);
        assert_eq!(files(&snapshot), ["logs/readable=r"]);
        let logs = snapshot.changes()[1].content.as_ref().unwrap();
        assert_eq!(logs.skipped, [path("logs/escape")]);
        std::fs::set_permissions(
            f.root.join("logs/locked"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }

    #[test]
    fn cloning_stops_once_an_output_exceeds_what_is_returned() {
        let f = fixture();
        let workspace = Confined::open(&f.root).unwrap();
        let staging = f.staging();
        for name in ["a", "b", "c"] {
            f.write(&format!("out/{name}"), "1234");
        }
        let mut budget = Limits {
            entries: 100,
            bytes: 6,
        };
        let mut shortfall = Shortfall::default();
        let taken = take(
            &workspace,
            staging.area(),
            &path("out"),
            false,
            &mut budget,
            &mut shortfall,
        );
        assert!(taken.is_none());
        assert_eq!(shortfall.entries()[0].cause, Cause::TooLarge);
        assert_eq!((budget.entries, budget.bytes), (100, 6), "nothing used");
        assert!(!f.areas.join("r-1/out").exists());
    }
}
