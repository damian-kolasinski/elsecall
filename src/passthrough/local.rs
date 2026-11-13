//! Executes the command as a child of this process, so it inherits the caller's
//! standard streams, environment, working directory and process group exactly.
use crate::{
    client::{self, Wait},
    failure::{Failure, Result},
    macos::process_usage,
    passthrough::{
        executor::{CommandLine, Executor, Exit, Finished, RunContext, RunResult, Running},
        signals,
    },
    run_admission::Admission,
    scheduling::messages::CommandStarted,
};
use std::{
    ffi::OsStr,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::{Child, Command},
    time::{Duration, Instant},
};

/// execvp's search path when PATH is unset.
const DEFAULT_PATH: &str = "/usr/bin:/bin";

/// Runs commands on this machine, binding each command's process to its grant
/// through the local service at `socket`.
pub struct LocalExecutor {
    pub socket: PathBuf,
}

fn executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Finds the program as execvp would: a name with a slash is a path, anything
/// else is searched in PATH, where an empty entry means the working directory.
fn resolve(program: &OsStr) -> Option<PathBuf> {
    if program.as_bytes().contains(&b'/') {
        return executable(Path::new(program)).then(|| program.into());
    }
    if program.is_empty() {
        return None;
    }
    let path = std::env::var_os("PATH").unwrap_or_else(|| DEFAULT_PATH.into());
    path.as_bytes()
        .split(|b| *b == b':')
        .map(|dir| match dir {
            b"" => Path::new(".").join(program),
            dir => Path::new(OsStr::from_bytes(dir)).join(program),
        })
        .find(|candidate| executable(candidate))
}

fn not_startable(detail: &str) -> Failure {
    // Never echo the command: its arguments may hold secrets.
    Failure::new(
        "invalid_request",
        format!("The command could not be started: {detail}."),
        "Check the command name, PATH and permissions; the command goes after --.",
    )
}

/// Fails when the command cannot start at all, as for a typo in its name.
pub fn preflight(command: &CommandLine) -> Result<()> {
    resolve(&command.program)
        .map(|_| ())
        .ok_or_else(|| not_startable("no executable file was found by that name"))
}

/// Spawns the command with the caller's streams and environment plus
/// `variables`, forwarding the caller's signals to it.
fn spawn<'a>(
    command: &CommandLine,
    variables: impl IntoIterator<Item = (&'a str, String)>,
) -> Result<Child> {
    signals::install();
    let child = Command::new(&command.program)
        .args(&command.arguments)
        .envs(variables)
        .spawn()
        .map_err(|e| not_startable(&e.to_string()))?;
    signals::target(child.id());
    Ok(child)
}

/// Starts the command as the caller would, for work that runs without admission.
/// Only `ELSECALL_ADMISSION` is added, so a script that re-execs itself through
/// Elsecall can tell it already did.
pub fn start_directly(command: &CommandLine, admission: Admission) -> Result<LocalRun> {
    let started = Instant::now();
    let child = spawn(command, [admission.variable()])?;
    Ok(LocalRun::observe(child, started))
}

impl Executor for LocalExecutor {
    type Running = LocalRun;

    fn preflight(&self, command: &CommandLine) -> Result<()> {
        preflight(command)
    }

    async fn start(&self, command: &CommandLine, context: &RunContext) -> RunResult<LocalRun> {
        let execution_started = Instant::now();
        let child = spawn(command, context.variables())?;
        let run = LocalRun::observe(child, execution_started);
        // Bind the command before anything else, so killing elsecall cannot free
        // its capacity while it runs. If the service is unreachable the grant
        // still ends when this process exits.
        let route = format!("/v1/runs/{}/command", context.run_id);
        let started = CommandStarted {
            pid: run.child.id(),
        };
        let _ = client::post::<(), _>(&self.socket, &route, &started, Wait::Bounded).await;
        Ok(run)
    }
}

pub struct LocalRun {
    child: Child,
    started: Instant,
    observed_exit: tokio::task::JoinHandle<Option<(Instant, Option<Duration>)>>,
}

impl LocalRun {
    fn observe(child: Child, started: Instant) -> Self {
        let pid = child.id();
        // Notice exit while binding and progress reporting happen, without
        // reaping the child before the service has had a chance to bind it.
        let observed_exit = tokio::task::spawn_blocking(move || {
            process_usage::wait_exited(pid)
                .ok()
                .map(|()| (Instant::now(), process_usage::cpu_time(pid)))
        });
        Self {
            child,
            started,
            observed_exit,
        }
    }
}

impl Running for LocalRun {
    fn warnings(&self) -> Vec<String> {
        Vec::new()
    }

    async fn wait(mut self) -> RunResult<Finished> {
        let (status, cpu, result_at) =
            match self.observed_exit.await.expect("waiting does not panic") {
                Some((at, cpu)) => (self.child.wait(), cpu, at),
                None => {
                    let (status, at) =
                        tokio::task::spawn_blocking(move || (self.child.wait(), Instant::now()))
                            .await
                            .expect("waiting does not panic");
                    (status, None, at)
                }
            };
        signals::finished();
        let status = status.map_err(|e| Failure::io("Wait for the command", e))?;
        let exit = match (status.code(), status.signal()) {
            (Some(code), _) => Exit::Code(code),
            (None, Some(signal)) => Exit::Signal(signal),
            (None, None) => unreachable!("a waited process exited or was signaled"),
        };
        Ok(Finished {
            plan_observation: None,
            exit,
            warnings: Vec::new(),
            cpu,
            execution: result_at.duration_since(self.started),
            result_at: Some(result_at),
            result_return: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::resolve;

    #[test]
    fn resolves_like_execvp() {
        assert!(resolve("sh".as_ref()).is_some());
        assert!(resolve("/bin/sh".as_ref()).is_some());
        assert!(resolve("elsecall-no-such-command".as_ref()).is_none());
        assert!(resolve("/etc/hosts".as_ref()).is_none(), "not executable");
        assert!(resolve("/bin".as_ref()).is_none(), "a directory");
        assert!(resolve("".as_ref()).is_none());
    }
}
