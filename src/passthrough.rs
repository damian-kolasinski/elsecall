//! `elsecall run`: admits a command, runs it, and passes its input, output,
//! signals and exit code through as if it ran directly. Elsecall's own failures
//! exit 125 after one agent-ready line, with the standard envelope as the last
//! line of stderr.
pub mod executor;
pub mod local;
pub mod remote;
mod signals;
pub mod status_file;

use crate::run_admission::Admission;
use crate::{
    admission_cli::{
        self,
        environment::{CallerEnvironment, Mode},
        options::{CallerOptions, RequestOptions},
        resolution::{Layers, resolve},
    },
    api::SCHEMA_VERSION,
    client::{self, Wait},
    consumer_configuration::ConsumerConfiguration,
    failure::{Failure, Result},
    guidance::{self, Unmanaged},
    local_access,
    paths::Paths,
    run_id_file::RunIdFile,
    scheduling::{
        messages::{Grant, RunEnded},
        placed::WorkspaceRef,
        request::{Kind, Placement},
        run_phases::{RemoteFailurePhase, RunPhases},
        timestamp::Timestamp,
    },
    workspace::{self, push_cache::CacheFiles},
};
use clap::Args;
use executor::{CommandLine, Executor, Exit, Finished, RunContext, RunFailure, Running};
use local::LocalExecutor;
use remote::RemoteExecutor;
use serde::Serialize;
use status_file::{Status, StatusFile, Subject};
use std::{
    ffi::{OsStr, OsString},
    io::Write,
    path::PathBuf,
    process::ExitCode,
};

/// Elsecall's exit code for its own failures, as `env`, `timeout` and `xargs` use.
pub const OWN_FAILURE: u8 = 125;

#[derive(Args, Debug)]
pub struct RunArgs {
    #[command(flatten)]
    pub options: RequestOptions,
    #[command(flatten)]
    pub caller: CallerOptions,
    /// Must run on this machine, even if the label rule makes it placeable
    #[arg(long, conflicts_with_all = ["placeable", "member"])]
    pub pinned: bool,
    /// May run on any member that can host it; with no fleet yet, runs here
    #[arg(long, conflicts_with = "member")]
    pub placeable: bool,
    /// Must run on this member (placeable work); with no fleet yet, only this
    /// machine is reachable
    #[arg(long, value_name = "NAME")]
    pub member: Option<String>,
    /// Write the outcome as JSON here: `data.exit_code` for the command's exit,
    /// or `error` for Elsecall's own failure
    #[arg(long, value_name = "PATH")]
    pub status_file: Option<PathBuf>,
    /// The command and its arguments, after --
    #[arg(required = true, trailing_var_arg = true, value_name = "COMMAND")]
    pub command: Vec<OsString>,
}

/// The standard error envelope, with `run_id` always present.
#[derive(Serialize)]
pub struct Envelope<'a> {
    schema_version: u32,
    error: EnvelopeError<'a>,
}
#[derive(Serialize)]
struct EnvelopeError<'a> {
    code: &'a str,
    message: &'a str,
    hint: &'a str,
    run_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<&'a serde_json::Value>,
}
pub fn envelope(failure: &Failure) -> Envelope<'_> {
    Envelope {
        schema_version: SCHEMA_VERSION,
        error: EnvelopeError {
            code: &failure.code,
            message: &failure.message,
            hint: &failure.hint,
            run_id: failure.run_id.as_deref(),
            details: failure.details.as_deref(),
        },
    }
}

/// Where `run` reports its outcome besides its exit status.
#[derive(Default)]
pub struct Reports {
    pub status: Option<StatusFile>,
    pub run_id: Option<RunIdFile>,
}

fn run_failure(failed: &RunFailure) -> RunEnded {
    let RunFailure {
        failure,
        remote_failure_phase,
    } = failed;
    if failure.code == "connection_lost"
        && let Some(stream) = failure
            .details
            .as_ref()
            .and_then(|d| serde_json::from_value(d["peer_stream"].clone()).ok())
    {
        RunEnded::PeerFailed {
            failure: stream,
            remote_failure_phase: *remote_failure_phase,
        }
    } else {
        RunEnded::Failed {
            code: failure.code.clone(),
            remote_failure_phase: *remote_failure_phase,
        }
    }
}

/// Reports one of Elsecall's own failures and exits 125: the agent-ready line,
/// then the envelope as the last line of stderr.
pub fn fail(failure: &Failure, reports: &Reports) -> ExitCode {
    // The envelope below still reports the original failure.
    if let Some(status) = &reports.status {
        let _ = status.fail(failure);
    }
    if let Some(file) = &reports.run_id {
        let _ = file.record(failure.run_id.as_deref());
    }
    let line = serde_json::to_string(&envelope(failure)).expect("envelopes serialize");
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{}", guidance::failure_line(failure));
    let _ = writeln!(stderr, "{line}");
    let _ = stderr.flush();
    ExitCode::from(OWN_FAILURE)
}

/// The value of `option` among the options after the subcommand word `command`
/// (`run`, or `acquire` of `lease acquire`), read from the raw arguments when Clap
/// rejected them. Arguments after `--` are the command's own.
pub fn option_argument(arguments: &[OsString], command: &str, option: &str) -> Option<PathBuf> {
    let options = arguments
        .iter()
        .skip_while(|a| a.as_os_str() != OsStr::new(command))
        .skip(1)
        .take_while(|a| a.as_os_str() != OsStr::new("--"));
    let mut options = options.peekable();
    let prefix = format!("{option}=");
    while let Some(argument) = options.next() {
        if argument == option {
            return options.next().map(PathBuf::from);
        }
        if let Some(path) = argument.to_str().and_then(|a| a.strip_prefix(&prefix)) {
            return Some(path.into());
        }
    }
    None
}

/// Clap rejected the arguments of `run`. Values are not echoed: they may hold
/// secrets. A status file or run ID file named among them still records the
/// failure, so a stale one is never mistaken for this run's outcome.
pub fn usage_failure() -> ExitCode {
    let failure = crate::scheduling::request::invalid(
        "Invalid run options; the command goes after --, as in elsecall run --size 2 -- make test.",
    );
    let arguments: Vec<OsString> = std::env::args_os().collect();
    let reports = Reports {
        status: option_argument(&arguments, "run", "--status-file").map(StatusFile::at),
        run_id: option_argument(&arguments, "run", "--run-id-file").map(RunIdFile::at),
    };
    fail(&failure, &reports)
}

pub async fn main(args: RunArgs) -> ExitCode {
    let mut reports = Reports::default();
    match args.status_file.clone().map(StatusFile::create).transpose() {
        Ok(status) => reports.status = status,
        Err(failure) => return fail(&failure, &reports),
    }
    match args
        .caller
        .run_id_file
        .clone()
        .map(RunIdFile::create)
        .transpose()
    {
        Ok(file) => reports.run_id = file,
        Err(failure) => return fail(&failure, &reports),
    }
    match run(args, &reports).await {
        Ok(Exit::Code(code)) => ExitCode::from(code as u8),
        Ok(Exit::Signal(signal)) => signals::exit_by(signal),
        Err(failure) => fail(&failure, &reports),
    }
}

async fn run(args: RunArgs, reports: &Reports) -> Result<Exit> {
    let environment = CallerEnvironment::current()?;
    let now = Timestamp::now();
    let directory =
        std::env::current_dir().map_err(|e| Failure::io("Read the working directory", e))?;
    let configuration = ConsumerConfiguration::discover(&directory)?;
    let placement = match (args.pinned, args.placeable, args.member) {
        (true, _, _) => Some(Placement::Pinned),
        (_, _, Some(member)) => Some(Placement::Member(member)),
        (_, true, None) => Some(Placement::Placeable),
        (false, false, None) => None,
    };
    let declared = args.options.declared(now)?;
    let resolved = resolve(
        Layers {
            kind: Kind::Run,
            holder_pid: None,
            cli: &declared,
            placement,
            when_unavailable: args.caller.when_unavailable.map(Into::into),
            environment: &environment,
            configuration: &configuration,
        },
        now,
    )?;
    let mut request = resolved.request;
    let mut parts = args.command.into_iter();
    let command = CommandLine {
        program: parts.next().expect("Clap requires a command"),
        arguments: parts.collect(),
    };
    if environment.mode == Mode::Off {
        local::preflight(&command)?;
        return directly(&command, Admission::Off, reports).await;
    }
    if !cfg!(target_os = "macos") {
        return Err(Failure::new(
            "unsupported_platform",
            "This version supports macOS only.",
            "Run the command directly, or set ELSECALL=off.",
        ));
    }
    local_access::require_user()?;
    let paths = Paths::current()?;
    let socket = paths.socket();
    // Placeable work is placed with the working tree it was requested in.
    let located = match request.placement {
        Placement::Pinned => None,
        _ => workspace::snapshot::locate(&directory)?,
    };
    request.workspace = located.as_ref().map(|located| WorkspaceRef {
        root: located.root_text.clone(),
        common_dir: located.common_dir.clone(),
    });
    // A pinned run needs no copy, but its workspace still identifies learning
    // history in an exported trace. Inspection failure cannot block local work.
    if request.placement == Placement::Pinned {
        request.workspace = workspace::snapshot::locate(&directory)
            .ok()
            .flatten()
            .map(|workspace| WorkspaceRef {
                root: workspace.root_text,
                common_dir: workspace.common_dir,
            })
            .filter(|workspace| workspace.validate().is_ok());
    }
    let local = LocalExecutor {
        socket: socket.clone(),
    };
    // A pinned command runs here, so a typo fails before waiting for capacity.
    if request.placement == Placement::Pinned {
        local.preflight(&command)?;
    }
    // What a placed run needs is ready before admission, so nothing can fail
    // between the grant and the start without the run's ID.
    let caches = paths.data.join("workspaces");
    let copy_configuration = match &located {
        Some(located) => {
            crate::private_files::directory(&caches)?;
            Some(ConsumerConfiguration::load(&located.root)?)
        }
        None => None,
    };
    // Work required on a named member is bound to it, so it never runs here unmanaged.
    let may_run_unmanaged = !matches!(request.placement, Placement::Member(_));
    let requested_at = std::time::Instant::now();
    let grant: Grant = match admission_cli::admit(&socket, &request).await {
        Ok(grant) => grant,
        Err(failure)
            if may_run_unmanaged
                && admission_cli::proceeds_unmanaged(&failure, resolved.when_unavailable) =>
        {
            local.preflight(&command)?;
            guidance::write(&guidance::unmanaged_line(&failure, Unmanaged::Run));
            return directly(&command, Admission::Unmanaged, reports).await;
        }
        Err(failure) => return Err(failure),
    };
    if let Some(file) = &reports.run_id {
        // Telemetry only: the admitted command runs either way.
        let _ = file.record(Some(grant.run_id.as_str()));
    }
    let status = reports.status.as_ref();
    match (&grant.placed, located, copy_configuration) {
        (Some(placed), Some(located), Some(configuration)) => {
            let remote = RemoteExecutor {
                workload: request.workload.clone(),
                socket: socket.clone(),
                caches: CacheFiles::new(&caches, &located.root_text),
                configuration,
                located,
                start_by: placed.start_by,
                calling_member: placed.calling_member.clone(),
                synced_paths: placed.synced_paths.clone(),
            };
            execute(&remote, &command, &grant, &socket, status, requested_at).await
        }
        _ => {
            if request.placement != Placement::Pinned
                && let Err(failure) = local.preflight(&command)
            {
                report(
                    &socket,
                    &grant,
                    RunEnded::Failed {
                        code: failure.code.clone(),
                        remote_failure_phase: None,
                    },
                )
                .await;
                return Err(failure.for_run(&grant.run_id));
            }
            execute(&local, &command, &grant, &socket, status, requested_at).await
        }
    }
}

/// Runs the command as the caller would have, without admission, passing its
/// streams, signals and exit status through unchanged.
async fn directly(command: &CommandLine, admission: Admission, reports: &Reports) -> Result<Exit> {
    let running = local::start_directly(command, admission)?;
    let subject = Subject::unadmitted(admission);
    if let Some(status) = &reports.status {
        let _ = status.update(Status::running(subject, &[]));
    }
    let Finished { exit, .. } = running.wait().await.map_err(|failed| failed.failure)?;
    record_end(reports.status.as_ref(), subject, exit, &[], None);
    Ok(exit)
}

/// Writes how the command ended to the status file. The command ran, so its exit
/// stands even if the file cannot be written; exiting 125 here would invite a
/// caller to run it again. A warning line says why the file is stale instead.
fn record_end(
    status: Option<&StatusFile>,
    subject: Subject,
    exit: Exit,
    warnings: &[String],
    run_id: Option<&str>,
) {
    if let Some(status) = status
        && let Err(mut failure) = status.update(Status::ended(subject, exit, warnings))
    {
        failure.run_id = run_id.map(str::to_owned);
        let warning = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "warning": envelope(&failure).error,
        });
        let _ = writeln!(std::io::stderr().lock(), "{warning}");
    }
}

/// Records how the run ended. The outcome is the command's even if the record
/// cannot be updated; the service ends the grant when this process and the command
/// exit.
async fn report(socket: &std::path::Path, grant: &Grant, ended: RunEnded) {
    let route = format!("/v1/runs/{}/end", grant.run_id);
    let posted = client::post::<(), _>(socket, &route, &ended, Wait::Bounded).await;
    if posted.is_err_and(|failure| failure.code == "invalid_request")
        && let Some(legacy) = ended.without_phases()
    {
        // A service from before phase timing rejects the extra field.
        let _ = client::post::<(), _>(socket, &route, &legacy, Wait::Bounded).await;
    }
}

/// Runs the admitted command with `executor` and passes its outcome through.
async fn execute<E: Executor>(
    executor: &E,
    command: &CommandLine,
    grant: &Grant,
    socket: &std::path::Path,
    status: Option<&StatusFile>,
    requested_at: std::time::Instant,
) -> Result<Exit> {
    let admission = match grant.nested_in {
        Some(_) => Admission::Nested,
        None => Admission::Granted,
    };
    let context = RunContext {
        run_id: grant.run_id.clone(),
        member: grant.member.clone(),
        admission,
    };
    let subject = Subject {
        run_id: Some(&grant.run_id),
        member: Some(&grant.member),
        admission,
    };
    let running = match executor.start(command, &context).await {
        Ok(running) => running,
        Err(failed) => {
            report(socket, grant, run_failure(&failed)).await;
            return Err(failed.failure.for_run(&grant.run_id));
        }
    };
    if let Some(status) = status {
        // Progress only: the command already runs, so this must not fail the run.
        let _ = status.update(Status::running(subject, &running.warnings()));
    }
    let Finished {
        plan_observation,
        exit,
        warnings,
        cpu,
        execution,
        result_at,
        result_return,
    } = match running.wait().await {
        Ok(finished) => finished,
        Err(failed) => {
            report(socket, grant, run_failure(&failed)).await;
            return Err(failed.failure.for_run(&grant.run_id));
        }
    };
    let mut run_phases = RunPhases {
        execution_ms: Some(execution.as_millis() as u64),
        request_to_result_ms: result_at
            .map(|at| at.duration_since(requested_at).as_millis() as u64),
        ..RunPhases::default()
    };
    match result_return {
        Some(executor::ResultReturn::Returned(duration)) => {
            run_phases.result_return_ms = Some(duration.as_millis() as u64);
        }
        Some(executor::ResultReturn::Failed) => {
            run_phases.remote_failure_phase = Some(RemoteFailurePhase::ResultReturn);
        }
        None => (),
    }
    report(
        socket,
        grant,
        match exit {
            Exit::Code(exit_code) => RunEnded::Exited {
                exit_code,
                cpu_ms: cpu.map(|cpu| cpu.as_millis() as u64),
                run_phases: Some(run_phases),
                plan_observation: plan_observation.clone(),
            },
            Exit::Signal(signal) => RunEnded::Signaled {
                signal,
                cpu_ms: cpu.map(|cpu| cpu.as_millis() as u64),
                run_phases: Some(run_phases),
                plan_observation: plan_observation.clone(),
            },
        },
    )
    .await;
    record_end(
        status,
        subject,
        exit,
        &warnings,
        Some(grant.run_id.as_str()),
    );
    Ok(exit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(text: &[&str]) -> Vec<OsString> {
        text.iter().map(OsString::from).collect()
    }

    #[test]
    fn finds_report_files_among_run_options_only() {
        assert_eq!(
            option_argument(
                &arguments(&[
                    "elsecall",
                    "run",
                    "--sise",
                    "2",
                    "--status-file",
                    "s.json",
                    "--",
                    "make"
                ]),
                "run",
                "--status-file"
            ),
            Some("s.json".into())
        );
        assert_eq!(
            option_argument(
                &arguments(&["elsecall", "run", "--status-file=t.json"]),
                "run",
                "--status-file"
            ),
            Some("t.json".into())
        );
        assert_eq!(
            option_argument(
                &arguments(&["elsecall", "run", "--run-id-file=id", "--", "x"]),
                "run",
                "--run-id-file"
            ),
            Some("id".into())
        );
        assert_eq!(
            option_argument(
                &arguments(&["elsecall", "lease", "acquire", "--run-id-file", "id"]),
                "acquire",
                "--run-id-file"
            ),
            Some("id".into())
        );
        assert_eq!(
            option_argument(
                &arguments(&[
                    "elsecall",
                    "run",
                    "--",
                    "tool",
                    "--status-file",
                    "theirs.json"
                ]),
                "run",
                "--status-file"
            ),
            None,
            "the command's own arguments are not Elsecall's"
        );
    }
}
