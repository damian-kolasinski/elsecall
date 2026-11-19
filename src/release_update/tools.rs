//! Bounded, non-interactive external tools; output never enters errors or logs.
use elsecall_failure::{Failure, Result};
use elsecall_local_state::paths::Paths;
use elsecall_process::command_capture::{self, Program};
use std::{
    ffi::OsString,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

pub fn mise(paths: &Paths) -> Result<PathBuf> {
    find_mise(paths).ok_or_else(|| {
        Failure::new(
            "mise_unavailable",
            "The release updater requires mise with packslip support.",
            "Install mise, or install the signed release manually and run elsecall install.",
        )
    })
}

/// `ELSECALL_MISE`, else the first of the places mise's installers put it, else
/// the first on `PATH`, where the one-command installer also looks. Every mise
/// edits the same global configuration. The service's launchd `PATH` holds none,
/// so only commands run from a shell find a mise there.
pub fn find_mise(paths: &Paths) -> Option<PathBuf> {
    // As `command -v` in the installer, skip a file named mise that cannot run.
    let candidates: Vec<_> = mise_candidates(&paths.home, std::env::var_os("PATH").as_deref())
        .into_iter()
        .filter(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
        })
        .collect();
    match command_capture::locate(command_capture::path_override("ELSECALL_MISE"), &candidates) {
        command_capture::Located::Found(path) => Some(path),
        _ => None,
    }
}

fn mise_candidates(home: &Path, search: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let on_path = search
        .map(|search| {
            std::env::split_paths(search)
                .filter(|directory| directory.is_absolute())
                .map(|directory| directory.join("mise"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    [
        home.join(".local/bin/mise"),
        home.join(".local/share/mise/bin/mise"),
        PathBuf::from("/opt/homebrew/bin/mise"),
        PathBuf::from("/usr/local/bin/mise"),
    ]
    .into_iter()
    .chain(on_path)
    .collect()
}
pub fn run(
    program: &Path,
    arguments: &[&str],
    directory: &Path,
    timeout: Duration,
    environment: &[(OsString, OsString)],
) -> Result<Vec<u8>> {
    let remove: Vec<_> = std::env::vars_os()
        .filter_map(|(key, _)| {
            let bytes = key.as_encoded_bytes();
            (bytes == b"MISE" || bytes.starts_with(b"MISE_") || bytes == b"ELSECALL_ADMISSION")
                .then_some(key)
        })
        .collect();
    match command_capture::run_with_env(
        Program {
            program,
            arguments,
            directory,
            timeout,
            remove_env: &remove,
        },
        environment,
    ) {
        Ok(result) if result.status.success() => Ok(result.stdout),
        Ok(_) => Err(Failure::new(
            "release_tool_failed",
            "A release download or verification command failed.",
            "Check connectivity and the installed mise version, then retry. Use elsecall update status to inspect progress.",
        )),
        Err(command_capture::CaptureFailure::TimedOut) => Err(Failure::new(
            "release_tool_timeout",
            "A release command exceeded its deadline.",
            "Check connectivity and retry the update.",
        )),
        Err(_) => Err(Failure::new(
            "release_tool_failed",
            "A release command could not run or returned too much output.",
            "Check the mise installation and retry.",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mise_on_the_path_is_the_last_resort_and_relative_entries_are_ignored() {
        let home = Path::new("/Users/me");
        assert_eq!(
            mise_candidates(home, Some("/Users/me/.cargo/bin:bin:/usr/bin".as_ref())),
            [
                "/Users/me/.local/bin/mise",
                "/Users/me/.local/share/mise/bin/mise",
                "/opt/homebrew/bin/mise",
                "/usr/local/bin/mise",
                "/Users/me/.cargo/bin/mise",
                "/usr/bin/mise",
            ]
            .map(PathBuf::from)
        );
        assert_eq!(mise_candidates(home, None).len(), 4);
    }
}
