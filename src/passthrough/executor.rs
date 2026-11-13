//! Where an admitted command executes. The caller-facing behaviour of `elsecall
//! run` (streams, exit status, the 125 envelope and the status file) lives in
//! `passthrough` and never depends on the executor. S1 runs every command on this
//! machine. A grant that names another member gets an executor for that member
//! (S3), which starts asynchronously, copies the workspace within the hold limit,
//! and forwards streams and signals over its connection.
use crate::{
    failure::{Failure, Result},
    run_admission::Admission,
    scheduling::{run_id::RunId, run_phases::RemoteFailurePhase},
};
use std::{
    ffi::OsString,
    time::{Duration, Instant},
};

/// The caller's command, passed through unchanged.
#[derive(Clone, Debug)]
pub struct CommandLine {
    pub program: OsString,
    pub arguments: Vec<OsString>,
}

/// What Elsecall tells the command about its run, as `ELSECALL_*` variables.
#[derive(Clone, Debug)]
pub struct RunContext {
    pub run_id: RunId,
    pub member: String,
    pub admission: Admission,
}
impl RunContext {
    pub fn variables(&self) -> [(&'static str, String); 3] {
        [
            ("ELSECALL_RUN_ID", self.run_id.to_string()),
            ("ELSECALL_MEMBER", self.member.clone()),
            self.admission.variable(),
        ]
    }
}

/// How the command ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
}

pub trait Executor {
    type Running: Running;
    /// Fails before admission when the command cannot start at all, so a caller
    /// never waits for capacity only to learn of a typo.
    fn preflight(&self, command: &CommandLine) -> Result<()>;
    /// Starts the admitted command with the caller's standard streams and
    /// environment plus the run's `ELSECALL_*` variables, forwards the caller's
    /// interrupt and termination signals to it, and binds whatever runs it to the
    /// grant so the grant lasts as long as the command.
    fn start(
        &self,
        command: &CommandLine,
        context: &RunContext,
    ) -> impl Future<Output = RunResult<Self::Running>> + Send;
}

/// How a command ended, with what Elsecall noticed on the way, such as
/// consistency warnings or outputs that could not come back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    pub plan_observation: Option<crate::workload_metadata::Observation>,
    pub exit: Exit,
    pub warnings: Vec<String>,
    /// CPU time the command and the descendants it waited for used, when measured.
    pub cpu: Option<Duration>,
    pub execution: Duration,
    /// The last usable result boundary, on the caller's monotonic clock.
    pub result_at: Option<Instant>,
    pub result_return: Option<ResultReturn>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultReturn {
    Returned(Duration),
    Failed,
}

pub type RunResult<T> = std::result::Result<T, RunFailure>;

pub struct RunFailure {
    pub failure: Failure,
    pub remote_failure_phase: Option<RemoteFailurePhase>,
}

impl From<Failure> for RunFailure {
    fn from(failure: Failure) -> Self {
        Self {
            failure,
            remote_failure_phase: None,
        }
    }
}

impl RunFailure {
    pub fn remote(failure: Failure, phase: RemoteFailurePhase) -> Self {
        Self {
            failure,
            remote_failure_phase: Some(phase),
        }
    }
}

pub trait Running {
    /// Warnings known once the command started.
    fn warnings(&self) -> Vec<String>;
    fn wait(self) -> impl Future<Output = RunResult<Finished>> + Send;
}
