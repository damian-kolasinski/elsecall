//! The agents' skill folders `setup`, refresh and `remove` act on, and what each
//! did. Two agents whose folders lead to the same place, as Codex and OpenCode
//! do, or through a linked skills folder, are acted on once. A folder that fails
//! is reported and the others go ahead.
use super::{
    agents::Agent,
    folder::{self, State},
    stub,
};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub(crate) struct Target {
    pub agent: Agent,
    pub folder: PathBuf,
    /// The folder's state before any change, or why it could not be read.
    pub before: Result<State, String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Done {
    Installed,
    Updated,
    Unchanged,
    Removed,
    /// Left alone: someone else's skill without --force, or a link.
    Conflict,
    Failed,
}

/// What happened to one target; with --dry-run, what would have.
pub(crate) struct Outcome {
    pub done: Done,
    pub error: Option<String>,
}

impl Outcome {
    fn of(done: Done) -> Self {
        Self { done, error: None }
    }
    fn failed(error: impl ToString) -> Self {
        Self {
            done: Done::Failed,
            error: Some(error.to_string()),
        }
    }
}

pub(crate) fn inspect(home: &Path, chosen: &[Agent]) -> Vec<Target> {
    chosen
        .iter()
        .map(|&agent| {
            let folder = agent.skills(home).join(folder::NAME);
            let before = folder::state(&folder).map_err(|e| e.to_string());
            Target {
                agent,
                folder,
                before,
            }
        })
        .collect()
}

/// Where a skill folder really is: its nearest existing ancestor resolved through
/// links, with the rest of the path after it.
fn place(folder: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut at = folder;
    while let Some(parent) = at.parent() {
        rest.push(at.file_name().expect("a named folder").to_owned());
        if let Ok(real) = parent.canonicalize() {
            return rest.iter().rev().fold(real, |path, part| path.join(part));
        }
        at = parent;
    }
    folder.to_owned()
}

/// Acts on each target once per real folder: `act` decides from the state what
/// to do, and `apply` does it; a target sharing a folder already handled gets the
/// same outcome without a second write.
fn each(
    targets: &[Target],
    dry_run: bool,
    act: impl Fn(State) -> Done,
    apply: impl Fn(&Target, State) -> std::io::Result<()>,
) -> Vec<Outcome> {
    let mut handled: Vec<(PathBuf, Done, Option<String>)> = Vec::new();
    targets
        .iter()
        .map(|target| {
            let state = match &target.before {
                Ok(state) => *state,
                Err(error) => return Outcome::failed(error),
            };
            let done = act(state);
            if !matches!(done, Done::Installed | Done::Updated | Done::Removed) || dry_run {
                return Outcome::of(done);
            }
            let place = place(&target.folder);
            if let Some((_, done, error)) = handled.iter().find(|(p, _, _)| *p == place) {
                return Outcome {
                    done: *done,
                    error: error.clone(),
                };
            }
            let outcome = match apply(target, state) {
                Ok(()) => Outcome::of(done),
                Err(e) => Outcome::failed(e),
            };
            handled.push((place, outcome.done, outcome.error.clone()));
            outcome
        })
        .collect()
}

fn write(target: &Target, state: State) -> std::io::Result<()> {
    match state {
        // An exported copy goes whole, so none of its references stay behind.
        State::Exported => {
            folder::replace(&target.folder, &[("SKILL.md".to_owned(), stub::text())])
        }
        _ => folder::write_stub(&target.folder),
    }
}

pub(crate) fn setup(targets: &[Target], dry_run: bool, force: bool) -> Vec<Outcome> {
    each(
        targets,
        dry_run,
        |state| match state {
            State::Missing => Done::Installed,
            State::Outdated | State::Exported => Done::Updated,
            State::Current => Done::Unchanged,
            State::Foreign if force => Done::Updated,
            State::Foreign | State::Linked => Done::Conflict,
        },
        write,
    )
}

/// What an upgrade does by itself: an older build's stub becomes this build's.
/// Nothing is installed where nothing was, and an exported copy, someone else's
/// skill or a newer build's stub is left as it is.
pub(crate) fn refresh(targets: &[Target]) -> Vec<Outcome> {
    each(
        targets,
        false,
        |state| match state {
            State::Outdated => Done::Updated,
            _ => Done::Unchanged,
        },
        write,
    )
}

pub(crate) fn remove(targets: &[Target], dry_run: bool) -> Vec<Outcome> {
    each(
        targets,
        dry_run,
        |state| {
            if state.ours() {
                Done::Removed
            } else {
                Done::Unchanged
            }
        },
        |target, _| folder::remove(&target.folder),
    )
}
