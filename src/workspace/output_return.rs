//! The caller's side of declared outputs: each changed output is rebuilt beside the
//! caller's under a fresh name, from content the caller already has and content
//! the member sends, and then swapped into place atomically. Nothing is written
//! through a link, and nothing is replaced until the transfer ended. A file that
//! did not arrive is left out of its directory's rebuild, and a single file that
//! did not arrive leaves the caller's version as it was; either is noted with its
//! cause.
use crate::workspace::{
    confined::{Confined, Create, Kind, remove_at, temporary_name},
    digest::{Digest, Hasher},
    not_returned::{Cause, Shortfall},
    outputs::{Changed, MAXIMUM_ENTRIES},
    path::WorkspacePath,
};
use std::{
    collections::HashMap,
    ffi::CString,
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsRawFd, OwnedFd},
    sync::Arc,
};

/// Where in a rebuild a file goes: the rebuilt file itself, or a path inside a
/// rebuilt directory.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Place {
    Itself,
    Inside(WorkspacePath),
}

enum Rebuilt {
    /// The run removed the output.
    Removed,
    /// The output's content did not arrive; the caller's version stays.
    Withdrawn,
    /// A file or directory under `temporary` in the output's parent.
    Present {
        parent: Arc<OwnedFd>,
        temporary: CString,
        /// The rebuilt directory, opened once.
        tree: Option<Confined>,
        modes: HashMap<Place, u32>,
    },
}

struct Rebuild {
    output: WorkspacePath,
    rebuilt: Rebuilt,
}

struct Wanted {
    digest: Digest,
    size: u64,
    places: Vec<(usize, Place)>,
}

/// The caller's work of bringing changed outputs back.
pub struct Returning {
    rebuilds: Vec<Rebuild>,
    wanted: Vec<Wanted>,
    /// What will not come back, and why.
    pub shortfall: Shortfall,
}

/// How a return ended.
pub struct Returned {
    pub shortfall: Shortfall,
    /// What else happened, for the warnings, such as an old version left behind.
    pub notes: Vec<String>,
}

/// A blob being received.
#[derive(Default)]
pub struct Receiving {
    file: Option<File>,
    hasher: Hasher,
    size: u64,
}

fn relative(output: &WorkspacePath, path: &WorkspacePath) -> Option<WorkspacePath> {
    let rest = path
        .as_str()
        .strip_prefix(output.as_str())?
        .strip_prefix('/')?;
    WorkspacePath::parse(rest)
}

/// Digests of the caller's own files at `output`, to reuse unchanged content.
fn local_digests(confined: &Confined, output: &WorkspacePath) -> HashMap<Digest, WorkspacePath> {
    let mut digests = HashMap::new();
    let mut pending = vec![output.clone()];
    while let Some(path) = pending.pop() {
        if digests.len() > MAXIMUM_ENTRIES {
            break;
        }
        match confined.stat(&path) {
            Ok(Some(status)) if status.kind == Kind::Directory => {
                for name in confined
                    .list(Some(&path))
                    .ok()
                    .flatten()
                    .unwrap_or_default()
                {
                    if let Some(child) = name.to_str().ok().and_then(|n| path.join(n)) {
                        pending.push(child);
                    }
                }
            }
            Ok(Some(status)) if status.kind == Kind::File => {
                if let Ok(Some(file)) = confined.open_file(&path)
                    && let Ok((digest, _)) = Digest::of_reader(&file)
                {
                    digests.insert(digest, path);
                }
            }
            _ => (),
        }
    }
    digests
}

impl Rebuild {
    fn create(&self, place: &Place) -> io::Result<File> {
        let Rebuilt::Present {
            parent,
            temporary,
            tree,
            ..
        } = &self.rebuilt
        else {
            unreachable!("only present outputs receive content")
        };
        let file = match (place, tree) {
            (Place::Itself, _) => Confined::create_at(parent, temporary)?,
            (Place::Inside(path), Some(tree)) => {
                let (directory, name) = tree.locate_for_writing(path)?;
                Confined::create_at(&directory, &name)?
            }
            (Place::Inside(_), None) => unreachable!("paths inside only in directories"),
        };
        // Created private; the listed modes are applied once every copy is made,
        // since a mode without read permission would stop copying from it.
        Ok(file)
    }

    /// Gives every rebuilt file the mode the member listed.
    fn apply_modes(&self) -> io::Result<()> {
        let Rebuilt::Present {
            parent,
            temporary,
            tree,
            modes,
        } = &self.rebuilt
        else {
            return Ok(());
        };
        for (place, mode) in modes {
            match (place, tree) {
                (Place::Itself, _) => {
                    // SAFETY: valid descriptor and C string; never follows a link.
                    let result = unsafe {
                        libc::fchmodat(
                            parent.as_raw_fd(),
                            temporary.as_ptr(),
                            *mode as libc::mode_t,
                            libc::AT_SYMLINK_NOFOLLOW,
                        )
                    };
                    if result < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                (Place::Inside(path), Some(tree)) => tree.set_mode(path, *mode)?,
                (Place::Inside(_), None) => (),
            }
        }
        Ok(())
    }

    fn open(&self, place: &Place) -> io::Result<File> {
        let Rebuilt::Present {
            parent,
            temporary,
            tree,
            ..
        } = &self.rebuilt
        else {
            unreachable!("only present outputs receive content")
        };
        match (place, tree) {
            (Place::Itself, _) => Confined::open_child_file(parent, temporary),
            (Place::Inside(path), Some(tree)) => tree
                .open_file(path)?
                .ok_or_else(|| io::Error::other("a received file vanished")),
            (Place::Inside(_), None) => unreachable!("paths inside only in directories"),
        }
    }

    /// Copies `source` into `place`, keeping it only if it still has `digest`.
    fn reuse(&self, mut source: File, place: &Place, digest: &Digest) -> io::Result<bool> {
        let mut target = self.create(place)?;
        let mut hasher = Hasher::default();
        let mut buffer = vec![0u8; 256 * 1024];
        loop {
            let n = source.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            target.write_all(&buffer[..n])?;
        }
        if hasher.finish() == *digest {
            return Ok(true);
        }
        // The caller's file changed meanwhile: receive it after all.
        drop(target);
        self.discard(place);
        Ok(false)
    }

    fn discard(&self, place: &Place) {
        if let Rebuilt::Present {
            parent,
            temporary,
            tree,
            ..
        } = &self.rebuilt
        {
            match (place, tree) {
                (Place::Itself, _) => {
                    let _ = remove_at(parent, temporary);
                }
                (Place::Inside(path), Some(tree)) => {
                    let _ = tree.remove(path);
                }
                (Place::Inside(_), None) => (),
            }
        }
    }

    fn abandon(&self) {
        if let Rebuilt::Present {
            parent, temporary, ..
        } = &self.rebuilt
        {
            let _ = remove_at(parent, temporary);
        }
    }
}

impl Returning {
    /// Prepares each change beside the caller's output, reusing content the caller
    /// already has, and lists what must still come from the member. `changes` must
    /// have passed `outputs::validate`.
    pub fn prepare(confined: &Confined, changes: Vec<Changed>) -> Self {
        let mut returning = Returning {
            rebuilds: Vec::new(),
            wanted: Vec::new(),
            shortfall: Shortfall::default(),
        };
        returning.shortfall.add_skipped(&changes);
        let mut wanted: HashMap<Digest, Wanted> = HashMap::new();
        for change in changes {
            let output = change.output.clone();
            let Some(content) = change.content else {
                returning.rebuilds.push(Rebuild {
                    output,
                    rebuilt: Rebuilt::Removed,
                });
                continue;
            };
            if !content.directory && content.files.is_empty() {
                continue;
            }
            let shortfall = &mut returning.shortfall;
            let not_returned = |shortfall: &mut Shortfall, e: &io::Error| {
                shortfall.add_error(&output, &output, Cause::TransferError, e)
            };
            let parent = match output.parent() {
                None => confined.directory(None, Create::No),
                Some(parent) => confined.directory(Some(&parent), Create::Directories),
            };
            let parent = match parent {
                Ok(Some(parent)) => parent,
                Ok(None) if confined.link_on_the_way(&output) => {
                    shortfall.add(&output, &output, Cause::OutsideWorkspace, None);
                    continue;
                }
                Ok(None) => {
                    not_returned(shortfall, &io::Error::other("a file is in the way"));
                    continue;
                }
                Err(e) => {
                    not_returned(shortfall, &e);
                    continue;
                }
            };
            let temporary = match temporary_name() {
                Ok(temporary) => temporary,
                Err(e) => {
                    not_returned(shortfall, &e);
                    continue;
                }
            };
            let place_of = |path: &WorkspacePath| {
                if content.directory {
                    Place::Inside(relative(&output, path).expect("validated inside"))
                } else {
                    Place::Itself
                }
            };
            let modes = content
                .files
                .iter()
                .map(|file| (place_of(&file.path), file.mode))
                .collect();
            let index = returning.rebuilds.len();
            let mut needed: Vec<(Digest, u64, Place)> = Vec::new();
            let built = (|| -> io::Result<Rebuild> {
                let tree = if content.directory {
                    // SAFETY: valid descriptor and C string.
                    if unsafe { libc::mkdirat(parent.as_raw_fd(), temporary.as_ptr(), 0o755) } < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Some(Confined::open_at(&parent, &temporary)?)
                } else {
                    None
                };
                let rebuild = Rebuild {
                    output: output.clone(),
                    rebuilt: Rebuilt::Present {
                        parent: parent.clone(),
                        temporary: temporary.clone(),
                        tree,
                        modes,
                    },
                };
                if let Rebuilt::Present {
                    tree: Some(tree), ..
                } = &rebuild.rebuilt
                {
                    for directory in &content.directories {
                        tree.directory(relative(&output, directory).as_ref(), Create::Directories)?;
                    }
                }
                let local = local_digests(confined, &output);
                for file in &content.files {
                    let place = place_of(&file.path);
                    let reused = match local
                        .get(&file.digest)
                        .map(|source| confined.open_file(source))
                    {
                        Some(Ok(Some(source))) => rebuild.reuse(source, &place, &file.digest)?,
                        _ => false,
                    };
                    if !reused {
                        needed.push((file.digest, file.size, place));
                    }
                }
                Ok(rebuild)
            })();
            match built {
                Ok(rebuild) => {
                    // Only a complete rebuild asks for content.
                    for (digest, size, place) in needed {
                        wanted
                            .entry(digest)
                            .or_insert_with(|| Wanted {
                                digest,
                                size,
                                places: Vec::new(),
                            })
                            .places
                            .push((index, place));
                    }
                    returning.rebuilds.push(rebuild);
                }
                Err(e) => {
                    let _ = crate::workspace::confined::remove_at(&parent, &temporary);
                    returning
                        .shortfall
                        .add_error(&output, &output, Cause::TransferError, &e);
                }
            }
        }
        returning.wanted = wanted.into_values().collect();
        returning
    }

    /// Content to ask the member for, in the order it will arrive.
    pub fn wanted(&self) -> Vec<Digest> {
        self.wanted.iter().map(|wanted| wanted.digest).collect()
    }

    /// Writes a chunk of the `index`th wanted blob.
    pub fn write(&self, index: usize, chunk: &[u8], state: &mut Receiving) -> io::Result<()> {
        let wanted = &self.wanted[index];
        let (rebuild, place) = &wanted.places[0];
        if state.file.is_none() {
            state.file = Some(self.rebuilds[*rebuild].create(place)?);
        }
        state.size += chunk.len() as u64;
        if state.size > wanted.size {
            return Err(io::Error::other("an output arrived larger than listed"));
        }
        state.hasher.update(chunk);
        state.file.as_mut().expect("created").write_all(chunk)
    }

    /// Ends the `index`th wanted blob: checks it and copies it everywhere it goes.
    pub fn complete(&self, index: usize, state: &mut Receiving) -> io::Result<()> {
        let wanted = &self.wanted[index];
        let (rebuild, place) = &wanted.places[0];
        let file = match state.file.take() {
            Some(file) => file,
            None => self.rebuilds[*rebuild].create(place)?,
        };
        drop(file);
        let received = std::mem::take(&mut state.hasher).finish();
        let length = std::mem::take(&mut state.size);
        if length != wanted.size || received != wanted.digest {
            return Err(io::Error::other(
                "an output arrived with other content than listed",
            ));
        }
        for (other, other_place) in wanted.places.iter().skip(1) {
            let mut copy = self.rebuilds[*other].create(other_place)?;
            let mut source = self.rebuilds[*rebuild].open(place)?;
            io::copy(&mut source, &mut copy)?;
        }
        Ok(())
    }

    /// The `index`th wanted blob did not arrive, for `cause`: every place it goes
    /// is left out. A directory is returned without it; a single file is not
    /// returned, so the caller's version stays.
    pub fn fail(&mut self, index: usize, cause: Cause, detail: Option<String>) {
        for (rebuild, place) in self.wanted[index].places.clone() {
            let rebuild = &mut self.rebuilds[rebuild];
            rebuild.discard(&place);
            let output = rebuild.output.clone();
            let path = match &place {
                Place::Itself => {
                    rebuild.abandon();
                    rebuild.rebuilt = Rebuilt::Withdrawn;
                    output.clone()
                }
                Place::Inside(relative) => {
                    if let Rebuilt::Present { modes, .. } = &mut rebuild.rebuilt {
                        modes.remove(&place);
                    }
                    output.join(relative.as_str()).expect("inside the output")
                }
            };
            self.shortfall.add(&output, &path, cause, detail.clone());
        }
    }

    /// Swaps every rebuilt output into place and removes what it replaced.
    pub fn finish(mut self, confined: &Confined) -> Returned {
        let mut shortfall = std::mem::take(&mut self.shortfall);
        let mut notes = Vec::new();
        for rebuild in &self.rebuilds {
            let output = &rebuild.output;
            let result = match &rebuild.rebuilt {
                Rebuilt::Withdrawn => continue,
                Rebuilt::Removed => confined.remove(output),
                Rebuilt::Present {
                    parent, temporary, ..
                } => {
                    let name = CString::new(output.file_name()).expect("paths have no NUL");
                    // The old version, if any, ends up under the temporary name.
                    rebuild
                        .apply_modes()
                        .and_then(|_| Confined::swap_at(parent, temporary, &name))
                        .map(|replaced| {
                            if replaced && remove_at(parent, temporary).is_err() {
                                notes.push(format!(
                                    "{output} was replaced, but its old version could not be removed; it is hidden beside it."
                                ));
                            }
                        })
                }
            };
            if let Err(e) = result {
                rebuild.abandon();
                shortfall.add_error(output, output, Cause::TransferError, &e);
            }
        }
        Returned { shortfall, notes }
    }

    /// Abandons the rebuilds, as after a lost connection: nothing is returned.
    pub fn abandon(self) {}
}

/// Rebuilds not swapped into place, as when a transfer is interrupted, leave no
/// temporaries behind. After a swap the temporary name is already gone.
impl Drop for Returning {
    fn drop(&mut self) {
        for rebuild in &self.rebuilds {
            rebuild.abandon();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::outputs::{Content, OutputFile};

    fn path(text: &str) -> WorkspacePath {
        WorkspacePath::parse(text).unwrap()
    }

    fn file(at: &str, content: &[u8]) -> OutputFile {
        OutputFile {
            path: path(at),
            mode: 0o644,
            size: content.len() as u64,
            digest: Digest::of(content),
        }
    }

    #[test]
    fn a_blob_that_does_not_arrive_never_keeps_the_others_from_coming_back() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("result.txt"), "old").unwrap();
        std::fs::create_dir(root.join("logs")).unwrap();
        std::fs::write(root.join("logs/stale.txt"), "old").unwrap();
        let confined = Confined::open(root).unwrap();
        let changes = vec![
            Changed {
                output: path("result.txt"),
                content: Some(Content {
                    files: vec![file("result.txt", b"new")],
                    ..Default::default()
                }),
            },
            Changed {
                output: path("logs"),
                content: Some(Content {
                    directory: true,
                    files: vec![file("logs/a.txt", b"A"), file("logs/b.txt", b"B")],
                    ..Default::default()
                }),
            },
        ];
        let mut returning = Returning::prepare(&confined, changes);
        for (index, digest) in returning.wanted().into_iter().enumerate() {
            if digest == Digest::of(b"A") {
                let mut state = Receiving::default();
                returning.write(index, b"A", &mut state).unwrap();
                returning.complete(index, &mut state).unwrap();
            } else if digest == Digest::of(b"B") {
                // Part of it arrived before the member gave up on it.
                let mut state = Receiving::default();
                returning.write(index, b"", &mut state).unwrap();
                drop(state);
                returning.fail(index, Cause::TransferError, None);
            } else {
                returning.fail(index, Cause::Unreadable, Some("EIO".into()));
            }
        }
        let returned = returning.finish(&confined);
        assert_eq!(
            std::fs::read_to_string(root.join("result.txt")).unwrap(),
            "old",
            "a single file that did not arrive leaves the caller's version"
        );
        let mut names: Vec<_> = std::fs::read_dir(root.join("logs"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["a.txt"], "the directory comes back without it");
        let leftovers = std::fs::read_dir(root)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".elsecall-")
            })
            .count();
        assert_eq!(leftovers, 0, "no temporaries are left behind");
        let mut warnings = returned.shortfall.warnings();
        warnings.sort();
        assert_eq!(
            warnings,
            [
                "logs/b.txt was not returned: the transfer failed.",
                "result.txt was not returned: it could not be read on the member (EIO).",
            ]
        );
    }
}
