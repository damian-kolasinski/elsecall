//! The mise tool manager, driven through the `mise` executable.
//!
//! Mise is `ELSECALL_MISE` when that variable is a non-empty absolute path of a
//! file. An empty value is unset, and then the first file among
//! `~/.local/bin/mise`, `/opt/homebrew/bin/mise` and `/usr/local/bin/mise` is
//! used. A non-empty value that is not absolute fails the manager. An absolute
//! path that is not a file hides those defaults and the executable is missing,
//! which is an empty inventory, not a failure. The override is honoured in
//! every build, like `ELSECALL_MACHINE_LOAD`.
//!
//! `HOME` is required before that choice. When it is unset the manager fails,
//! whether or not a mise executable exists.
//!
//! The version is the first token of `mise --version`. Installed tools come from
//! `mise ls --json` and the default from `mise ls --global --json` (see
//! `listing`). Each command runs in the home directory, with stdin closed, with
//! `MISE` and `MISE_*` removed from the environment, and with a 30 second limit.
//! Output is not logged.
mod listing;

use crate::{
    command_capture::{self, Program},
    tool_inventory::{
        Executable, InventoryFailure, Manager, ManagerInventory, ToolManager, version_token,
    },
};
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    time::Duration,
};

const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
pub struct MiseCli;

impl ToolManager for MiseCli {
    fn manager(&self) -> Manager {
        Manager::Mise
    }

    fn inventory(&self) -> Result<ManagerInventory, InventoryFailure> {
        let (home, program) = begin(home_dir(), locate_mise())?;
        let Some(program) = program else {
            return ManagerInventory::try_new(Manager::Mise, Executable::Missing, Vec::new());
        };
        let remove = variables_to_remove();
        let version = run(&program, &["--version"], "mise --version", &home, &remove)?;
        let version = version_token(&version, "mise --version")?;
        let listing = run(
            &program,
            &["ls", "--json"],
            "mise ls --json",
            &home,
            &remove,
        )?;
        let global = run(
            &program,
            &["ls", "--global", "--json"],
            "mise ls --global --json",
            &home,
            &remove,
        )?;
        let tools = listing::installed_tools(&listing, &global)?;
        ManagerInventory::try_new(
            Manager::Mise,
            Executable::Found {
                path: program,
                version,
            },
            tools,
        )
    }
}

fn begin(
    home: Option<PathBuf>,
    located: command_capture::Located,
) -> Result<(PathBuf, Option<PathBuf>), InventoryFailure> {
    let Some(home) = home else {
        return Err(InventoryFailure::failed(
            "HOME is not set, so mise cannot be limited to the global configuration",
        ));
    };
    let program = match located {
        command_capture::Located::NotAbsolute => {
            return Err(InventoryFailure::not_absolute("ELSECALL_MISE"));
        }
        command_capture::Located::Missing => None,
        command_capture::Located::Found(path) => Some(path),
    };
    Ok((home, program))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

fn locate_mise() -> command_capture::Located {
    let mut candidates = Vec::new();
    if let Some(home) = home_dir() {
        candidates.push(home.join(".local/bin/mise"));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin/mise"));
    candidates.push(PathBuf::from("/usr/local/bin/mise"));
    command_capture::locate(command_capture::path_override("ELSECALL_MISE"), &candidates)
}

fn is_mise_variable(name: &OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    bytes == b"MISE" || bytes.starts_with(b"MISE_")
}

fn variables_to_remove() -> Vec<OsString> {
    std::env::vars_os()
        .filter_map(|(name, _)| is_mise_variable(&name).then_some(name))
        .collect()
}

fn run(
    program: &Path,
    arguments: &[&str],
    label: &str,
    home: &Path,
    remove_env: &[OsString],
) -> Result<Vec<u8>, InventoryFailure> {
    match command_capture::run(Program {
        program,
        arguments,
        directory: home,
        timeout: TIMEOUT,
        remove_env,
    }) {
        Ok(captured) if captured.status.success() => Ok(captured.stdout),
        Ok(captured) => Err(InventoryFailure::failed(&exit_detail(
            label,
            captured.status,
        ))),
        Err(command_capture::CaptureFailure::TimedOut) => Err(InventoryFailure::timeout(label)),
        Err(command_capture::CaptureFailure::TooLarge) => Err(InventoryFailure::output_invalid(
            label,
            "the output was larger than 8 MiB",
        )),
        Err(command_capture::CaptureFailure::Start(error)) => Err(InventoryFailure::failed(
            &format!("{label} could not be started: {error}"),
        )),
    }
}

fn exit_detail(label: &str, status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("{label} exited with status {code}"),
        None => format!("{label} was terminated by a signal"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_failure(located: command_capture::Located) -> InventoryFailure {
        begin(None, located).unwrap_err()
    }

    #[test]
    fn mise_variables_are_only_the_mise_prefix() {
        assert!(is_mise_variable(OsStr::new("MISE")));
        assert!(is_mise_variable(OsStr::new("MISE_CONFIG_DIR")));
        assert!(is_mise_variable(OsStr::new("MISE_ENV")));
        assert!(!is_mise_variable(OsStr::new("PATH")));
        assert!(!is_mise_variable(OsStr::new("NOT_MISE")));
        assert!(!is_mise_variable(OsStr::new("mise_config")));
    }

    #[test]
    fn missing_home_fails_whether_or_not_mise_exists() {
        let missing = home_failure(command_capture::Located::Missing);
        let present = home_failure(command_capture::Located::Found(PathBuf::from(
            "/opt/homebrew/bin/mise",
        )));
        let relative = home_failure(command_capture::Located::NotAbsolute);
        for failure in [missing, present, relative] {
            assert_eq!(failure.code(), "tool_manager_failed");
            assert_eq!(
                failure.message(),
                "HOME is not set, so mise cannot be limited to the global configuration."
            );
        }
    }

    #[test]
    fn a_relative_mise_override_fails_and_a_missing_file_does_not() {
        let home = PathBuf::from("/Users/me");
        let failure = begin(Some(home.clone()), command_capture::Located::NotAbsolute).unwrap_err();
        assert_eq!(failure.code(), "tool_manager_failed");
        assert_eq!(failure.hint(), "ELSECALL_MISE must be an absolute path");
        let (resolved, program) =
            begin(Some(home.clone()), command_capture::Located::Missing).unwrap();
        assert_eq!(resolved, home);
        assert!(program.is_none());
    }
}
