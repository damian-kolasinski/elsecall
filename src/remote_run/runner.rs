//! The member's side of a placed run: sync the copy, start the command in it, pass
//! input, output and signals, and stop the command if the caller goes silent.
//! Processes the command leaves running when it exits are handed to
//! [`leftovers`](crate::remote_run::leftovers), which caps them.
use crate::{
    failure::{Failure, Result},
    macos::process_usage,
    remote_run::{
        consistency::{self, Fingerprints, Mismatches},
        environment::Placed,
        fingerprint,
        leftovers::{self, Cap, Watch},
        link::{self, Link, Outbox, SyncOverLink},
        output::{self, Window},
        protocol::{Exited, RunMessage, Start, Stream},
    },
    run_admission::{self, Admission},
    scheduling::{leftovers::Leftovers, run_id::RunId},
    wire::Frame,
    workspace::{
        confined::{Confined, Create},
        copies::Claim,
        digest::Digest,
        location::CopyLocation,
        not_returned::{Account, Cause},
        object_sources::ObjectSources,
        output_snapshot::{AtExit, Limits, Snapshot},
        output_survey, outputs,
        path::WorkspacePath,
        sync::{self, MemberStats},
    },
};
use std::{
    ffi::{OsStr, OsString},
    os::{
        fd::{FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, process::ExitStatusExt},
    },
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{io::AsyncWriteExt, net::unix::pipe, sync::mpsc};

/// Signals a caller may forward, as a local run forwards them.
pub const FORWARDED: [i32; 6] = [
    libc::SIGINT,
    libc::SIGTERM,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGUSR1,
    libc::SIGUSR2,
];
/// How long a stopped command has to exit before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(5);
/// How long a run waits for what the previous run of its copy left running to
/// end before it stops it and syncs the copy.
pub const LEFTOVERS_WAIT: Duration = Duration::from_secs(10);

/// Leave the executor's stopping budget inside the end-to-end hold limit.
pub fn leftovers_wait_budget(remaining: Duration) -> Duration {
    LEFTOVERS_WAIT.min(remaining.saturating_sub(leftovers::STOPPING))
}

/// How far a placed run got, for its record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reached {
    Syncing,
    Synced,
    Started,
    Exited(Exited),
}

/// How a placed run ended on this member.
#[derive(Clone, Debug)]
pub enum Ending {
    /// The process exited and the member entered the output exchange.
    Finished {
        exit: Exited,
        cpu_ms: Option<u64>,
        return_failed: bool,
    },
    Failed {
        failure: Failure,
        reached: Reached,
    },
}

/// What the runner tells its member about the run.
pub trait Host: Send + Sync {
    fn synced(&self, run: &RunId, stats: &MemberStats);
    /// Consistency warnings, for the run's record.
    fn warned(&self, run: &RunId, warnings: &[String]);
    /// The command's process, so work it requests nests inside the run.
    fn started(&self, run: &RunId, pid: u32);
    fn ended(&self, run: &RunId, ending: Ending);
    /// The command's validated assertion, received through its local grant.
    fn observation(&self, _run: &RunId) -> Option<crate::workload_metadata::Observation> {
        None
    }
    /// Processes the command left running, as they run and once they ended;
    /// reported also after the run ended.
    fn leftovers(&self, run: &RunId, leftovers: Leftovers);
    /// The run reached the wait for an earlier run's leftovers in its copy.
    fn waiting_for_leftovers(&self, run: &RunId);
    /// How long the run waited for an earlier run's leftovers in its copy.
    fn waited_for_leftovers(&self, run: &RunId, waited: Duration);
}

/// One admitted placed run, ready to be attached.
pub struct Assignment {
    pub run_id: RunId,
    pub member: String,
    pub calling_member: String,
    pub location: CopyLocation,
    /// Exclusive use of the copy for the whole run.
    pub claim: Claim,
    /// The member's PATH for placed work, if configured.
    pub path: Option<OsString>,
    /// Where the member may find callers' history before asking for it.
    pub sources: ObjectSources,
    /// The latest the command may start: the hold limit, end to end.
    pub start_by: Instant,
    /// How long processes the command leaves running may run after it exits.
    pub leftovers_cap: Cap,
}

pub async fn run<S>(stream: S, assignment: Assignment, host: Arc<dyn Host>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    let mut link = Link::open_monitored(stream);
    let id = assignment.run_id.clone();
    let mut left = None;
    let ending = match execute(&mut link, &assignment, &host).await {
        Ok(Finished {
            exit,
            cpu_ms,
            return_failed,
            leftovers,
        }) => {
            left = leftovers;
            Ending::Finished {
                exit,
                cpu_ms,
                return_failed,
            }
        }
        Err((failure, reached)) => {
            if failure.code != "connection_lost" {
                let _ = link
                    .message(&RunMessage::Failed {
                        failure: failure.clone(),
                    })
                    .await;
            }
            Ending::Failed { failure, reached }
        }
    };
    // The run's capacity is released here; what it left running is not admitted
    // work and counts as outside load.
    host.ended(&id, ending);
    link.close().await;
    match left {
        Some(leftovers) => assignment.claim.release_with(Arc::new(leftovers)),
        None => drop(assignment.claim),
    }
}

type Outcome<T> = std::result::Result<T, (Failure, Reached)>;

/// A run whose caller received the exit.
struct Finished {
    exit: Exited,
    cpu_ms: Option<u64>,
    return_failed: bool,
    /// What the command left running, while it runs.
    leftovers: Option<Watch>,
}

async fn execute(
    link: &mut Link,
    assignment: &Assignment,
    host: &Arc<dyn Host>,
) -> Outcome<Finished> {
    let at = |reached| move |failure| (failure, reached);
    let too_late = || {
        Failure::new(
            "fleet_busy",
            "The command could not start within its hold limit; the workspace sync took too long.",
            "Retry: the copy is warm now, so the next sync is faster. Or pass a longer --hold-limit.",
        )
    };
    // The hold limit bounds everything up to the start, so a caller cannot hold
    // this member's capacity by syncing or waiting forever.
    let deadline = tokio::time::Instant::from_std(assignment.start_by);
    // What the previous run left running may still write in the copy: it ends,
    // or is stopped, before the sync, so the copy matches the caller at the start.
    if let Some(lingering) = assignment.claim.lingering() {
        let waiting = Instant::now();
        // Stopping them takes time too, within the hold limit.
        let wait = leftovers_wait_budget(assignment.start_by.saturating_duration_since(waiting));
        host.waiting_for_leftovers(&assignment.run_id);
        lingering.settle(wait, &assignment.run_id.to_string()).await;
        host.waited_for_leftovers(&assignment.run_id, waiting.elapsed());
    }
    let (copy, stats) = tokio::time::timeout_at(
        deadline,
        sync::receive(
            &mut SyncOverLink(link),
            &assignment.location,
            &assignment.sources,
        ),
    )
    .await
    .unwrap_or_else(|_| Err(too_late()))
    .map_err(at(Reached::Syncing))?;
    host.synced(&assignment.run_id, &stats);
    let start = match tokio::time::timeout_at(deadline, link.receive())
        .await
        .unwrap_or_else(|_| Err(too_late()))
        .map_err(at(Reached::Synced))?
    {
        Frame::Message(bytes) => match link::decode(&bytes).map_err(at(Reached::Synced))? {
            RunMessage::Start(start) => start,
            _ => return Err((protocol("Start"), Reached::Synced)),
        },
        _ => return Err((protocol("Start"), Reached::Synced)),
    };
    let root = copy.root().to_owned();
    let prepared = start
        .validate()
        .map_err(not_startable)
        .and_then(|_| Prepared::new(&start, &root, assignment));
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(failure) => {
            let _ = link
                .message(&RunMessage::StartFailed {
                    failure: failure.clone(),
                })
                .await;
            return Err((failure, Reached::Synced));
        }
    };
    let fingerprints = match &start.consistency {
        Some(request) => fingerprint(request, &prepared, &root).await,
        None => None,
    };
    let confined = Arc::new(
        Confined::open(&root).map_err(|e| (not_startable(&e.to_string()), Reached::Synced))?,
    );
    let before = tokio::task::block_in_place(|| output_survey::survey(&confined, &start.outputs));
    // Ready before the command starts, so the snapshot can be taken the moment it exits.
    let staging = match start.outputs.is_empty() {
        true => Ok(None),
        false => tokio::task::block_in_place(|| {
            copy.output_staging(&assignment.run_id.to_string())
                .map(Some)
                .map_err(|failure| failure.message)
        }),
    };
    let snapshot_at_exit = {
        let confined = confined.clone();
        move || {
            tokio::task::spawn_blocking(move || match staging {
                Ok(Some(staging)) => AtExit::Taken(Snapshot::take(
                    &confined,
                    &before,
                    staging,
                    Limits::RETURNED,
                )),
                Ok(None) => AtExit::NoOutputs,
                Err(reason) => AtExit::Unavailable(reason),
            })
        }
    };
    let mut command = match Command::spawn(&start, &prepared) {
        Ok(command) => command,
        Err(failure) => {
            let _ = link
                .message(&RunMessage::StartFailed {
                    failure: failure.clone(),
                })
                .await;
            return Err((failure, Reached::Synced));
        }
    };
    host.started(&assignment.run_id, command.pid);
    if let Err(failure) = link.message(&RunMessage::Started { fingerprints }).await {
        command.stop().await;
        return Err((failure, Reached::Started));
    }
    // The caller reports what differed; only names it declared are recorded, in
    // this member's own words.
    let declared = start
        .consistency
        .as_ref()
        .map(|request| request.declared.clone());
    let record = |mismatches: &Mismatches| {
        if let Some(declared) = &declared
            && mismatches.within(declared)
        {
            let warnings = mismatches.warnings(&assignment.calling_member, &assignment.member);
            host.warned(&assignment.run_id, &warnings);
        }
    };
    // A run that fails from here on drops its snapshot, which removes the staging.
    let (exited, snapshot) = match command.pass(link, &record, snapshot_at_exit).await {
        Ok(passed) => passed,
        Err((failure, reached)) => {
            command.stop().await;
            return Err((failure, reached));
        }
    };
    // The exit goes first, so the caller can act on it, and on an interrupt, while
    // the outputs' snapshot is described.
    let cpu_ms = command.cpu.map(|cpu| cpu.as_millis() as u64);
    if let Err(failure) = link
        .message(&RunMessage::Exited {
            exit: exited,
            cpu_ms,
            plan_observation: host
                .observation(&assignment.run_id)
                .map(|o| serde_json::value::to_raw_value(&o).expect("observations serialize")),
        })
        .await
    {
        command.stop().await;
        return Err((failure, Reached::Exited(exited)));
    }
    // The exit has been queued. Processes the command left in its group run on,
    // as after a local run, up to the cap, whatever happens to the connection.
    let leftovers = {
        let host = host.clone();
        let id = assignment.run_id.clone();
        command.leave(
            assignment.leftovers_cap,
            Box::new(move |leftovers| host.leftovers(&id, leftovers)),
        )
    };
    let at_exit = snapshot
        .await
        .unwrap_or_else(|_| AtExit::Unavailable("the snapshot at exit failed".into()));
    // The command exit stands in the host record through the output exchange.
    let mut account = match &at_exit {
        AtExit::NoOutputs => Account::default(),
        AtExit::Unavailable(reason) => Account::unavailable(reason.clone()),
        AtExit::Taken(snapshot) => {
            let mut own = snapshot.shortfall().clone();
            own.add_skipped(snapshot.changes());
            Account::new(own)
        }
    };
    let mut exit_confirmed = false;
    let returned = return_outputs(
        link,
        &at_exit,
        &start.outputs,
        &mut account,
        &record,
        &mut exit_confirmed,
    )
    .await;
    let return_reported = account.return_reported();
    let mut warnings = account.warnings(&start.outputs);
    // Only the caller's output messages confirm it received Exited.
    // After a valid Returned, losing Done does not cost the outputs.
    let return_failed =
        exit_confirmed && (!warnings.is_empty() || (returned.is_err() && !return_reported));
    if let Err(failure) = returned {
        let reason = match failure.code.as_str() {
            "connection_lost" => "the caller's connection ended".to_owned(),
            _ => failure.message,
        };
        warnings.push(format!("Outputs were not returned: {reason}."));
    }
    if !warnings.is_empty() {
        host.warned(&assignment.run_id, &warnings);
    }
    // Removing the staging area is blocking work.
    tokio::task::block_in_place(|| drop(at_exit));
    Ok(Finished {
        exit: exited,
        cpu_ms,
        return_failed,
        leftovers,
    })
}

/// Describes the declared outputs the run changed, from the snapshot at exit, and
/// sends what the caller asks for, until the caller ends the run.
async fn return_outputs(
    link: &mut Link,
    at_exit: &AtExit,
    declared: &[WorkspacePath],
    account: &mut Account,
    record: &(dyn Fn(&Mismatches) + Sync),
    exit_confirmed: &mut bool,
) -> Result<()> {
    // Declared outputs come back whatever the exit code, if the run changed them.
    let (described, changes, not_returned) = match at_exit {
        AtExit::NoOutputs => (true, &[][..], Vec::new()),
        AtExit::Unavailable(_) => (false, &[][..], Vec::new()),
        AtExit::Taken(snapshot) => (true, snapshot.changes(), snapshot.shortfall().entries()),
    };
    link.message(&RunMessage::Outputs {
        described,
        not_returned,
    })
    .await?;
    send_blob(
        link,
        &serde_json::to_vec(changes).expect("changes serialize"),
    )
    .await?;
    // The caller confirms it has everything before the run ends.
    loop {
        match link.receive().await? {
            Frame::Message(bytes) => match link::decode(&bytes)? {
                RunMessage::Done => {
                    *exit_confirmed = true;
                    return Ok(());
                }
                RunMessage::Mismatched { mismatches } => record(&mismatches),
                RunMessage::WantOutputs => {
                    *exit_confirmed = true;
                    let wanted = receive_blob(link).await?;
                    let digests: Vec<Digest> = serde_json::from_slice(&wanted)
                        .map_err(|_| protocol("a list of wanted outputs"))?;
                    match at_exit {
                        AtExit::Taken(snapshot) => {
                            send_outputs(link, snapshot, &digests, account).await?
                        }
                        _ if digests.is_empty() => (),
                        _ => return Err(protocol("a list of wanted outputs")),
                    }
                }
                RunMessage::Returned {
                    not_returned,
                    nothing,
                } => {
                    *exit_confirmed = true;
                    account.reported(declared, not_returned, nothing);
                }
                // Late acknowledgements and signals after the exit.
                _ => continue,
            },
            _ => continue,
        }
    }
}

async fn send_blob(link: &Link, bytes: &[u8]) -> Result<()> {
    for chunk in bytes.chunks(crate::wire::CHUNK) {
        link.send(Frame::Content(chunk.to_vec())).await?;
    }
    link.send(Frame::Content(Vec::new())).await
}

async fn receive_blob(link: &mut Link) -> Result<Vec<u8>> {
    let mut blob = Vec::new();
    loop {
        match link.receive().await? {
            Frame::Content(chunk) if chunk.is_empty() => return Ok(blob),
            Frame::Content(chunk) if blob.len() + chunk.len() <= outputs::MAXIMUM_LISTING => {
                blob.extend_from_slice(&chunk)
            }
            _ => return Err(protocol("a list of wanted outputs")),
        }
    }
}

/// Sends the content of the changed outputs the caller asked for, one blob each,
/// from the snapshot. A file that cannot be sent is noted and replaced by
/// `OutputNotSent`; the others still go. Only digests of the outputs described can
/// be asked for.
async fn send_outputs(
    link: &Link,
    snapshot: &Snapshot,
    digests: &[Digest],
    account: &mut Account,
) -> Result<()> {
    let files: std::collections::HashMap<_, _> = snapshot
        .changes()
        .iter()
        .flat_map(|change| {
            change
                .content
                .iter()
                .flat_map(|content| &content.files)
                .map(move |file| (file.digest, (&change.output, file)))
        })
        .collect();
    for digest in digests {
        let (output, file) = files
            .get(digest)
            .ok_or_else(|| protocol("a digest of a changed output"))?;
        if let Err((cause, detail)) = send_file(link, snapshot, file).await? {
            link.message(&RunMessage::OutputNotSent {
                cause,
                detail: detail.clone(),
            })
            .await?;
            account.add(output, &file.path, cause, detail);
        }
    }
    Ok(())
}

/// Sends one file as a blob. The inner error is why the rest of it could not be
/// sent; the outer one ends the transfer, as a lost connection does.
async fn send_file(
    link: &Link,
    snapshot: &Snapshot,
    file: &outputs::OutputFile,
) -> Result<std::result::Result<(), (Cause, Option<String>)>> {
    let unreadable = |e: std::io::Error| (Cause::Unreadable, Some(e.to_string()));
    let mut reader = match tokio::task::block_in_place(|| snapshot.open(file)) {
        Ok(reader) => reader,
        Err(e) => return Ok(Err(unreadable(e))),
    };
    let mut sent = 0u64;
    loop {
        let chunk = tokio::task::block_in_place(|| {
            let mut buffer = vec![0u8; crate::wire::CHUNK];
            let n = std::io::Read::read(&mut reader, &mut buffer)?;
            buffer.truncate(n);
            Ok::<_, std::io::Error>(buffer)
        });
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(e) => return Ok(Err(unreadable(e))),
        };
        sent += chunk.len() as u64;
        // The snapshot is the run's own, but a file that differs from its
        // description would not match its digest.
        if sent > file.size {
            return Ok(Err((
                Cause::TransferError,
                Some("it changed in the snapshot after it was described".into()),
            )));
        }
        let done = chunk.is_empty();
        link.send(Frame::Content(chunk)).await?;
        if done {
            return Ok(Ok(()));
        }
    }
}

/// The member's fingerprints of what the caller declared, under the caller's salt.
/// `None` for a declaration that is not valid.
async fn fingerprint(
    request: &consistency::Request,
    prepared: &Prepared,
    root: &std::path::Path,
) -> Option<Fingerprints> {
    let locations = request.declared.validate()?;
    let request = request.clone();
    let environment = prepared.environment.clone();
    let root = root.to_owned();
    let home = environment
        .iter()
        .find(|(name, _)| name == "HOME")
        .map(|(_, value)| std::path::PathBuf::from(value))
        .unwrap_or_default();
    tokio::task::spawn_blocking(move || {
        fingerprint::take(
            &request.declared,
            &locations,
            &request.salt,
            &home,
            Some(&environment),
            &root,
        )
    })
    .await
    .ok()
}

fn protocol(expected: &str) -> Failure {
    Failure::new(
        "protocol_error",
        format!("The caller did not send {expected}."),
        "Install the same Elsecall version on every member.",
    )
}

fn not_startable(detail: &str) -> Failure {
    // Never echo the command: its arguments may hold secrets.
    Failure::new(
        "invalid_request",
        format!("The command could not be started on the member: {detail}."),
        "Check the command name and the member's PATH for placed work (elsecall doctor on the member).",
    )
}

/// A unidirectional pipe whose ends do not leak into other children.
fn pipe_pair() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors pipe writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    for fd in fds {
        // SAFETY: both are new, valid descriptors.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    // SAFETY: pipe returned two new descriptors nothing else owns.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

struct Command {
    child: tokio::process::Child,
    pid: u32,
    outputs: Vec<(Stream, pipe::Receiver)>,
    stdin: Option<pipe::Sender>,
    /// CPU time the command used, read once it exited.
    cpu: Option<Duration>,
    /// What reads the command's output, once it is being passed.
    readers: Vec<tokio::task::JoinHandle<()>>,
}

fn exited(status: std::process::ExitStatus) -> Exited {
    match (status.code(), status.signal()) {
        (Some(code), _) => Exited::Code { code },
        (None, Some(signal)) => Exited::Signal { signal },
        (None, None) => unreachable!("a waited process exited or was signaled"),
    }
}

fn signal_group(pid: u32, signal: i32) {
    // SAFETY: kill has no memory-safety preconditions. The command leads its own
    // process group, so this reaches everything it started there.
    unsafe { libc::kill(-(pid as i32), signal) };
}

/// Where and with what environment the command runs.
struct Prepared {
    directory: PathBuf,
    environment: Vec<(OsString, OsString)>,
}

impl Prepared {
    fn new(start: &Start, root: &std::path::Path, assignment: &Assignment) -> Result<Self> {
        let root = root.to_owned();
        let directory = match &start.directory {
            None => root.clone(),
            Some(prefix) => {
                // The caller may work in an ignored or empty directory the copy lacks.
                let confined = Confined::open(&root).map_err(|e| not_startable(&e.to_string()))?;
                confined
                    .directory(Some(prefix), Create::Directories)
                    .map_err(|e| not_startable(&e.to_string()))?
                    .ok_or_else(|| {
                        not_startable("the working directory is not a directory in the copy")
                    })?;
                root.join(prefix.as_str())
            }
        };
        let environment = Placed {
            target: std::env::vars_os().collect(),
            path: assignment.path.as_deref(),
            forwarded: &start.environment,
            elsecall: vec![
                ("ELSECALL_RUN_ID", assignment.run_id.to_string().into()),
                ("ELSECALL_MEMBER", assignment.member.clone().into()),
                // Placed work always has a grant of its own: nested work is never placed.
                (run_admission::VARIABLE, Admission::Granted.as_str().into()),
                (
                    "ELSECALL_CALLER_MEMBER",
                    assignment.calling_member.clone().into(),
                ),
                (
                    "ELSECALL_CALLER_WORKSPACE",
                    assignment.location.caller_root.clone().into(),
                ),
                ("ELSECALL_WORKSPACE_COPY", root.clone().into_os_string()),
            ],
            working_directory: directory.as_os_str(),
        }
        .build();
        Ok(Self {
            directory,
            environment,
        })
    }
}

impl Command {
    fn spawn(start: &Start, prepared: &Prepared) -> Result<Self> {
        let failed = |e: std::io::Error| not_startable(&e.to_string());
        // From the command's pipes until it exists, no other child may start.
        let starting = crate::child_start::starting();
        let (stdout_read, stdout_write) = pipe_pair().map_err(failed)?;
        let (stderr_read, stderr_write) = if start.merged {
            (None, stdout_write.try_clone().map_err(failed)?)
        } else {
            let (read, write) = pipe_pair().map_err(failed)?;
            (Some(read), write)
        };
        let (stdin_read, stdin_write) = if start.input {
            let (read, write) = pipe_pair().map_err(failed)?;
            (Stdio::from(read), Some(write))
        } else {
            (Stdio::null(), None)
        };
        let mut command = tokio::process::Command::new(OsStr::from_bytes(&start.program));
        command
            .args(start.arguments.iter().map(|a| OsStr::from_bytes(a)))
            .env_clear()
            .envs(prepared.environment.iter().map(|(k, v)| (k, v)))
            .current_dir(&prepared.directory)
            .stdin(stdin_read)
            .stdout(Stdio::from(stdout_write))
            .stderr(Stdio::from(stderr_write))
            .process_group(0);
        let child = command.spawn().map_err(failed)?;
        // Close this process's copies of the command's ends, so its exit ends them.
        drop(command);
        drop(starting);
        let pid = child.id().expect("a spawned child has a pid");
        let streams = (|| -> std::io::Result<_> {
            let mut outputs = vec![(Stream::Stdout, pipe::Receiver::from_owned_fd(stdout_read)?)];
            if let Some(read) = stderr_read {
                outputs.push((Stream::Stderr, pipe::Receiver::from_owned_fd(read)?));
            }
            let stdin = stdin_write.map(pipe::Sender::from_owned_fd).transpose()?;
            Ok((outputs, stdin))
        })();
        let (outputs, stdin) = match streams {
            Ok(streams) => streams,
            Err(e) => {
                signal_group(pid, libc::SIGKILL);
                return Err(failed(e));
            }
        };
        Ok(Self {
            child,
            pid,
            outputs,
            stdin,
            cpu: None,
            readers: Vec::new(),
        })
    }

    /// Passes input, output and signals until the command has exited and its output
    /// is drained. `at_exit` runs the moment the exit is noticed, before draining,
    /// and its result comes back with the exit.
    async fn pass<T>(
        &mut self,
        link: &mut Link,
        mismatched: &(dyn Fn(&Mismatches) + Sync),
        at_exit: impl FnOnce() -> T,
    ) -> Outcome<(Exited, T)> {
        let mut at_exit = Some(at_exit);
        let mut taken = None;
        let outbox = link.outbox();
        let mut readers = Vec::new();
        for (stream, receiver) in std::mem::take(&mut self.outputs) {
            let window = Arc::new(Window::default());
            let task = tokio::spawn(output::read(
                stream,
                receiver,
                window.clone(),
                outbox.clone(),
            ));
            readers.push((stream, task.abort_handle(), window));
            self.readers.push(task);
        }
        let mut stdin = self.stdin.take().map(|pipe| {
            let (sender, receiver) = mpsc::unbounded_channel();
            tokio::spawn(write_input(pipe, receiver, outbox.clone()));
            sender
        });
        let mut exit: Option<Exited> = None;
        // The command's exit is noticed without reaping it, so its CPU time can
        // still be read: on each SIGCHLD and on every tick, without parking a
        // thread that would keep the service from stopping. If that fails, the
        // exit is waited for as usual.
        let pid = self.pid;
        let mut children =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child()).ok();
        let mut plain_wait = children.is_none();
        let mut ticker = tokio::time::interval(Duration::from_millis(200));
        let reached = |exit: Option<Exited>| exit.map_or(Reached::Started, Reached::Exited);
        loop {
            if let Some(exit) = exit {
                if let Some(at_exit) = at_exit.take() {
                    taken = Some(at_exit());
                    for (_, _, window) in &readers {
                        window.exited();
                    }
                }
                if readers
                    .iter()
                    .all(|(_, task, window)| task.is_finished() || window.drained())
                {
                    return Ok((exit, taken.take().expect("taken at the exit")));
                }
            }
            tokio::select! {
                frame = link.receive() => match frame {
                    Err(failure) => return Err((failure, reached(exit))),
                    Ok(Frame::Stdin(bytes)) => {
                        if let Some(stdin) = &stdin {
                            let _ = stdin.send(bytes);
                        }
                    }
                    Ok(Frame::Message(bytes)) => match link::decode(&bytes).map_err(|f| (f, reached(exit)))? {
                        RunMessage::InputEnded => stdin = None,
                        RunMessage::Mismatched { mismatches } => mismatched(&mismatches),
                        RunMessage::Signal { signal } if FORWARDED.contains(&signal) && exit.is_none() => {
                            signal_group(self.pid, signal);
                        }
                        RunMessage::Ack { stream, bytes } => {
                            if let Some((_, _, window)) = readers.iter().find(|(s, _, _)| *s == stream) {
                                window.acknowledge(bytes);
                            }
                        }
                        RunMessage::OutputClosed { stream } => {
                            // Closing the pipe gives the command SIGPIPE, as locally.
                            if let Some((_, task, _)) = readers.iter().find(|(s, _, _)| *s == stream) {
                                task.abort();
                            }
                        }
                        RunMessage::Done => return Err((link::lost("the caller ended the run early"), reached(exit))),
                        _ => (),
                    },
                    Ok(_) => (),
                },
                _ = async { children.as_mut().expect("guarded").recv().await }, if exit.is_none() && !plain_wait => {
                    match process_usage::has_exited(pid) {
                        Ok(true) => exit = Some(self.reap().await?),
                        Ok(false) => (),
                        Err(_) => plain_wait = true,
                    }
                }
                status = self.child.wait(), if exit.is_none() && plain_wait => {
                    let status = status.map_err(|e| (not_startable(&e.to_string()), Reached::Started))?;
                    exit = Some(exited(status));
                }
                // A SIGCHLD can arrive before the stream was registered.
                _ = ticker.tick() => {
                    if exit.is_none() && !plain_wait && matches!(process_usage::has_exited(pid), Ok(true)) {
                        exit = Some(self.reap().await?);
                    }
                }
            }
        }
    }

    /// Reads the exited command's CPU time, then reaps it.
    async fn reap(&mut self) -> Outcome<Exited> {
        self.cpu = process_usage::cpu_time(self.pid);
        let status = self
            .child
            .wait()
            .await
            .map_err(|e| (not_startable(&e.to_string()), Reached::Started))?;
        Ok(exited(status))
    }

    /// Stops the command and everything in its process group: a termination signal,
    /// then a kill if it has not exited within the grace period.
    async fn stop(&mut self) {
        signal_group(self.pid, libc::SIGTERM);
        if tokio::time::timeout(STOP_GRACE, self.child.wait())
            .await
            .is_err()
        {
            signal_group(self.pid, libc::SIGKILL);
            let _ = self.child.wait().await;
        }
        // Whatever else the group still holds, even after its leader exited.
        signal_group(self.pid, libc::SIGKILL);
        for reader in self.readers.drain(..) {
            reader.abort();
        }
    }

    /// Leaves what the exited command still runs in its group running, up to
    /// `cap`, with its output read and dropped. `None` when it left nothing.
    fn leave(&mut self, cap: Cap, report: leftovers::Report) -> Option<Watch> {
        leftovers::watch(self.pid, cap, report, std::mem::take(&mut self.readers))
    }
}

async fn write_input(
    mut pipe: pipe::Sender,
    mut chunks: mpsc::UnboundedReceiver<Vec<u8>>,
    outbox: Outbox,
) {
    while let Some(chunk) = chunks.recv().await {
        if pipe.write_all(&chunk).await.is_err() {
            let _ = outbox.message(&RunMessage::InputClosed).await;
            return;
        }
        let _ = outbox
            .message(&RunMessage::Ack {
                stream: Stream::Stdin,
                bytes: chunk.len() as u64,
            })
            .await;
    }
}

/// The caller was admitted but its connection never arrived.
pub fn never_attached(assignment: Assignment, host: Arc<dyn Host>) {
    host.ended(
        &assignment.run_id,
        Ending::Failed {
            failure: link::lost("the caller's connection did not arrive"),
            reached: Reached::Syncing,
        },
    );
}
