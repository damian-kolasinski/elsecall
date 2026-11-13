//! CLI commands for leases and request records.
pub mod environment;
pub mod options;
pub mod resolution;

use crate::admission_api::MAXIMUM_LISTED;
use crate::{
    client::{self, Wait},
    consumer_configuration::{ConsumerConfiguration, WhenUnavailable},
    failure::{Failure, Result},
    guidance::{self, Unmanaged},
    local_access,
    paths::Paths,
    run_id_file::RunIdFile,
    scheduling::{
        capacity::Cores,
        estimates,
        messages::{Grant, Release, Runs},
        record::Record,
        request::{AdmissionRequest, Kind, Urgency, invalid},
        run_id::RunId,
        timestamp::Timestamp,
    },
};
use clap::Subcommand;
use environment::{CallerEnvironment, Mode};
use options::{CallerOptions, RequestOptions, UnavailableAction};
use resolution::{Layers, resolve};
use serde::Serialize;
use std::{path::Path, time::Duration};

#[derive(Subcommand, Debug)]
pub enum LeaseCommand {
    /// Wait for capacity, then hold it until the holder exits or releases it
    #[command(after_help = "Wrap a heavy section of a shell script:\n\n  \
        elsecall lease acquire --label \"test FeatureModule\" >/dev/null || exit 1\n  \
        ...heavy work...\n  \
        elsecall lease release >/dev/null\n\n\
        The lease also ends when the holder exits, even after SIGKILL. Work started \
        inside a lease reuses it instead of queueing, and a holder that acquires \
        again (a script that re-execs itself) gets the same lease back. \
        .elsecall.toml label rules supply what the flags leave out. ELSECALL=off \
        makes acquire and release succeed without doing anything.")]
    Acquire {
        #[command(flatten)]
        options: RequestOptions,
        #[command(flatten)]
        caller: CallerOptions,
        /// PID of the process that holds the lease: elsecall's parent (the calling
        /// script) by default. From a subshell or $(...), pass --holder $$.
        #[arg(long, value_name = "PID")]
        holder: Option<u32>,
    },
    /// Release one lease by ID, or every lease the holder holds
    Release {
        /// The lease to release, as printed by acquire
        #[arg(value_name = "RUN_ID", conflicts_with = "holder")]
        run_id: Option<String>,
        /// PID of the holder whose leases to release [default: elsecall's parent]
        #[arg(long, value_name = "PID")]
        holder: Option<u32>,
        /// When the local service cannot be reached: fail, or succeed releasing
        /// nothing [default: as for acquire]
        #[arg(long, value_name = "ACTION")]
        when_unavailable: Option<UnavailableAction>,
    },
}

/// How a lease request turned out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    /// The lease was granted after admission.
    Granted,
    /// Requested from inside other work, so it shares that work's grant.
    Nested,
    /// The holder already held a lease, which acquiring again returned.
    Reused,
    /// The service was unavailable and the caller chose to run anyway: nothing is held.
    Unmanaged,
    /// `ELSECALL=off`: Elsecall did nothing.
    Off,
}

/// What `lease acquire` prints. Without a lease, `run_id` and the rest are null.
#[derive(Debug, PartialEq, Serialize)]
pub struct Acquired {
    pub state: LeaseState,
    pub run_id: Option<RunId>,
    pub member: Option<String>,
    pub size: Option<Cores>,
    pub nested_in: Option<RunId>,
    pub waited_ms: u64,
    pub placement_note: Option<String>,
}
impl Acquired {
    fn without_lease(state: LeaseState) -> Self {
        Self {
            state,
            run_id: None,
            member: None,
            size: None,
            nested_in: None,
            waited_ms: 0,
            placement_note: None,
        }
    }
    fn granted(grant: Grant) -> Self {
        let state = match (grant.reused, &grant.nested_in) {
            (true, _) => LeaseState::Reused,
            (false, Some(_)) => LeaseState::Nested,
            (false, None) => LeaseState::Granted,
        };
        Self {
            state,
            run_id: Some(grant.run_id),
            member: Some(grant.member),
            size: Some(grant.size),
            nested_in: grant.nested_in,
            waited_ms: grant.waited_ms,
            placement_note: grant.placement_note,
        }
    }
}

/// How a release turned out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseState {
    /// The service released the leases listed.
    Released,
    Unmanaged,
    Off,
}

/// What `lease release` prints.
#[derive(Debug, PartialEq, Serialize)]
pub struct ReleaseOutcome {
    pub state: ReleaseState,
    pub released: Vec<RunId>,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum LeaseOutput {
    Acquired(Acquired),
    Released(ReleaseOutcome),
}

fn parent_pid() -> u32 {
    // SAFETY: getppid has no preconditions.
    unsafe { libc::getppid() as u32 }
}

fn run_id(text: &str) -> Result<RunId> {
    RunId::parse(text).ok_or_else(|| invalid("Run IDs look like r-3f9a0c41d2b7."))
}

/// How long past its hold limit an interactive request may stay undecided before
/// the caller stops waiting, as when the service stalls.
const DECISION_GRACE: Duration = Duration::from_secs(30);

/// Asks the local service to admit a request and waits for its decision.
/// Interactive requests wait at most their hold limit plus a grace period;
/// deferrable ones until the service decides.
pub async fn admit(socket: &Path, request: &AdmissionRequest) -> Result<Grant> {
    let decision = client::post(socket, "/v1/admissions", request, Wait::Unbounded);
    let limit = match request.urgency {
        Urgency::Interactive { hold_limit_ms } => hold_limit_ms
            .map(Duration::from_millis)
            .unwrap_or(estimates::TOOL_CALL_BUDGET),
        Urgency::Deferrable { .. } => return decision.await,
    };
    tokio::time::timeout(limit + DECISION_GRACE, decision)
        .await
        .unwrap_or_else(|_| {
            Err(Failure::new(
                "service_timeout",
                "The local service did not decide within the hold limit.",
                "Run elsecall doctor, then elsecall service restart.",
            ))
        })
}

/// Whether a failure to reach the service lets the work proceed unmanaged.
pub fn proceeds_unmanaged(failure: &Failure, when_unavailable: WhenUnavailable) -> bool {
    failure.code == "service_unavailable" && when_unavailable == WhenUnavailable::Run
}

fn working_configuration() -> Result<ConsumerConfiguration> {
    let directory =
        std::env::current_dir().map_err(|e| Failure::io("Read the working directory", e))?;
    ConsumerConfiguration::discover(&directory)
}

/// The local service's paths, for a caller allowed to use it.
fn service_paths() -> Result<Paths> {
    if !cfg!(target_os = "macos") {
        return Err(Failure::new(
            "unsupported_platform",
            "This version supports macOS only.",
            "Run the work directly, or set ELSECALL=off.",
        ));
    }
    local_access::require_user()?;
    Paths::current()
}

/// Runs a lease command; `ELSECALL=off` makes it succeed without doing anything.
pub async fn lease(command: LeaseCommand) -> Result<LeaseOutput> {
    match command {
        LeaseCommand::Acquire {
            options,
            caller,
            holder,
        } => {
            // Emptied before anything can fail, so a stale ID is never read.
            let run_id_file = caller
                .run_id_file
                .clone()
                .map(RunIdFile::create)
                .transpose()?;
            let acquired = match CallerEnvironment::current() {
                Ok(environment) => lease_acquire(&environment, &options, &caller, holder).await,
                Err(failure) => Err(failure),
            };
            if let Some(file) = &run_id_file {
                let id = match &acquired {
                    Ok(acquired) => acquired.run_id.as_ref().map(RunId::as_str),
                    Err(failure) => failure.run_id.as_deref(),
                };
                // The outcome stands even if its ID cannot be recorded.
                let _ = file.record(id);
            }
            acquired.map(LeaseOutput::Acquired)
        }
        LeaseCommand::Release {
            run_id,
            holder,
            when_unavailable,
        } => lease_release(
            &CallerEnvironment::current()?,
            run_id.as_deref(),
            holder,
            when_unavailable,
        )
        .await
        .map(LeaseOutput::Released),
    }
}

async fn lease_acquire(
    environment: &CallerEnvironment,
    options: &RequestOptions,
    caller: &CallerOptions,
    holder: Option<u32>,
) -> Result<Acquired> {
    let now = Timestamp::now();
    let configuration = working_configuration()?;
    let declared = options.declared(now)?;
    let resolved = resolve(
        Layers {
            kind: Kind::Lease,
            holder_pid: Some(holder.unwrap_or_else(parent_pid)),
            cli: &declared,
            placement: None,
            when_unavailable: caller.when_unavailable.map(Into::into),
            environment,
            configuration: &configuration,
        },
        now,
    )?;
    if environment.mode == Mode::Off {
        return Ok(Acquired::without_lease(LeaseState::Off));
    }
    let paths = service_paths()?;
    match admit(&paths.socket(), &resolved.request).await {
        Ok(grant) => Ok(Acquired::granted(grant)),
        Err(failure) if proceeds_unmanaged(&failure, resolved.when_unavailable) => {
            guidance::write(&guidance::unmanaged_line(&failure, Unmanaged::Lease));
            Ok(Acquired::without_lease(LeaseState::Unmanaged))
        }
        Err(failure) => Err(failure),
    }
}

async fn lease_release(
    environment: &CallerEnvironment,
    id: Option<&str>,
    holder: Option<u32>,
    when_unavailable: Option<UnavailableAction>,
) -> Result<ReleaseOutcome> {
    let release = match id {
        Some(id) => Release {
            run_id: Some(run_id(id)?),
            holder_pid: None,
        },
        None => Release {
            run_id: None,
            holder_pid: Some(holder.unwrap_or_else(parent_pid)),
        },
    };
    let configuration = working_configuration()?;
    let when_unavailable = resolution::when_unavailable(
        when_unavailable.map(Into::into),
        environment,
        &configuration,
    );
    let nothing = |state| ReleaseOutcome {
        state,
        released: Vec::new(),
    };
    if environment.mode == Mode::Off {
        return Ok(nothing(ReleaseState::Off));
    }
    let paths = service_paths()?;
    let released: Result<crate::scheduling::messages::Released> = client::post(
        &paths.socket(),
        "/v1/leases/release",
        &release,
        Wait::Bounded,
    )
    .await;
    match released {
        Ok(released) => Ok(ReleaseOutcome {
            state: ReleaseState::Released,
            released: released.released,
        }),
        // An unmanaged acquire held nothing; a lease that survives the service's
        // restart still ends when its holder exits.
        Err(failure) if proceeds_unmanaged(&failure, when_unavailable) => {
            Ok(nothing(ReleaseState::Unmanaged))
        }
        Err(failure) => Err(failure),
    }
}

pub async fn runs(paths: &Paths, limit: u32) -> Result<Runs> {
    if !(1..=MAXIMUM_LISTED).contains(&limit) {
        return Err(invalid(format!("--limit must be 1 to {MAXIMUM_LISTED}.")));
    }
    client::get_large(&paths.socket(), &format!("/v1/runs?limit={limit}")).await
}

pub async fn explain(paths: &Paths, id: &str) -> Result<Record> {
    let id = run_id(id)?;
    client::get_large(&paths.socket(), &format!("/v1/runs/{id}")).await
}
