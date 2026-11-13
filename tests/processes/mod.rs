//! Subprocess environments belong to each test, even inside an admitted check.
#![allow(dead_code)]
use std::{ffi::OsStr, process::Command};

/// Preserve the toolchain and OS environment, but remove every inherited Elsecall
/// control before a fixture adds its own. Unknown future controls are covered too.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.env("TOKIO_WORKER_THREADS", "2");
    for (name, _) in std::env::vars_os() {
        if name == "ELSECALL" || name.as_encoded_bytes().starts_with(b"ELSECALL_") {
            command.env_remove(name);
        }
    }
    command
}

pub fn async_command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    command(program).into()
}

/// A scheduling allowance, not an expectation about how long an operation takes.
/// Poll the observable condition and fail with diagnostics when it never happens.
pub const WAIT: std::time::Duration = std::time::Duration::from_secs(120);
