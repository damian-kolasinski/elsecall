//! Bounded, non-interactive external tools; output never enters errors or logs.
use crate::{
    command_capture::{self, Program},
    failure::{Failure, Result},
    paths::Paths,
};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

pub fn mise(paths: &Paths) -> Result<PathBuf> {
    let candidates = [
        paths.home.join(".local/bin/mise"),
        paths.home.join(".local/share/mise/bin/mise"),
        PathBuf::from("/opt/homebrew/bin/mise"),
        PathBuf::from("/usr/local/bin/mise"),
    ];
    match command_capture::locate(command_capture::path_override("ELSECALL_MISE"), &candidates) {
        command_capture::Located::Found(path) => Ok(path),
        _ => Err(Failure::new(
            "mise_unavailable",
            "The release updater requires mise with packslip support.",
            "Install mise, or install the signed release manually and run elsecall install.",
        )),
    }
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
