//! Executes the command on the member a placed grant names: syncs the workspace,
//! starts the command there, and passes the caller's input, output, signals and the
//! command's exit through, as if it ran here.
use crate::{
    client::{self, Wait},
    consumer_configuration::ConsumerConfiguration,
    failure::{Failure, Result},
    passthrough::executor::{
        CommandLine, Executor, Exit, Finished, ResultReturn, RunContext, RunFailure, RunResult,
        Running,
    },
    remote_run::{
        consistency, environment, fingerprint,
        link::{self, Link, Outbox, SyncOverLink},
        protocol::{Exited, RunMessage, Start, Stream, WINDOW},
        runner::FORWARDED,
    },
    scheduling::{
        placed::{PlacedReport, Reached, SyncSummary},
        run_id::RunId,
        run_phases::RemoteFailurePhase,
        timestamp::Timestamp,
    },
    wire::Frame,
    workspace::{
        confined::Confined,
        not_returned::{self, Cause},
        output_return::{Receiving, Returning},
        outputs::{self, Changed},
        path::WorkspacePath,
        push_cache::CacheFiles,
        snapshot::Located,
        sync,
    },
};
use std::{
    fs::File,
    io::{Read, Write},
    mem::ManuallyDrop,
    os::{fd::FromRawFd, unix::ffi::OsStrExt},
    path::PathBuf,
    time::Instant,
};
use tokio::sync::mpsc;

/// Runs commands on another member, through the local service's relay.
pub struct RemoteExecutor {
    pub workload: Option<crate::workload_metadata::Workload>,
    pub socket: PathBuf,
    pub located: Located,
    pub configuration: ConsumerConfiguration,
    pub caches: CacheFiles,
    /// The hold limit, end to end: the command must start by then.
    pub start_by: Timestamp,
    /// This member's name, for warnings.
    pub calling_member: String,
    /// Synced paths the member was brought up to date with before the run.
    pub synced_paths: Vec<String>,
}

fn hold_limit_passed() -> Failure {
    Failure::new(
        "fleet_busy",
        "The command could not start on the member within its hold limit, which includes the workspace sync.",
        "Retry: the copy stays warm, so the next sync moves only what changed. Or pass a longer --hold-limit.",
    )
}

/// Whether stdout and stderr are one destination, as with `2>&1`.
fn merged_output() -> bool {
    let identity = |fd| {
        // SAFETY: an all-zero stat is a valid output buffer for fstat.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fstat only writes the buffer.
        (unsafe { libc::fstat(fd, &mut stat) } == 0).then_some((stat.st_dev, stat.st_ino))
    };
    matches!((identity(1), identity(2)), (Some(a), Some(b)) if a == b)
}

fn descriptor_open(fd: i32) -> bool {
    // SAFETY: F_GETFD only reads the descriptor's flags.
    (unsafe { libc::fcntl(fd, libc::F_GETFD) }) >= 0
}

/// One of the caller's standard descriptors, borrowed without closing it.
fn standard(fd: i32) -> ManuallyDrop<File> {
    // SAFETY: the descriptor stays open for the process's life; ManuallyDrop never
    // closes it.
    ManuallyDrop::new(unsafe { File::from_raw_fd(fd) })
}

impl Executor for RemoteExecutor {
    type Running = RemoteRun;

    /// The command is looked up on the member, not here.
    fn preflight(&self, _: &CommandLine) -> Result<()> {
        Ok(())
    }

    async fn start(&self, command: &CommandLine, context: &RunContext) -> RunResult<RemoteRun> {
        let mut report = Report {
            socket: self.socket.clone(),
            run_id: context.run_id.clone(),
            data: PlacedReport::default(),
        };
        let attached = tokio::time::timeout(
            self.start_by.since(Timestamp::now()),
            crate::local_run_client::attach(&self.socket, &context.run_id),
        )
        .await
        .unwrap_or_else(|_| Err(hold_limit_passed()));
        let mut link = match attached {
            Ok(stream) => stream,
            Err(failure) => {
                report.send().await;
                return Err(RunFailure::remote(
                    failure,
                    RemoteFailurePhase::WorkspaceSync,
                ));
            }
        };
        let merged = merged_output();
        let start = Start {
            program: command.program.as_bytes().to_vec(),
            arguments: command
                .arguments
                .iter()
                .map(|a| a.as_bytes().to_vec())
                .collect(),
            environment: environment::forwarded(&self.configuration, std::env::vars_os()),
            directory: self.located.prefix.clone(),
            merged,
            input: descriptor_open(0),
            outputs: self.configuration.outputs.clone(),
            consistency: None,
        };
        // The caller's own fingerprints are taken while the workspace syncs; they
        // never leave this process. Locations within synced paths are current by
        // construction and not compared.
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let declared = consistency::Declared::from(&self.configuration)
            .without_synced(&self.synced_paths, &home);
        let salt = (!declared.is_empty()).then(consistency::salt).flatten();
        let own_fingerprints = salt.clone().map(|salt| {
            let declared = declared.clone();
            let root = self.located.root.clone();
            let locations: Vec<_> = self
                .configuration
                .configuration
                .iter()
                .filter(|l| declared.locations.contains(&l.display()))
                .cloned()
                .collect();
            tokio::task::spawn_blocking(move || {
                fingerprint::take(&declared, &locations, &salt, &home, None, &root)
            })
        });
        let remaining = self.start_by.since(Timestamp::now());
        let mut warnings = Vec::new();
        let started = tokio::time::timeout(remaining, async {
            let stats = sync::push(
                &mut SyncOverLink(&mut link),
                &self.located,
                &self.caches,
                &context.member,
                &self.configuration.workspace_refs,
            )
            .await?;
            report.data.reached = Reached::Synced;
            report.data.sync = Some(summary(&stats));
            let mut start = start;
            start.consistency = salt.map(|salt| consistency::Request {
                salt,
                declared: declared.clone(),
            });
            let message = RunMessage::Start(start);
            if serde_json::to_vec(&message).expect("messages serialize").len() > crate::wire::MAXIMUM_PAYLOAD {
                return Err(Failure::new(
                    "invalid_request",
                    "The command line and forwarded variables are too large to place.",
                    "Forward fewer variables, or pass large data through a file in the working tree.",
                ));
            }
            let execution_started = Instant::now();
            link.message(&message).await?;
            loop {
                match link.receive().await? {
                    Frame::Message(bytes) => match link::decode(&bytes)? {
                        RunMessage::Started { fingerprints } => {
                            if let Some(own) = own_fingerprints {
                                let own = own.await.unwrap_or_default();
                                let member = fingerprints.unwrap_or_default();
                                let mismatches = consistency::compare(&declared, &own, &member);
                                warnings =
                                    mismatches.warnings(&self.calling_member, &context.member);
                                if !mismatches.is_empty() {
                                    link.message(&RunMessage::Mismatched { mismatches }).await?;
                                }
                            }
                            return Ok(execution_started);
                        }
                        RunMessage::StartFailed { failure } | RunMessage::Failed { failure } => {
                            return Err(untrusted(failure));
                        }
                        _ => continue,
                    },
                    _ => continue,
                }
            }
        })
        .await
        .unwrap_or_else(|_| Err(hold_limit_passed()));
        let execution_started = match started {
            Ok(started) => started,
            Err(failure) => {
                let phase = if report.data.reached == Reached::Synced {
                    RemoteFailurePhase::Execution
                } else {
                    RemoteFailurePhase::WorkspaceSync
                };
                report.send().await;
                return Err(RunFailure::remote(failure, phase));
            }
        };
        report.data.reached = Reached::Started;
        report.data.warnings = warnings.clone();
        report.send().await;
        Ok(RemoteRun {
            workload: self.workload.clone(),
            plan_observation: None,
            link,
            execution_started,
            merged,
            input: start_input(),
            report,
            warnings,
            outputs: self.configuration.outputs.clone(),
            root: self.located.root.clone(),
        })
    }
}

fn summary(stats: &sync::SyncStats) -> SyncSummary {
    let member = &stats.member;
    SyncSummary {
        duration_ms: Some(stats.duration_ms),
        sent_bytes: Some(stats.sent_bytes),
        received_bytes: member.received_bytes,
        pack_bytes: member.pack_bytes,
        history_from_source: member.history_from_source,
        // What the caller sent and the member says it set: a member that predates
        // carried refs sets none.
        refs: stats
            .refs
            .iter()
            .filter(|name| member.refs.contains(name))
            .cloned()
            .collect(),
        refs_only_commits: stats.refs_only_commits.filter(|commits| *commits > 0),
        files_written: member.written,
        files_removed: member.removed + member.extras_removed,
        run_changes_undone: member.dirty,
        content_from_copy: member.reused_local,
        content_from_history: member.from_objects,
        full_listing: member.full_manifest,
        copy_files: member.files,
        copy_content_bytes: member.content_bytes,
        phases_ms: sync::phases(Some(&stats.phases), &member.phases),
    }
}

/// What the caller tells its own service about the placed run, for the record.
struct Report {
    socket: PathBuf,
    run_id: RunId,
    data: PlacedReport,
}
impl Report {
    async fn send(&self) {
        let route = format!("/v1/runs/{}/placed", self.run_id);
        // Records only: the run's outcome does not depend on it.
        let _ = client::post::<(), _>(&self.socket, &route, &self.data, Wait::Bounded).await;
    }
}

/// Reads the caller's standard input on its own thread, so a terminal or a pipe
/// that never ends never blocks the run, and the process can exit while it waits.
fn start_input() -> Option<mpsc::Receiver<Vec<u8>>> {
    if !descriptor_open(0) {
        return None;
    }
    let (sender, receiver) = mpsc::channel(4);
    std::thread::spawn(move || {
        let mut input = standard(0);
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            match input.read(&mut buffer) {
                Ok(0) => return,
                Ok(n) => {
                    if sender.blocking_send(buffer[..n].to_vec()).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
    });
    Some(receiver)
}

/// What a writer thread reports back: bytes written, or that the destination closed.
enum Written {
    Bytes(Stream, u64),
    Closed(Stream),
}

/// Writes one output stream on its own thread, so a slow reader slows only the
/// member's command, through the window, and never the link's heartbeats.
struct Writer {
    chunks: Option<std::sync::mpsc::Sender<Vec<u8>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Writer {
    fn start(stream: Stream, fd: i32, written: mpsc::UnboundedSender<Written>) -> Self {
        let (chunks, receiver) = std::sync::mpsc::channel::<Vec<u8>>();
        let thread = std::thread::spawn(move || {
            let mut output = standard(fd);
            let mut closed = false;
            while let Ok(chunk) = receiver.recv() {
                if closed {
                    continue;
                }
                match output.write_all(&chunk).and_then(|_| output.flush()) {
                    Ok(()) => {
                        let _ = written.send(Written::Bytes(stream, chunk.len() as u64));
                    }
                    Err(_) => {
                        closed = true;
                        let _ = written.send(Written::Closed(stream));
                    }
                }
            }
        });
        Self {
            chunks: Some(chunks),
            thread: Some(thread),
        }
    }
    fn write(&self, chunk: Vec<u8>) {
        if let Some(chunks) = &self.chunks {
            let _ = chunks.send(chunk);
        }
    }
    /// Waits until everything sent has been written.
    async fn finish(mut self) {
        self.chunks = None;
        if let Some(thread) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}

/// Forwards the caller's interrupt and termination signals to the member. Signals
/// the caller ignores stay ignored, as they would for a local command.
fn forward_signals(outbox: Outbox) -> Vec<tokio::task::JoinHandle<()>> {
    let mut tasks = Vec::new();
    for signal in FORWARDED {
        // SAFETY: sigaction with a null new action only reads the current one.
        let ignored = unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            libc::sigaction(signal, std::ptr::null(), &mut current);
            current.sa_sigaction == libc::SIG_IGN
        };
        if ignored {
            continue;
        }
        let Ok(mut stream) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(signal))
        else {
            continue;
        };
        let outbox = outbox.clone();
        tasks.push(tokio::spawn(async move {
            while stream.recv().await.is_some() {
                if outbox
                    .message(&RunMessage::Signal { signal })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }));
    }
    tasks
}

pub struct RemoteRun {
    workload: Option<crate::workload_metadata::Workload>,
    plan_observation: Option<crate::workload_metadata::Observation>,
    link: Link,
    execution_started: Instant,
    merged: bool,
    input: Option<mpsc::Receiver<Vec<u8>>>,
    report: Report,
    warnings: Vec<String>,
    /// The declared outputs, which alone may come back.
    outputs: Vec<WorkspacePath>,
    root: PathBuf,
}

impl RemoteRun {
    async fn pass(&mut self) -> Result<Exited> {
        let (written_sender, mut written) = mpsc::unbounded_channel();
        let stdout = Writer::start(Stream::Stdout, 1, written_sender.clone());
        let stderr = (!self.merged).then(|| Writer::start(Stream::Stderr, 2, written_sender));
        let signals = forward_signals(self.link.outbox());
        let mut unacknowledged_input = 0u64;
        // Output received but not yet written, per stream. A member that ignores
        // the window cannot make the caller buffer without bound.
        let mut unwritten = [0u64; 2];
        let slot = |stream: Stream| usize::from(stream == Stream::Stderr);
        let result = loop {
            let input_ready = unacknowledged_input < WINDOW;
            tokio::select! {
                frame = self.link.receive() => match frame {
                    Err(failure) => break Err(failure),
                    Ok(Frame::Stdout(bytes) | Frame::Stderr(bytes)) if bytes.len() as u64 + unwritten.iter().sum::<u64>() > 2 * WINDOW => {
                        break Err(protocol("output beyond its window"));
                    }
                    Ok(Frame::Stdout(bytes)) => {
                        unwritten[0] += bytes.len() as u64;
                        stdout.write(bytes);
                    }
                    Ok(Frame::Stderr(bytes)) => {
                        unwritten[1] += bytes.len() as u64;
                        match &stderr {
                            Some(stderr) => stderr.write(bytes),
                            None => stdout.write(bytes),
                        }
                    }
                    Ok(Frame::Message(bytes)) => match link::decode(&bytes) {
                        Err(failure) => break Err(failure),
                        Ok(RunMessage::Ack { stream: Stream::Stdin, bytes }) => {
                            unacknowledged_input = unacknowledged_input.saturating_sub(bytes);
                        }
                        Ok(RunMessage::InputClosed) => self.input = None,
                        Ok(RunMessage::Exited { exit, cpu_ms, plan_observation }) => {
                            let (observation, rejected) = crate::remote_run::protocol::received_observation(plan_observation, self.workload.as_ref());
                            self.plan_observation = observation;
                            if rejected {
                                self.warnings.push("The member returned invalid workload metadata; plan coverage is unknown.".into());
                            }
                            // A member's measurement is believed only within reason.
                            self.report.data.cpu_ms = cpu_ms.filter(|ms| *ms < crate::scheduling::record::MAXIMUM_CPU_MS);
                            break valid_exit(exit);
                        }
                        Ok(RunMessage::Failed { failure }) => break Err(untrusted(failure)),
                        Ok(_) => (),
                    },
                    Ok(_) => (),
                },
                Some(event) = written.recv() => {
                    let message = match event {
                        Written::Bytes(stream, bytes) => {
                            unwritten[slot(stream)] = unwritten[slot(stream)].saturating_sub(bytes);
                            RunMessage::Ack { stream, bytes }
                        }
                        Written::Closed(stream) => RunMessage::OutputClosed { stream },
                    };
                    if let Err(failure) = self.link.message(&message).await {
                        break Err(failure);
                    }
                }
                chunk = next_input(&mut self.input), if input_ready && self.input.is_some() => match chunk {
                    Some(chunk) => {
                        unacknowledged_input += chunk.len() as u64;
                        if let Err(failure) = self.link.send(Frame::Stdin(chunk)).await {
                            break Err(failure);
                        }
                    }
                    None => {
                        self.input = None;
                        if let Err(failure) = self.link.message(&RunMessage::InputEnded).await {
                            break Err(failure);
                        }
                    }
                },
            }
        };
        for task in signals {
            task.abort();
        }
        // Everything the command printed is on the caller's descriptors before the
        // run ends, even after a lost connection.
        stdout.finish().await;
        if let Some(stderr) = stderr {
            stderr.finish().await;
        }
        result
    }
}

/// At most 32 warnings of at most 500 characters, as records keep them.
fn bounded(warnings: Vec<String>) -> Vec<String> {
    let total = warnings.len();
    let mut kept: Vec<String> = warnings
        .into_iter()
        .take(32)
        .map(|w| outputs::sanitized(&w, 500))
        .collect();
    if total > 32 {
        kept.push(format!("{} more warnings were left out.", total - 32));
    }
    kept
}

/// A failure the member reported, made safe to show: a code of Elsecall's own
/// shape, and text without control characters, bounded.
fn untrusted(failure: Failure) -> Failure {
    let code_ok = !failure.code.is_empty()
        && failure.code.len() <= 64
        && failure
            .code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'_');
    Failure::new(
        if code_ok {
            &failure.code
        } else {
            "protocol_error"
        },
        outputs::sanitized(&failure.message, 500),
        outputs::sanitized(&failure.hint, 500),
    )
}

fn protocol(what: &str) -> Failure {
    Failure::new(
        "protocol_error",
        format!("The member sent {what}."),
        "Install the same Elsecall version on every member.",
    )
}

/// An exit the member reports, checked: a code a process can exit with, or a signal
/// that ends a process, so a member cannot make the caller stop or lie about
/// success.
fn valid_exit(exit: Exited) -> Result<Exited> {
    let ends = |signal: i32| {
        (1..32).contains(&signal)
            && !matches!(
                signal,
                libc::SIGSTOP
                    | libc::SIGTSTP
                    | libc::SIGTTIN
                    | libc::SIGTTOU
                    | libc::SIGCONT
                    | libc::SIGCHLD
                    | libc::SIGURG
                    | libc::SIGWINCH
                    | libc::SIGINFO
                    | libc::SIGIO
            )
    };
    match exit {
        Exited::Code { code } if (0..=255).contains(&code) => Ok(exit),
        Exited::Signal { signal } if ends(signal) => Ok(exit),
        _ => Err(protocol("an exit status no process can have")),
    }
}

async fn next_input(input: &mut Option<mpsc::Receiver<Vec<u8>>>) -> Option<Vec<u8>> {
    match input {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

struct ReturnedOutputs {
    warnings: Vec<String>,
    complete: bool,
}

impl ReturnedOutputs {
    fn incomplete(warning: String) -> Self {
        Self {
            warnings: vec![warning],
            complete: false,
        }
    }
}

impl RemoteRun {
    /// Brings back the declared outputs the run changed, each blob on its own.
    /// Problems are warnings: the command's exit stands. Nothing is replaced until
    /// the transfer ended, and an interrupt or a lost connection abandons it.
    async fn bring_back(&mut self) -> Result<ReturnedOutputs> {
        let mut interrupts = Vec::new();
        for kind in [
            tokio::signal::unix::SignalKind::interrupt(),
            tokio::signal::unix::SignalKind::terminate(),
            tokio::signal::unix::SignalKind::hangup(),
        ] {
            if let Ok(stream) = tokio::signal::unix::signal(kind) {
                interrupts.push(stream);
            }
        }
        if interrupts.is_empty() {
            return self.return_outputs().await;
        }
        let interrupted = async {
            let waits = interrupts.iter_mut().map(|s| Box::pin(s.recv()));
            futures_util::future::select_all(waits).await;
        };
        tokio::select! {
            result = self.return_outputs() => return result,
            // Rebuilds left behind are hidden temporaries beside their outputs.
            _ = interrupted => (),
        }
        self.tell_nothing_returned("the caller was interrupted while they came back")
            .await;
        Ok(ReturnedOutputs::incomplete(
            "Interrupted while outputs came back; none were replaced.".into(),
        ))
    }

    async fn return_outputs(&mut self) -> Result<ReturnedOutputs> {
        let not_returned =
            |why: &str| ReturnedOutputs::incomplete(format!("Outputs were not returned: {why}."));
        let (described, left_out) = loop {
            match self.link.receive().await? {
                Frame::Message(bytes) => match link::decode(&bytes) {
                    Ok(RunMessage::Outputs {
                        described,
                        not_returned,
                    }) => break (described, not_returned),
                    Ok(_) => continue,
                    Err(_) => return Ok(not_returned("the member sent an invalid message")),
                },
                _ => continue,
            }
        };
        let listing = receive_blob(&mut self.link).await?;
        let changes = match (described, listing) {
            (false, _) => {
                return self
                    .skip("the member could not take a snapshot of them; its record says why")
                    .await;
            }
            (true, None) => return self.skip("the member's description was too large").await,
            (true, Some(listing)) => serde_json::from_slice::<Vec<Changed>>(&listing),
        };
        let changes = match changes {
            Ok(changes) => changes,
            Err(_) => return self.skip("the member's description was invalid").await,
        };
        if let Err(problem) = outputs::validate(&changes, &self.outputs) {
            return self.skip(problem.trim_end_matches('.')).await;
        }
        let Ok(confined) = Confined::open(&self.root) else {
            return self.skip("the working tree could not be opened").await;
        };
        let mut warnings = Vec::new();
        let mut returning = tokio::task::block_in_place(|| Returning::prepare(&confined, changes));
        // What the member's snapshot left out, in the member's grouping.
        let described_shortfall = match not_returned::accept(&left_out, &self.outputs) {
            Ok(()) => {
                returning.shortfall.extend(&self.outputs, left_out);
                true
            }
            Err(problem) => {
                warnings.push(format!("Outputs: {problem}."));
                false
            }
        };
        let wanted = returning.wanted();
        self.link.message(&RunMessage::WantOutputs).await?;
        send_blob(
            &self.link,
            &serde_json::to_vec(&wanted).expect("digests serialize"),
        )
        .await?;
        // Each blob arrives whole, or not, on its own. Only a lost connection ends
        // the return, which then replaces nothing.
        let link = &mut self.link;
        let received: Result<()> = async {
            let mut index = 0;
            while index < wanted.len() {
                let mut state = Receiving::default();
                let mut failed: Option<(Cause, Option<String>)> = None;
                let transfer =
                    |e: std::io::Error| Some((Cause::TransferError, Some(e.to_string())));
                loop {
                    match link.receive().await? {
                        Frame::Content(chunk) if chunk.is_empty() => {
                            if failed.is_none()
                                && let Err(e) = tokio::task::block_in_place(|| {
                                    returning.complete(index, &mut state)
                                })
                            {
                                failed = transfer(e);
                            }
                            break;
                        }
                        Frame::Content(chunk) => {
                            if failed.is_none()
                                && let Err(e) = tokio::task::block_in_place(|| {
                                    returning.write(index, &chunk, &mut state)
                                })
                            {
                                failed = transfer(e);
                            }
                        }
                        Frame::Message(bytes) => match link::decode(&bytes) {
                            Ok(RunMessage::OutputNotSent { cause, detail }) => {
                                failed = failed.or(Some((cause, detail)));
                                break;
                            }
                            _ => {
                                // Out of step with the member: nothing more can be
                                // received, but what arrived is still returned.
                                let problem = "the member sent something else";
                                for rest in index..wanted.len() {
                                    returning.fail(
                                        rest,
                                        Cause::TransferError,
                                        Some(problem.into()),
                                    );
                                }
                                return Ok(());
                            }
                        },
                        _ => {
                            let problem = "the member sent something else";
                            for rest in index..wanted.len() {
                                returning.fail(rest, Cause::TransferError, Some(problem.into()));
                            }
                            return Ok(());
                        }
                    }
                }
                drop(state);
                if let Some((cause, detail)) = failed {
                    tokio::task::block_in_place(|| returning.fail(index, cause, detail));
                }
                index += 1;
            }
            Ok(())
        }
        .await;
        match received {
            Ok(()) => {
                let returned = tokio::task::block_in_place(|| returning.finish(&confined));
                let shortfall = returned.shortfall.entries();
                let complete = described_shortfall && shortfall.is_empty();
                // The member records the same; losing this message costs only that.
                let _ = self
                    .link
                    .message(&RunMessage::Returned {
                        not_returned: shortfall,
                        nothing: None,
                    })
                    .await;
                warnings.extend(returned.notes);
                warnings.extend(returned.shortfall.warnings());
                Ok(ReturnedOutputs { warnings, complete })
            }
            Err(failure) => {
                returning.abandon();
                if failure.code == "connection_lost" {
                    return Err(failure);
                }
                warnings.push(format!("Outputs were not returned: {}.", failure.message));
                Ok(ReturnedOutputs {
                    warnings,
                    complete: false,
                })
            }
        }
    }

    /// Asks for nothing, so the member ends the run, and tells it why, for its
    /// record.
    async fn skip(&mut self, why: &str) -> Result<ReturnedOutputs> {
        self.link.message(&RunMessage::WantOutputs).await?;
        send_blob(&self.link, b"[]").await?;
        self.tell_nothing_returned(why).await;
        Ok(ReturnedOutputs::incomplete(format!(
            "Outputs were not returned: {why}."
        )))
    }

    /// Tells the member that nothing came back, and why; losing this message costs
    /// only that line in the member's record.
    async fn tell_nothing_returned(&self, why: &str) {
        let _ = self
            .link
            .message(&RunMessage::Returned {
                not_returned: Vec::new(),
                nothing: Some(why.into()),
            })
            .await;
    }
}

async fn send_blob(link: &Link, bytes: &[u8]) -> Result<()> {
    for chunk in bytes.chunks(crate::wire::CHUNK) {
        link.send(Frame::Content(chunk.to_vec())).await?;
    }
    link.send(Frame::Content(Vec::new())).await
}

/// A blob of at most the listing bound; `None` when the member sent more.
async fn receive_blob(link: &mut Link) -> Result<Option<Vec<u8>>> {
    let mut blob = Vec::new();
    let mut overflow = false;
    loop {
        match link.receive().await? {
            Frame::Content(chunk) if chunk.is_empty() => return Ok((!overflow).then_some(blob)),
            Frame::Content(chunk) => {
                if blob.len() + chunk.len() > outputs::MAXIMUM_LISTING {
                    overflow = true;
                    blob = Vec::new();
                } else if !overflow {
                    blob.extend_from_slice(&chunk);
                }
            }
            _ => return Ok(None),
        }
    }
}

impl Running for RemoteRun {
    fn warnings(&self) -> Vec<String> {
        self.warnings.clone()
    }

    async fn wait(mut self) -> RunResult<Finished> {
        match self.pass().await {
            Ok(exited) => {
                // The member sends Exited after draining stdout and stderr. Any
                // work carried by those streams is part of execution.
                let execution_ended = Instant::now();
                // The exit stands even if declared outputs could not come back.
                let returned = self.bring_back().await.unwrap_or_else(|_| {
                    ReturnedOutputs::incomplete(
                        "Outputs were not returned: the connection was lost.".into(),
                    )
                });
                let returned_at = Instant::now();
                let (result_at, result_return) = match (self.outputs.is_empty(), returned.complete)
                {
                    // With no declared outputs, the result was already usable
                    // when the exit and standard streams were known.
                    (true, true) => (Some(execution_ended), None),
                    (true, false) => (Some(execution_ended), Some(ResultReturn::Failed)),
                    (false, true) => (
                        Some(returned_at),
                        Some(ResultReturn::Returned(
                            returned_at.duration_since(execution_ended),
                        )),
                    ),
                    (false, false) => (None, Some(ResultReturn::Failed)),
                };
                self.warnings.extend(returned.warnings);
                self.warnings = bounded(self.warnings);
                self.report.data.warnings = self.warnings.clone();
                let _ = self.link.message(&RunMessage::Done).await;
                self.link.close().await;
                let exit = match exited {
                    Exited::Code { code } => {
                        self.report.data.exit_code = Some(code);
                        Exit::Code(code)
                    }
                    Exited::Signal { signal } => {
                        self.report.data.signal = Some(signal);
                        Exit::Signal(signal)
                    }
                };
                self.report.data.reached = Reached::Finished;
                self.report.send().await;
                Ok(Finished {
                    plan_observation: self.plan_observation,
                    exit,
                    warnings: self.warnings,
                    cpu: self
                        .report
                        .data
                        .cpu_ms
                        .map(std::time::Duration::from_millis),
                    execution: execution_ended.duration_since(self.execution_started),
                    result_at,
                    result_return,
                })
            }
            Err(failure) => {
                self.report.data.warnings = bounded(self.warnings);
                self.report.send().await;
                Err(RunFailure::remote(failure, RemoteFailurePhase::Execution))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real execution/return consumer against an external peer stream. Invalid
    /// optional telemetry must not turn a confirmed exit into HolderExited.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_peer_observations_preserve_exit_and_later_return_failure() {
        use crate::workload_metadata::{Observation, parse_workload};
        use serde_json::json;
        let plan = parse_workload(&json!({"kind":"sequential","planned_units":3,"stop_policy":"continue_all","estimate_key":"a".repeat(64),"variant_key":"b".repeat(64)}).to_string()).unwrap();
        let cases = vec![
            (None, r#"{"status":"unknown"}"#.to_owned(),true),
            (Some(plan.clone()), json!({"status":"all_units_observed","coverage":{"attempted_units":4,"terminal_units":3,"failed_units":0}}).to_string(),true),
            (Some(plan.clone()), json!({"status":"unknown","secret":"x".repeat(4096)}).to_string(),true),
            (Some(plan.clone()), "1e999".into(),true),
            (Some(plan.clone()),format!("{}0{}","[".repeat(200),"]".repeat(200)),true),
            (Some(plan.clone()),"null".into(),false),
            (Some(plan.clone()),json!({"status":"all_units_observed","coverage":{"attempted_units":3,"terminal_units":3,"failed_units":1}}).to_string(),false),
        ];
        for (workload, observation, rejected) in cases {
            let root = tempfile::tempdir().unwrap();
            let (stream, mut peer) = tokio::io::duplex(16384);
            let terminal = format!(
                r#"{{"type":"exited","exit":{{"kind":"code","code":7}},"cpu_ms":123,"plan_observation":{observation}}}"#
            );
            let sender = tokio::spawn(async move {
                crate::wire::write(&mut peer, &Frame::Message(terminal.into_bytes()))
                    .await
                    .unwrap();
                // A lost external peer during output return, after Exited.
                use tokio::io::AsyncWriteExt;
                peer.shutdown().await.unwrap();
            });
            let declared = workload.is_some();
            let running = RemoteRun {
                workload,
                plan_observation: None,
                link: Link::open(stream),
                execution_started: Instant::now(),
                merged: true,
                input: None,
                report: Report {
                    socket: root.path().join("absent.sock"),
                    run_id: RunId::parse("r-000000000001").unwrap(),
                    data: PlacedReport::default(),
                },
                warnings: Vec::new(),
                outputs: vec![WorkspacePath::parse("build/result").unwrap()],
                root: root.path().to_owned(),
            };
            let finished = running
                .wait()
                .await
                .unwrap_or_else(|_| panic!("confirmed exit must survive invalid metadata"));
            assert_eq!(finished.exit, Exit::Code(7));
            assert_eq!(finished.cpu, Some(std::time::Duration::from_millis(123)));
            assert_eq!(finished.result_return, Some(ResultReturn::Failed));
            assert_eq!(
                finished
                    .warnings
                    .iter()
                    .any(|w| w.contains("invalid workload metadata")),
                rejected
            );
            if rejected {
                assert_eq!(
                    finished.plan_observation,
                    declared.then_some(Observation::Unknown {})
                );
            }
            assert!(!finished.warnings.iter().any(|w| w.contains("secret")));
            sender.await.unwrap();
        }
    }

    #[test]
    fn only_exits_a_process_can_have_are_believed() {
        assert!(valid_exit(Exited::Code { code: 0 }).is_ok());
        assert!(valid_exit(Exited::Code { code: 255 }).is_ok());
        assert!(
            valid_exit(Exited::Signal {
                signal: libc::SIGTERM
            })
            .is_ok()
        );
        for bad in [
            Exited::Code { code: 256 },
            Exited::Code { code: -1 },
            Exited::Signal { signal: 0 },
            Exited::Signal {
                signal: libc::SIGSTOP,
            },
            Exited::Signal { signal: 64 },
        ] {
            assert_eq!(valid_exit(bad).unwrap_err().code, "protocol_error");
        }
    }
}
