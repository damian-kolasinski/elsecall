//! The skill folder `setup` writes: one `SKILL.md` that points the agent at
//! `elsecall skill show`. Elsecall replaces or removes only a `SKILL.md` it wrote,
//! which it recognises by its marker line.
use serde::Serialize;
use std::{io, path::Path};

/// The skill's name, and its folder's name in an agent's skills folder.
pub const NAME: &str = "elsecall";
const FILE: &str = "SKILL.md";
const MARKER: &str = "<!-- Written by `elsecall skill setup`";

pub const STUB: &str = "---
name: elsecall
description: Run builds, tests and other heavy commands through Elsecall, which admits work when this Mac has room and can place it on another Mac in the fleet, and adopt it in a repository's scripts. Use when a repository's scripts call `elsecall` or source an Elsecall guard, when asked to run work on another fleet member, or when integrating a project with Elsecall.
---
<!-- Written by `elsecall skill setup`; running it again updates it, and `elsecall skill remove` removes it. -->

# Elsecall

Run `elsecall skill show` and follow the guide it prints. It comes from the installed `elsecall`, so it describes that version's commands; `elsecall skill show NAME` prints each topic the guide names.
";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// No skill named elsecall.
    Missing,
    /// What this build's `setup` writes.
    Current,
    /// What another build's `setup` wrote.
    Outdated,
    /// A skill named elsecall that Elsecall did not write.
    Foreign,
    /// A folder or `SKILL.md` that is a link to somewhere else. Elsecall never
    /// writes through it or removes it, even with --force.
    Linked,
}

impl State {
    pub fn ours(self) -> bool {
        matches!(self, State::Current | State::Outdated)
    }
}

/// Where `write` stages the new file; a leftover from an interrupted write is ours.
const STAGED: &str = ".SKILL.md.elsecall-new";

fn is_link(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.file_type().is_symlink()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// A folder or `SKILL.md` that is a link is someone else's arrangement: Elsecall
/// writes and removes only plain files in a plain folder.
pub fn state(folder: &Path) -> io::Result<State> {
    if is_link(folder)? || is_link(&folder.join(FILE))? {
        return Ok(State::Linked);
    }
    match std::fs::read_to_string(folder.join(FILE)) {
        Ok(text) if text == STUB => Ok(State::Current),
        Ok(text) if text.lines().any(|line| line.starts_with(MARKER)) => Ok(State::Outdated),
        Ok(_) => Ok(State::Foreign),
        Err(e) if e.kind() == io::ErrorKind::NotFound => match std::fs::read_dir(folder) {
            Ok(entries) => {
                for entry in entries {
                    if entry?.file_name() != STAGED {
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

/// Writes this build's `SKILL.md`, replacing the file whole.
pub fn write(folder: &Path) -> io::Result<()> {
    std::fs::create_dir_all(folder)?;
    let staged = folder.join(STAGED);
    // A fresh file, never one a leftover or a link would put elsewhere.
    gone(std::fs::remove_file(&staged))?;
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
        .and_then(|mut file| io::Write::write_all(&mut file, STUB.as_bytes()))
        .and_then(|()| std::fs::rename(&staged, folder.join(FILE)));
    if written.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    written
}

/// Removes what Elsecall wrote, and the folder once nothing else is in it. A
/// file already gone counts as removed.
pub fn remove(folder: &Path) -> io::Result<()> {
    gone(std::fs::remove_file(folder.join(FILE)))?;
    gone(std::fs::remove_file(folder.join(STAGED)))?;
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

    #[test]
    fn a_folder_is_missing_then_current_then_outdated_after_another_build() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join(NAME);
        assert_eq!(state(&folder).unwrap(), State::Missing);
        std::fs::create_dir(&folder).unwrap();
        assert_eq!(state(&folder).unwrap(), State::Missing, "an empty folder");
        write(&folder).unwrap();
        assert_eq!(state(&folder).unwrap(), State::Current);
        let older = STUB.replace("each topic", "every reference");
        std::fs::write(folder.join(FILE), older).unwrap();
        assert_eq!(state(&folder).unwrap(), State::Outdated);
        write(&folder).unwrap();
        assert_eq!(std::fs::read_to_string(folder.join(FILE)).unwrap(), STUB);
        remove(&folder).unwrap();
        assert!(!folder.exists());
    }

    #[test]
    fn a_leftover_of_an_interrupted_write_is_still_ours() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join(NAME);
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join(STAGED), "half").unwrap();
        assert_eq!(state(&folder).unwrap(), State::Missing);
        write(&folder).unwrap();
        assert_eq!(state(&folder).unwrap(), State::Current);
        assert!(!folder.join(STAGED).exists());
        std::fs::write(folder.join(STAGED), "half").unwrap();
        remove(&folder).unwrap();
        assert!(!folder.exists());
        remove(&folder).unwrap();
    }

    #[test]
    fn links_are_foreign_and_a_staged_link_is_never_written_through() {
        let home = tempfile::tempdir().unwrap();
        let elsewhere = home.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let linked = home.path().join("linked").join(NAME);
        std::fs::create_dir(linked.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &linked).unwrap();
        assert_eq!(state(&linked).unwrap(), State::Linked, "a linked folder");

        let folder = home.path().join(NAME);
        std::fs::create_dir(&folder).unwrap();
        let outside = home.path().join("outside.md");
        std::os::unix::fs::symlink(&outside, folder.join(FILE)).unwrap();
        assert_eq!(state(&folder).unwrap(), State::Linked, "a linked SKILL.md");

        std::fs::remove_file(folder.join(FILE)).unwrap();
        std::os::unix::fs::symlink(&outside, folder.join(STAGED)).unwrap();
        write(&folder).unwrap();
        assert!(
            !outside.exists(),
            "the staged link was replaced, not followed"
        );
        assert_eq!(state(&folder).unwrap(), State::Current);
    }

    #[test]
    fn a_skill_elsecall_did_not_write_is_foreign_and_its_files_stay() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join(NAME);
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("notes.md"), "mine").unwrap();
        assert_eq!(
            state(&folder).unwrap(),
            State::Foreign,
            "files but no SKILL.md"
        );
        std::fs::write(folder.join(FILE), "---\nname: elsecall\n---\nmine\n").unwrap();
        assert_eq!(state(&folder).unwrap(), State::Foreign);
        assert!(!state(&folder).unwrap().ours());
        write(&folder).unwrap();
        remove(&folder).unwrap();
        assert_eq!(
            std::fs::read_to_string(folder.join("notes.md")).unwrap(),
            "mine"
        );
    }
}
