//! One skill folder named `elsecall`: what is in it, and writing or removing what
//! Elsecall puts there. Elsecall writes and removes only its own files, in a plain
//! folder, and never follows a link.
use super::{
    guide,
    stub::{self, Owner},
};
use serde::Serialize;
use std::{io, path::Path};

/// The skill's name, and its folder's name in an agent's skills folder.
pub const NAME: &str = "elsecall";
const FILE: &str = "SKILL.md";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// No skill: no folder, or one holding only hidden files such as `.DS_Store`.
    Missing,
    /// This build's stub, or a newer build's.
    Current,
    /// An older build's stub.
    Outdated,
    /// A whole copy `elsecall skill install` exported.
    Exported,
    /// A skill named elsecall that Elsecall did not write.
    Foreign,
    /// The folder or its `SKILL.md` is a link to somewhere else.
    Linked,
}

impl State {
    pub fn ours(self) -> bool {
        matches!(self, State::Current | State::Outdated | State::Exported)
    }
}

fn is_link(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.file_type().is_symlink()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn hidden(name: &std::ffi::OsStr) -> bool {
    name.as_encoded_bytes().starts_with(b".")
}

pub fn state(folder: &Path) -> io::Result<State> {
    if is_link(folder)? || is_link(&folder.join(FILE))? {
        return Ok(State::Linked);
    }
    match std::fs::read_to_string(folder.join(FILE)) {
        Ok(text) => Ok(match stub::owner(&text) {
            Owner::Stub(_) if text == stub::text() => State::Current,
            Owner::Stub(version) if version > stub::VERSION => State::Current,
            Owner::Stub(_) => State::Outdated,
            Owner::Exported => State::Exported,
            Owner::Foreign => State::Foreign,
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => match std::fs::read_dir(folder) {
            Ok(entries) => {
                for entry in entries {
                    if !hidden(&entry?.file_name()) {
                        return Ok(State::Foreign);
                    }
                }
                Ok(State::Missing)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(State::Missing),
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    }
}

fn gone(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// A fresh file, never one a leftover or a link would put elsewhere.
fn create(path: &Path, text: &str) -> io::Result<()> {
    gone(std::fs::remove_file(path))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    io::Write::write_all(&mut file, text.as_bytes())
}

/// Where a write stages its file; the process ID keeps concurrent runs apart,
/// and a leftover of an interrupted write is recognised by the prefix.
const STAGED: &str = ".SKILL.md.elsecall-new";

/// Files no one keeps on purpose: Finder's `.DS_Store` and Elsecall's leftovers.
/// Removing a skill deletes these; every other file someone put there stays.
fn junk(name: &std::ffi::OsStr) -> bool {
    let name = name.as_encoded_bytes();
    name == b".DS_Store" || name.starts_with(STAGED.as_bytes())
}

/// Writes the stub as the folder's `SKILL.md`, replacing that file whole and
/// leaving any other file in the folder.
pub fn write_stub(folder: &Path) -> io::Result<()> {
    std::fs::create_dir_all(folder)?;
    let staged = folder.join(format!("{STAGED}-{}", std::process::id()));
    let written =
        create(&staged, &stub::text()).and_then(|()| std::fs::rename(&staged, folder.join(FILE)));
    if written.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    written
}

/// Puts `files` in place as the whole folder: written beside it, then renamed
/// over it, so an agent never reads half a folder. What Elsecall wrote in the
/// old folder goes; anything else in it moves into the new one. Only for a
/// folder that is missing or Elsecall's own.
pub fn replace(folder: &Path, files: &[(String, String)]) -> io::Result<()> {
    let parent = folder.parent().expect("a skill folder has a parent");
    std::fs::create_dir_all(parent)?;
    let staging = |what: &str| parent.join(format!(".{NAME}.{what}-{}", std::process::id()));
    let (fresh, old) = (staging("new"), staging("old"));
    let _ = std::fs::remove_dir_all(&fresh);
    let result = (|| {
        for (path, text) in files {
            let path = fresh.join(path);
            std::fs::create_dir_all(path.parent().expect("inside the folder"))?;
            create(&path, text)?;
        }
        let state = state(folder)?;
        match std::fs::rename(folder, &old) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        std::fs::rename(&fresh, folder)?;
        if old.exists() {
            remove_ours(&old, state)?;
            for entry in std::fs::read_dir(&old)? {
                let entry = entry?;
                let kept = folder.join(entry.file_name());
                if !kept.exists() {
                    std::fs::rename(entry.path(), kept)?;
                }
            }
            gone(std::fs::remove_dir(&old))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&fresh);
        if !folder.exists() {
            let _ = std::fs::rename(&old, folder);
        }
    }
    result
}

/// The whole skill, as `elsecall skill install` exports it.
pub fn exported() -> Vec<(String, String)> {
    guide::files()
        .into_iter()
        .map(|(path, text)| (path, text.to_owned()))
        .collect()
}

/// Deletes what Elsecall wrote in `folder`, given its state: the `SKILL.md`, an
/// exported copy's references (never through a link), and junk.
fn remove_ours(folder: &Path, state: State) -> io::Result<()> {
    if state.ours() {
        gone(std::fs::remove_file(folder.join(FILE)))?;
    }
    if state == State::Exported && !is_link(&folder.join("references"))? {
        for (path, _) in exported().iter().filter(|(path, _)| path != FILE) {
            gone(std::fs::remove_file(folder.join(path)))?;
        }
        let _ = std::fs::remove_dir(folder.join("references"));
    }
    match std::fs::read_dir(folder) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if junk(&entry.file_name()) {
                    gone(std::fs::remove_file(entry.path()))?;
                }
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Removes what Elsecall wrote, then the folder if nothing else is left in it. A
/// file already gone counts as removed.
pub fn remove(folder: &Path) -> io::Result<()> {
    let state = state(folder)?;
    if !state.ours() {
        return Ok(());
    }
    remove_ours(folder, state)?;
    match std::fs::remove_dir(folder) {
        Err(e)
            if !matches!(
                e.kind(),
                io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
            ) =>
        {
            Err(e)
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn folder() -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join(NAME);
        (home, folder)
    }

    #[test]
    fn a_stub_is_missing_then_current_then_outdated_after_an_older_build() {
        let (_home, folder) = folder();
        assert_eq!(state(&folder).unwrap(), State::Missing);
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join(".DS_Store"), "").unwrap();
        assert_eq!(state(&folder).unwrap(), State::Missing, "only hidden files");
        write_stub(&folder).unwrap();
        assert_eq!(state(&folder).unwrap(), State::Current);
        let older = stub::text()
            .replace("elsecall-stub: \"1\"", "elsecall-stub: \"0\"")
            .replace("never retry", "do not retry");
        std::fs::write(folder.join(FILE), &older).unwrap();
        assert_eq!(state(&folder).unwrap(), State::Outdated);
        let newer = older.replace("elsecall-stub: \"0\"", "elsecall-stub: \"9\"");
        std::fs::write(folder.join(FILE), newer).unwrap();
        assert_eq!(state(&folder).unwrap(), State::Current, "a newer build's");
        remove(&folder).unwrap();
        assert!(!folder.exists(), "the hidden file went with it");
    }

    #[test]
    fn an_exported_copy_is_ours_and_a_stub_replaces_it_whole() {
        let (_home, folder) = folder();
        replace(&folder, &exported()).unwrap();
        assert_eq!(state(&folder).unwrap(), State::Exported);
        assert!(folder.join("references/integrate.md").is_file());
        replace(&folder, &[(FILE.to_owned(), stub::text())]).unwrap();
        assert_eq!(state(&folder).unwrap(), State::Current);
        assert!(!folder.join("references").exists());
        let siblings: Vec<_> = std::fs::read_dir(folder.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(siblings, [NAME], "no staging folder left behind");
    }

    #[test]
    fn hidden_files_someone_keeps_survive_setup_replace_and_remove() {
        let (_home, folder) = folder();
        std::fs::create_dir_all(folder.join(".git")).unwrap();
        std::fs::write(folder.join(".git/HEAD"), "ref").unwrap();
        assert_eq!(state(&folder).unwrap(), State::Missing);
        write_stub(&folder).unwrap();
        remove(&folder).unwrap();
        assert!(folder.join(".git/HEAD").is_file(), "remove kept it");
        replace(&folder, &exported()).unwrap();
        assert_eq!(state(&folder).unwrap(), State::Exported);
        assert!(folder.join(".git/HEAD").is_file(), "replace moved it along");
        replace(&folder, &[(FILE.to_owned(), stub::text())]).unwrap();
        assert!(folder.join(".git/HEAD").is_file());
        assert!(!folder.join("references").exists());
    }

    #[test]
    fn a_linked_references_folder_is_never_followed() {
        let (home, folder) = folder();
        replace(&folder, &exported()).unwrap();
        let elsewhere = home.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("integrate.md"), "theirs").unwrap();
        std::fs::remove_dir_all(folder.join("references")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, folder.join("references")).unwrap();
        remove(&folder).unwrap();
        assert_eq!(
            std::fs::read_to_string(elsewhere.join("integrate.md")).unwrap(),
            "theirs"
        );
    }

    #[test]
    fn a_skill_elsecall_did_not_write_is_foreign_and_its_files_stay() {
        let (_home, folder) = folder();
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("notes.md"), "mine").unwrap();
        assert_eq!(
            state(&folder).unwrap(),
            State::Foreign,
            "files but no SKILL.md"
        );
        std::fs::write(folder.join(FILE), "---\nname: elsecall\n---\nmine\n").unwrap();
        assert_eq!(state(&folder).unwrap(), State::Foreign);
        write_stub(&folder).unwrap();
        remove(&folder).unwrap();
        assert_eq!(
            std::fs::read_to_string(folder.join("notes.md")).unwrap(),
            "mine"
        );
    }

    #[test]
    fn links_are_linked_and_a_staged_link_is_never_written_through() {
        let (home, folder) = folder();
        let elsewhere = home.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &folder).unwrap();
        assert_eq!(state(&folder).unwrap(), State::Linked, "a linked folder");
        std::fs::remove_file(&folder).unwrap();

        std::fs::create_dir(&folder).unwrap();
        let outside = home.path().join("outside.md");
        std::os::unix::fs::symlink(&outside, folder.join(FILE)).unwrap();
        assert_eq!(state(&folder).unwrap(), State::Linked, "a linked SKILL.md");
        std::fs::remove_file(folder.join(FILE)).unwrap();
        std::os::unix::fs::symlink(
            &outside,
            folder.join(format!("{STAGED}-{}", std::process::id())),
        )
        .unwrap();
        write_stub(&folder).unwrap();
        assert!(
            !outside.exists(),
            "the staged link was replaced, not followed"
        );
        assert_eq!(state(&folder).unwrap(), State::Current);
    }
}
