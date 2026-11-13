//! Run one external program and keep its stdout, with a deadline.
//!
//! Stdin is closed. Stderr is discarded and neither stream is logged. The child
//! leads its own process group. That group is killed when the run finishes or
//! the deadline passes, including after the leader has already been reaped: a
//! grandchild can still hold the group and the stdout pipe. `wait` is called
//! only when the leader has not already been reaped.
//!
//! Stdout is read only until the original deadline. After the leader exits, a
//! pipe that is still open at that deadline — a grandchild backgrounded onto
//! stdout is the usual reason — makes `run` return [`CaptureFailure::TimedOut`].
//! Bytes written so far are discarded, and the reader is not joined past the
//! deadline. The program path must be absolute. A relative path is refused
//! before the command is started, so it is never resolved through `PATH`.
//! `remove_env` names variables the caller wants removed from the child.
use std::{
    ffi::OsString,
    io::Read,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

/// How much stdout is kept. Anything larger is refused rather than buffered.
pub(crate) const OUTPUT_LIMIT: u64 = 8 * 1024 * 1024;

pub(crate) struct Program<'a> {
    pub program: &'a Path,
    pub arguments: &'a [&'a str],
    pub directory: &'a Path,
    pub timeout: Duration,
    pub remove_env: &'a [OsString],
}

#[derive(Debug)]
pub(crate) struct Captured {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
}

#[derive(Debug)]
pub(crate) enum CaptureFailure {
    Start(std::io::Error),
    TimedOut,
    TooLarge,
}

pub(crate) fn run(program: Program<'_>) -> Result<Captured, CaptureFailure> {
    if !program.program.is_absolute() {
        return Err(CaptureFailure::Start(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "program path is not absolute",
        )));
    }
    let mut command = Command::new(program.program);
    command
        .args(program.arguments)
        .current_dir(program.directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    for name in program.remove_env {
        command.env_remove(name);
    }
    let mut child = crate::child_start::spawn(&mut command).map_err(CaptureFailure::Start)?;
    let pgid = libc::pid_t::try_from(child.id()).ok();
    let mut stdout = child.stdout.take().expect("piped stdout");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stdout).take(OUTPUT_LIMIT + 1).read_to_end(&mut bytes);
        let _ = sender.send(bytes);
    });
    let deadline = Instant::now() + program.timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(25).min(left));
            }
            Err(error) => {
                finish(pgid, &mut child, false);
                return Err(CaptureFailure::Start(error));
            }
        }
    };
    let reaped = status.is_some();
    let stdout = if status.is_some() {
        let left = deadline.saturating_duration_since(Instant::now());
        receiver.recv_timeout(left).ok()
    } else {
        None
    };
    finish(pgid, &mut child, reaped);
    let (Some(status), Some(stdout)) = (status, stdout) else {
        return Err(CaptureFailure::TimedOut);
    };
    if stdout.len() as u64 > OUTPUT_LIMIT {
        return Err(CaptureFailure::TooLarge);
    }
    Ok(Captured { status, stdout })
}

/// An explicit path from the environment, when the variable is set and not empty.
///
/// Honoured in every build, the same way `ELSECALL_MACHINE_LOAD` is: these are
/// not debug-only seams.
pub(crate) fn path_override(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name)?;
    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

pub(crate) enum OverridePath {
    Unset,
    Absolute(PathBuf),
    NotAbsolute,
}

pub(crate) fn classify(value: Option<PathBuf>) -> OverridePath {
    match value {
        None => OverridePath::Unset,
        Some(path) if path.is_absolute() => OverridePath::Absolute(path),
        Some(_) => OverridePath::NotAbsolute,
    }
}

pub(crate) enum Located {
    Found(PathBuf),
    Missing,
    NotAbsolute,
}

/// An absolute override that names a file, otherwise the first absolute
/// candidate that is a file. An absolute override that is not a file hides the
/// candidates. A non-absolute override is [`Located::NotAbsolute`] and also
/// hides them.
pub(crate) fn locate(override_path: Option<PathBuf>, candidates: &[PathBuf]) -> Located {
    match classify(override_path) {
        OverridePath::NotAbsolute => Located::NotAbsolute,
        OverridePath::Absolute(path) => {
            if path.is_file() {
                Located::Found(path)
            } else {
                Located::Missing
            }
        }
        OverridePath::Unset => candidates
            .iter()
            .find(|path| path.is_absolute() && path.is_file())
            .cloned()
            .map(Located::Found)
            .unwrap_or(Located::Missing),
    }
}

fn finish(pgid: Option<libc::pid_t>, child: &mut Child, reaped: bool) {
    if let Some(pgid) = pgid.filter(|pgid| *pgid > 1) {
        // SAFETY: a negative pid names the process group. The child leads that
        // group, and the id remains the right target after the leader is reaped.
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
    if !reaped {
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, time::Instant};

    fn write_script(directory: &Path, name: &str, body: &str) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn capture(
        program: &Path,
        directory: &Path,
        timeout: Duration,
    ) -> Result<Captured, CaptureFailure> {
        run(Program {
            program,
            arguments: &[],
            directory,
            timeout,
            remove_env: &[],
        })
    }

    #[test]
    fn a_missing_override_hides_a_real_program_and_a_relative_one_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let real = write_script(dir.path(), "mise", "#!/bin/sh\n");
        let missing = dir.path().join("missing");
        assert!(matches!(
            locate(Some(missing.clone()), std::slice::from_ref(&real)),
            Located::Missing
        ));
        assert!(matches!(
            locate(Some(real.clone()), std::slice::from_ref(&missing)),
            Located::Found(path) if path == real
        ));
        assert!(matches!(
            locate(None, &[missing, real.clone()]),
            Located::Found(path) if path == real
        ));
        assert!(matches!(
            locate(Some(PathBuf::from("mise")), std::slice::from_ref(&real)),
            Located::NotAbsolute
        ));
        assert!(matches!(
            classify(Some(PathBuf::from("/opt/homebrew/bin/mise"))),
            OverridePath::Absolute(_)
        ));
        assert!(matches!(classify(None), OverridePath::Unset));
    }

    #[test]
    fn a_relative_program_is_not_searched_on_path() {
        let error = capture(Path::new("true"), Path::new("/"), Duration::from_secs(1)).unwrap_err();
        assert!(matches!(error, CaptureFailure::Start(_)));
    }

    #[test]
    fn captures_stdout_and_stops_a_program_that_overruns_its_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let speaker = write_script(dir.path(), "speak.sh", "#!/bin/sh\nprintf 'hello'\n");
        let captured = capture(&speaker, dir.path(), Duration::from_secs(5)).unwrap();
        assert!(captured.status.success());
        assert_eq!(captured.stdout, b"hello");

        let sleeper = write_script(dir.path(), "sleep.sh", "#!/bin/sh\nsleep 30\n");
        let started = Instant::now();
        let error = capture(&sleeper, dir.path(), Duration::from_millis(200)).unwrap_err();
        assert!(matches!(error, CaptureFailure::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn stdout_is_kept_when_the_pipe_closes_before_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let brief = write_script(
            dir.path(),
            "brief.sh",
            "#!/bin/sh\nprintf 'hello\\n' >/dev/stdout &\nexit 0\n",
        );
        let captured = capture(&brief, dir.path(), Duration::from_secs(2)).unwrap();
        assert!(captured.status.success());
        assert_eq!(captured.stdout, b"hello\n");
    }

    #[test]
    fn a_grandchild_holding_stdout_times_out_within_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let hold = write_script(
            dir.path(),
            "hold.sh",
            "#!/bin/sh\nprintf 'hello\\n'\nsleep 30 >/dev/stdout &\nexit 0\n",
        );
        let started = Instant::now();
        let error = capture(&hold, dir.path(), Duration::from_millis(400)).unwrap_err();
        assert!(matches!(error, CaptureFailure::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
