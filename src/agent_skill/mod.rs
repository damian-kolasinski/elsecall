//! The skill coding agents use to learn Elsecall. The guide is embedded in the
//! binary, so it always matches the installed build. `elsecall skill setup`, which
//! `elsecall install` runs, puts a stub where each agent loads skills that points
//! at `elsecall skill show`, and the service refreshes stubs an older build wrote
//! when it starts on a new one.
mod agents;
pub mod cli;
mod folder;
mod guide;
mod stub;
mod targets;

use crate::paths::Paths;
use serde_json::Value;
use std::path::Path;

/// What `elsecall install` does for agents: set up every agent found. A skill it
/// must leave alone is reported, not a failed installation.
pub fn set_up_found(home: &Path) -> Value {
    let found: Vec<agents::Agent> = agents::ALL.into_iter().filter(|a| a.found(home)).collect();
    cli::setup(home, &found, false, false).0
}

/// What `elsecall uninstall` does for agents: remove every stub and exported copy
/// Elsecall wrote into an agent's skills folder, and the copy `skill install`
/// wrote by default. The flag is whether nothing failed.
pub fn remove_all(paths: &Paths) -> (Value, bool) {
    let (mut data, mut complete) = cli::remove(&paths.home, &agents::ALL, false);
    let default = paths.data.join("skill");
    let copy = default.join(folder::NAME);
    match folder::remove(&copy) {
        Ok(()) => {
            let _ = std::fs::remove_dir(&default);
        }
        Err(e) => {
            complete = false;
            data["default_copy_error"] = Value::String(format!("{}: {e}", copy.display()));
        }
    }
    (data, complete)
}

pub use cli::health;

/// Brings stubs an older build wrote up to this build's, when the service starts
/// from the installed executable; a development or test build never does.
/// Returns how many folders it updated.
pub fn refresh_if_installed(paths: &Paths) -> usize {
    let installed = |path: &Path| path.canonicalize().ok();
    let running = std::env::current_exe().ok().and_then(|exe| installed(&exe));
    if running.is_none() || running != installed(&paths.executable()) {
        return 0;
    }
    refresh(&paths.home)
}

/// Brings stubs an older build wrote up to this build's, in every agent's
/// folder; installs nothing where nothing was.
pub fn refresh(home: &Path) -> usize {
    let targets = targets::inspect(home, &agents::ALL);
    targets::refresh(&targets)
        .iter()
        .filter(|outcome| outcome.done == targets::Done::Updated)
        .count()
}
