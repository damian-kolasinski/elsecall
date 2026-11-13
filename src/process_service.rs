//! A service manager that runs the installed service as a plain background process,
//! for tests that install and upgrade real services in temporary homes without
//! registering LaunchAgents. `ELSECALL_SERVICE_MANAGER=process` selects it in debug
//! builds only; release builds always use launchd. It keeps nothing alive after a
//! crash, which launchd would.
use crate::{
    failure::{Failure, Result},
    installation::{ServiceManager, Session},
    paths::Paths,
    private_files,
};
use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub const ENVIRONMENT: &str = "ELSECALL_SERVICE_MANAGER";
/// The process ID of the running service, in the state directory.
pub const PID_FILE: &str = "process-service.pid";

/// Whether the environment asks for this manager instead of launchd. A release
/// build refuses rather than falling back to launchd, which would register real
/// LaunchAgents for whatever home a test gave it.
pub fn requested() -> Result<bool> {
    match std::env::var(ENVIRONMENT) {
        Err(_) => Ok(false),
        Ok(value) if value == "process" && cfg!(debug_assertions) => Ok(true),
        Ok(_) => Err(Failure::new(
            "invalid_service_manager",
            format!("{ENVIRONMENT} is honoured only as `process`, and only by debug builds."),
            format!("Unset {ENVIRONMENT} to use launchd."),
        )),
    }
}

pub struct ProcessService {
    paths: Paths,
}
impl ProcessService {
    pub fn new(paths: Paths) -> Self {
        crate::service_log!(
            "{}",
            serde_json::json!({"event": "test_seam_in_use", "detail": "ELSECALL_SERVICE_MANAGER=process runs the service as a plain process instead of a LaunchAgent"})
        );
        Self { paths }
    }
    fn pid_file(&self) -> PathBuf {
        self.paths.data.join(PID_FILE)
    }
    /// The recorded service process, if it still runs the installed executable, so
    /// a reused process ID is never signalled.
    fn pid(&self) -> Option<i32> {
        let pid = std::fs::read_to_string(self.pid_file())
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let installed = std::fs::canonicalize(self.paths.executable()).ok()?;
        (executable_of(pid)? == installed).then_some(pid)
    }
}

/// The path of the executable process `pid` runs.
fn executable_of(pid: i32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut buffer = vec![0u8; 4096];
    // SAFETY: the buffer is valid for the length passed.
    let length =
        unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if length <= 0 {
        return None;
    }
    buffer.truncate(length as usize);
    Some(PathBuf::from(std::ffi::OsString::from_vec(buffer)))
}

fn alive(pid: i32) -> bool {
    // SAFETY: kill with signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn failed(message: &str) -> Failure {
    Failure::new(
        "service_manager_error",
        message,
        "Inspect the service log in the Elsecall state directory.",
    )
}

impl ServiceManager for ProcessService {
    fn configuration(&self, _: &Paths, session: Session) -> Result<Vec<u8>> {
        Ok(match session {
            Session::Desktop => b"elsecall-process-service/1 desktop\n".to_vec(),
            Session::Background => b"elsecall-process-service/1 background\n".to_vec(),
        })
    }
    fn configured_session(&self, configuration: &[u8]) -> Option<Session> {
        match configuration {
            b"elsecall-process-service/1 desktop\n" => Some(Session::Desktop),
            b"elsecall-process-service/1 background\n" => Some(Session::Background),
            _ => None,
        }
    }
    fn desktop_available(&self) -> Result<bool> {
        Ok(false)
    }
    fn loaded(&self) -> Result<bool> {
        Ok(self.pid().is_some_and(alive))
    }
    fn start(&self, _: &Path, _: Session) -> Result<()> {
        // The shell starts the service in the background and exits, so the service
        // belongs to no caller that could leave it unreaped.
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "\"$0\" serve </dev/null >/dev/null 2>&1 & echo $!"])
            .arg(self.paths.executable())
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: setsid is async-signal-safe and touches no memory of this process.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let output = crate::child_start::output(&mut command)
            .map_err(|e| Failure::io("Start the service process", e))?;
        let pid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !output.status.success() || pid.parse::<i32>().is_err() {
            return Err(failed("The service process did not start."));
        }
        private_files::write(&self.pid_file(), format!("{pid}\n").as_bytes(), 0o600)
    }
    fn stop(&self) -> Result<()> {
        let Some(pid) = self.pid() else {
            return Ok(());
        };
        // SAFETY: kill has no memory-safety preconditions.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(10);
        while alive(pid) {
            if Instant::now() > deadline {
                // SAFETY: as above.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        private_files::remove_file(&self.pid_file())
    }
}
