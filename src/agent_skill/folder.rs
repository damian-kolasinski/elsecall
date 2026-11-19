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
}

impl State {
    pub fn ours(self) -> bool {
        matches!(self, State::Current | State::Outdated)
    }
}

pub fn state(folder: &Path) -> io::Result<State> {
    match std::fs::read_to_string(folder.join(FILE)) {
        Ok(text) if text == STUB => Ok(State::Current),
        Ok(text) if text.lines().any(|line| line.starts_with(MARKER)) => Ok(State::Outdated),
        Ok(_) => Ok(State::Foreign),
        Err(e) if e.kind() == io::ErrorKind::NotFound => match std::fs::read_dir(folder) {
            Ok(mut entries) => Ok(match entries.next() {
                Some(_) => State::Foreign,
                None => State::Missing,
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(State::Missing),
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    }
}

/// Writes this build's `SKILL.md`, replacing the file whole.
pub fn write(folder: &Path) -> io::Result<()> {
    std::fs::create_dir_all(folder)?;
    let staged = folder.join(format!(".{FILE}.elsecall-new"));
    std::fs::write(&staged, STUB)?;
    std::fs::rename(&staged, folder.join(FILE))
}

/// Removes the `SKILL.md` Elsecall wrote, and the folder once nothing else is in it.
pub fn remove(folder: &Path) -> io::Result<()> {
    std::fs::remove_file(folder.join(FILE))?;
    match std::fs::remove_dir(folder) {
        Err(e) if e.kind() != io::ErrorKind::DirectoryNotEmpty => Err(e),
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
