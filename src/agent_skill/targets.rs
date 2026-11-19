//! The agents' skill folders `setup` and `remove` act on, and what each did. Two
//! agents whose folders lead to the same place, as Codex and OpenCode do, or
//! through a linked skills folder, are acted on once.
use super::{
    agents::Agent,
    folder::{self, State},
};
use serde::Serialize;
use std::{
    io,
    path::{Path, PathBuf},
};

pub struct Target {
    pub agent: Agent,
    pub folder: PathBuf,
    pub before: State,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Unchanged,
    /// Written or removed; with --dry-run, would be.
    Changed,
    /// Left alone: a skill Elsecall did not write, without --force.
    SkippedForeign,
    /// Left alone: a link, whatever the options.
    SkippedLinked,
}

/// An I/O failure and the skill folder it happened in.
pub type Failed = (PathBuf, io::Error);

pub fn inspect(home: &Path, chosen: &[Agent]) -> Result<Vec<Target>, Failed> {
    chosen
        .iter()
        .map(|&agent| {
            let folder = agent.skills(home).join(folder::NAME);
            let before = folder::state(&folder).map_err(|e| (folder.clone(), e))?;
            Ok(Target {
                agent,
                folder,
                before,
            })
        })
        .collect()
}

/// Where a skill folder really is: its parent resolved through links.
fn place(folder: &Path) -> PathBuf {
    folder
        .parent()
        .and_then(|parent| parent.canonicalize().ok())
        .map_or_else(|| folder.to_owned(), |parent| parent.join(folder::NAME))
}

pub fn setup(targets: &[Target], dry_run: bool, force: bool) -> Result<Vec<Outcome>, Failed> {
    let mut done = Vec::new();
    targets
        .iter()
        .map(|target| {
            let outcome = match target.before {
                State::Current => Outcome::Unchanged,
                State::Missing | State::Outdated => Outcome::Changed,
                State::Foreign if force => Outcome::Changed,
                State::Foreign => Outcome::SkippedForeign,
                State::Linked => Outcome::SkippedLinked,
            };
            let place = place(&target.folder);
            if outcome == Outcome::Changed && !dry_run && !done.contains(&place) {
                folder::write(&target.folder).map_err(|e| (target.folder.clone(), e))?;
                done.push(place);
            }
            Ok(outcome)
        })
        .collect()
}

pub fn remove(targets: &[Target]) -> Result<Vec<Outcome>, Failed> {
    let mut done = Vec::new();
    targets
        .iter()
        .map(|target| {
            if !target.before.ours() {
                return Ok(Outcome::Unchanged);
            }
            let place = place(&target.folder);
            if !done.contains(&place) {
                folder::remove(&target.folder).map_err(|e| (target.folder.clone(), e))?;
                done.push(place);
            }
            Ok(Outcome::Changed)
        })
        .collect()
}
