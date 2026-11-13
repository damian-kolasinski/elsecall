//! Starting child processes without one child inheriting another's pipes.
//!
//! On macOS a new pipe is made close-on-exec only after it exists: neither the
//! standard library nor `pipe` can do both at once. A child started on another
//! thread in that moment inherits the pipe and holds it open for as long as it
//! runs. A `git cat-file` then never sees the end of its input while a sync's
//! `git index-pack` lives, and a placed command's output would not end while a
//! sync's git does.
//!
//! So every start in this process that creates pipes holds [`starting`] from
//! creating them until the child exists. Starting takes milliseconds; waiting for
//! the child happens after the guard is dropped.
use std::sync::{Mutex, MutexGuard};

static STARTING: Mutex<()> = Mutex::new(());

/// Held while creating a child's pipes and starting it.
#[must_use = "the guard protects only while it is held"]
pub fn starting() -> MutexGuard<'static, ()> {
    // The guard protects no data, so a panic while it was held changes nothing.
    STARTING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Starts `command` while holding [`starting`].
pub fn spawn(command: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    let _starting = starting();
    command.spawn()
}

/// Starts `command` while holding [`starting`].
pub fn spawn_async(
    command: &mut tokio::process::Command,
) -> std::io::Result<tokio::process::Child> {
    let _starting = starting();
    command.spawn()
}

/// Runs `command` to its end with its standard output and error captured and no
/// input, as [`std::process::Command::output`] does, started while holding
/// [`starting`].
pub fn output(command: &mut std::process::Command) -> std::io::Result<std::process::Output> {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    spawn(command)?.wait_with_output()
}
