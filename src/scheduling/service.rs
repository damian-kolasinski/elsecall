//! The service side of admission: this member's queue, grants, timers and records,
//! and the requests it places on, or hosts for, other members.
mod auto;
mod expiry;
mod hosting;
mod initial;
mod internal;
pub use internal::{InternalHold, InternalPause};
#[cfg(test)]
mod runtime_tests;
pub use hosting::{HostedEnd, HostedExit};
mod placing;
mod quotes;
mod ratio_cache;
mod running;
mod running_estimates;
mod upgrading;
pub use upgrading::{HostingHold, WorkInFlight};

use crate::{
    configuration::ServiceConfiguration,
    failure::{Failure, Result},
    inspection::{CoreCounts, MachineInspector},
    load_log::{LoadLog, minute::Second},
    scheduling::{
        admission::{Admission, Class},
        binding::{Bindings, Lost},
        busy::{self, Situation, Work},
        capacity::{Capacity, Cores},
        clock::{Clock, SystemClock},
        estimates::{self, HoldBasis},
        learning,
        leftovers::Leftovers,
        load::{Gate, LoadHistory},
        messages::{Grant, MemberState, Runs},
        parameters::Parameters,
        placed::{Members, PlacedGrant, Side},
        pre_run_sync::PreRunSync,
        processes::{self, ProcessIdentity, ProcessInfo, ProcessTable},
        record::{
            Candidate, Decision, HoldBasis as RecordBasis, HoldLimitRecord, Outcome, Record,
            SizeBasis, State, Verdict,
        },
        records::Records,
        recovery,
        request::{self, AdmissionRequest, AtDeadline, Kind, Placement, Urgency, WorkRequest},
        run_id::{RandomRunIds, RunId, RunIds},
        speed::{self, SpeedHistory, SpeedMeter},
        timestamp::Timestamp,
    },
};
use auto::Auto;
use placing::Placing;
use quotes::Quotes;
use serde::Serialize;
use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    path::Path,
    sync::{Arc, Mutex, MutexGuard, Weak},
    time::Duration,
};
use tokio::time::Instant;
use tokio::{
    sync::{mpsc, oneshot},
    task::AbortHandle,
};

/// How often live load is sampled and waiting work reconsidered.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// How often ended records past their retention are deleted.
const PRUNE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The system boundaries admission depends on.
pub struct Adapters {
    pub machine: Arc<dyn MachineInspector>,
    /// Measures one core's speed, for placement on the master.
    pub speed: Arc<dyn SpeedMeter>,
    pub processes: Arc<dyn ProcessTable>,
    /// PIDs of watched processes that exited, from the process table adapter.
    pub exits: mpsc::UnboundedReceiver<u32>,
}

/// Time and identity supplied at the scheduler boundary. Production uses the
/// wall clock and random IDs; deterministic execution supplies virtual adapters.
pub struct Runtime {
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn RunIds>,
    pub driving: Driving,
}

/// Whether periodic observations are driven by service tasks or an event driver.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Driving {
    Background,
    Controlled,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            clock: Arc::new(SystemClock),
            ids: Arc::new(RandomRunIds),
            driving: Driving::Background,
        }
    }
}

/// Already-received pending work at an initial-state boundary. The adapter has
/// validated the request and preserved the original size/hold resolution.
/// Receiving it again must not learn a new size or restart its hold clock.
pub struct AdmissionSnapshot {
    pub request: WorkRequest,
    pub received_at: Timestamp,
    pub size_basis: SizeBasis,
    pub hold_limit: Option<HoldLimitRecord>,
    pub expected_run: Option<Duration>,
}

#[derive(Clone, Copy)]
enum HoldResolution {
    Fresh,
    Interactive(HoldLimitRecord),
    Deferrable(Option<Duration>),
}

/// Background work of a running scheduler, stopped when dropped.
pub struct Tasks(Vec<AbortHandle>);
impl Drop for Tasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

pub struct Scheduler {
    /// The scheduler itself, for timers that must not keep it alive.
    this: Weak<Self>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn RunIds>,
    /// The host name up to its first dot, for a member not in a fleet.
    hostname: String,
    capacity: Capacity,
    machine: Arc<dyn MachineInspector>,
    processes: Arc<dyn ProcessTable>,
    members: Arc<dyn Members>,
    /// Brings a member's synced configuration up to date before a run is placed there.
    pre_run: Arc<dyn PreRunSync>,
    records: Records,
    state: Mutex<Book>,
    speed: Mutex<SpeedHistory>,
    /// Speed ratios learned from run times, kept for a minute.
    ratios: ratio_cache::RatioCache,
    /// What placement, the load gate and learned estimates decide with.
    parameters: Parameters,
    /// This member's load over time, from the same samples as the gate.
    load_log: Arc<LoadLog>,
}

/// Everything admission knows, guarded together so decisions see one state.
struct Book {
    admission: Admission,
    internal_paused: Option<Timestamp>,
    /// While positive, Elsecall's own queued work is kept out of admission.
    internal_held: usize,
    load: LoadHistory,
    entries: HashMap<RunId, Entry>,
    bindings: Bindings,
    /// Requests waiting on this master for another member, per member, in order.
    placing: HashMap<String, VecDeque<RunId>>,
    /// Members whose waiting requests a task is offering.
    offering: HashSet<String>,
    /// Offer loops suspended until their next retry, apart from loops that can
    /// progress immediately. An event driver must drain the latter before time moves.
    offer_retry: HashMap<String, Instant>,
    /// What every other member last said, for placeable requests.
    quotes: Quotes,
    /// What history expects of running work nothing else expects anything of.
    running: running_estimates::RunningEstimates,
    /// Set once the service is shutting down; new requests are refused.
    stopping: bool,
    /// While positive, runs other members would place here are refused as busy.
    hosting_held: usize,
    /// Supervised background processes left by placed commands.
    lingering: HashSet<RunId>,
}
struct Entry {
    record: Record,
    expected_run: Option<Duration>,
    phase: Phase,
    /// A run placed here by another member: its runner, not its processes, ends it.
    hosted: bool,
    /// A placeable request this master places, while it waits.
    auto: Option<Box<Auto>>,
}
enum Phase {
    Waiting(Waiting),
    /// Waiting on this master for another member to admit it.
    Placing(Box<Placing>),
    /// Holds capacity until every holder exited or the work ended.
    Active {
        holders: BTreeSet<ProcessIdentity>,
    },
}
/// A queued request; the exit of its requester or holder cancels it.
struct Waiting {
    requester: ProcessIdentity,
    holder: ProcessIdentity,
    /// When the hold limit or deadline passes, on the wall clock; `None` once a
    /// deferrable request has been moved ahead at its deadline.
    expires_at: Option<Timestamp>,
    responder: oneshot::Sender<Result<Grant>>,
    timer: Option<AbortHandle>,
}
impl Entry {
    fn processes(&self) -> Vec<ProcessIdentity> {
        match &self.phase {
            Phase::Waiting(waiting) if waiting.holder == waiting.requester => {
                vec![waiting.requester]
            }
            Phase::Waiting(waiting) => vec![waiting.requester, waiting.holder],
            Phase::Placing(placing) => vec![placing.requester],
            Phase::Active { holders } => holders.iter().copied().collect(),
        }
    }
}
impl Phase {
    /// The caller still waiting for this request's decision, if any.
    fn responder(self) -> Option<oneshot::Sender<Result<Grant>>> {
        match self {
            Phase::Waiting(waiting) => Some(waiting.responder),
            Phase::Placing(placing) => Some(placing.responder),
            Phase::Active { .. } => None,
        }
    }
}

fn log(event: &str, detail: impl Serialize) {
    crate::service_log!("{}", serde_json::json!({"event": event, "detail": detail}));
}

/// The member's name: the host name up to its first dot.
pub fn member_name(hostname: Option<&str>) -> String {
    hostname
        .and_then(|name| name.split('.').next())
        .filter(|name| !name.is_empty())
        .unwrap_or("local")
        .to_owned()
}

fn not_found(id: &RunId) -> Failure {
    Failure::new(
        "not_found",
        format!("No active run or lease {id} on this member."),
        "List recent work with elsecall runs.",
    )
}

fn stopping() -> Failure {
    Failure::new(
        "service_unavailable",
        "The local service stopped before the request started.",
        "Retry once the service is back; run elsecall doctor if it does not return.",
    )
}

/// This member's gate and the processes holding its active grants.
#[derive(Clone, Debug, PartialEq)]
pub struct OwnLoad {
    pub at: Timestamp,
    pub gate: Gate,
    pub holders: Vec<u32>,
    /// When the earliest active grant started; `None` without active grants.
    pub granted_since: Option<Timestamp>,
}

/// Where a request runs.
enum Route {
    /// On this member, with a note saying why when the caller asked for more.
    Here(Option<String>),
    /// On the named member, placed by this master.
    There(String),
    /// On the member with the earliest expected completion, chosen by this master.
    Auto,
}

impl Scheduler {
    /// Opens the records, recovers grants whose holders survived a restart, and
    /// starts sampling load and following process exits.
    pub fn start(
        records: &Path,
        configuration: ServiceConfiguration,
        adapters: Adapters,
        members: Arc<dyn Members>,
        pre_run: Arc<dyn PreRunSync>,
        load_log: Arc<LoadLog>,
    ) -> Result<(Arc<Self>, Tasks)> {
        Self::start_with_runtime(
            records,
            configuration,
            adapters,
            members,
            pre_run,
            load_log,
            Runtime::default(),
        )
    }

    /// Starts the real scheduler with explicitly supplied time and request IDs.
    pub fn start_with_runtime(
        records: &Path,
        configuration: ServiceConfiguration,
        adapters: Adapters,
        members: Arc<dyn Members>,
        pre_run: Arc<dyn PreRunSync>,
        load_log: Arc<LoadLog>,
        runtime: Runtime,
    ) -> Result<(Arc<Self>, Tasks)> {
        // Admission must work even when inspection fails, as other API routes do.
        let cores = adapters
            .machine
            .sample_load()
            .map(|s| s.cores)
            .unwrap_or_else(|error| {
                log("core_types_unavailable", &error);
                CoreCounts {
                    performance: std::thread::available_parallelism().map_or(1, |n| n.get() as u32),
                    efficiency: 0,
                }
            });
        let hostname = adapters.machine.inspect().ok().and_then(|m| m.hostname);
        let capacity = Capacity::of(cores, configuration.reserved);
        let records = Records::open(records)?;
        let meter = adapters.speed;
        records.save_scheduler_settings(&crate::scheduling::records::SchedulerSettings {
            at: runtime.clock.now(),
            capacity,
            parameters: configuration.parameters.clone(),
        })?;
        let driving = runtime.driving;
        let scheduler = Arc::new_cyclic(|this| Self {
            this: this.clone(),
            clock: runtime.clock,
            ids: runtime.ids,
            hostname: member_name(hostname.as_deref()),
            capacity,
            machine: adapters.machine,
            processes: adapters.processes,
            members,
            pre_run,
            records,
            state: Mutex::new(Book {
                admission: Admission::new(capacity.capacity),
                internal_paused: None,
                internal_held: 0,
                load: LoadHistory::default(),
                entries: HashMap::new(),
                bindings: Bindings::default(),
                placing: HashMap::new(),
                offering: HashSet::new(),
                offer_retry: HashMap::new(),
                quotes: Quotes::default(),
                running: Default::default(),
                stopping: false,
                hosting_held: 0,
                lingering: HashSet::new(),
            }),
            speed: Mutex::new(SpeedHistory::default()),
            ratios: Default::default(),
            parameters: configuration.parameters.clone(),
            load_log,
        });
        scheduler.recover()?;
        if driving == Driving::Controlled {
            return Ok((scheduler, Tasks(Vec::new())));
        }
        let sampler = tokio::spawn(sample(Arc::downgrade(&scheduler)));
        let exits = tokio::spawn(follow_exits(Arc::downgrade(&scheduler), adapters.exits));
        let speed = tokio::spawn(measure_speed(Arc::downgrade(&scheduler), meter));
        let placement = tokio::spawn(auto::reconsider(Arc::downgrade(&scheduler)));
        Ok((
            scheduler,
            Tasks(vec![
                sampler.abort_handle(),
                exits.abort_handle(),
                speed.abort_handle(),
                placement.abort_handle(),
            ]),
        ))
    }

    /// Samples the machine, expires due requests and runs the real admission
    /// pump. The service sampler and deterministic event driver share this path.
    pub fn tick(&self) -> Result<()> {
        let sample = self.machine.sample_load();
        let second = {
            let mut book = self.book();
            match sample {
                Ok(sample) => book.load.record(sample),
                Err(_) => book.load.record_unavailable(),
            }
            self.expire_due(&mut book, self.clock.now());
            self.pump(&mut book);
            self.second(&book)
        };
        self.load_log.observe(second)
    }

    /// Refreshes observations and runs the same periodic placement round as the
    /// service task. Calling it without waiting placeable work is inexpensive.
    pub async fn reconsider_waiting(&self) {
        let waiting = self
            .book()
            .entries
            .values()
            .any(|e| e.auto.is_some() && !matches!(e.phase, Phase::Active { .. }));
        if waiting {
            self.refresh(None).await;
            self.reconsider(&mut self.book());
        }
    }

    /// Whether an offer or due expiry can still progress at this instant.
    /// Controlled adapters complete communication without external I/O, so an
    /// event driver drains this work before advancing its clock.
    pub fn has_immediate_work(&self) -> bool {
        let book = self.book();
        let monotonic = Instant::now();
        let now = self.clock.now();
        book.offering.iter().any(|target| {
            book.offer_retry
                .get(target)
                .is_none_or(|at| *at <= monotonic)
        }) || book.entries.values().any(|entry| match &entry.phase {
            Phase::Waiting(waiting) => waiting.expires_at.is_some_and(|at| at <= now),
            Phase::Placing(placing) => placing.expires_at <= now,
            Phase::Active { .. } => false,
        })
    }

    /// Member offer loops that can progress now, excluding future retry sleeps.
    pub fn ready_offer_targets(&self) -> Vec<String> {
        let book = self.book();
        let now = Instant::now();
        let mut ready: Vec<_> = book
            .offering
            .iter()
            .filter(|target| book.offer_retry.get(*target).is_none_or(|at| *at <= now))
            .cloned()
            .collect();
        ready.sort();
        ready
    }

    /// Whether the request is still awaiting a scheduling verdict. Once false,
    /// its response task can be drained without moving virtual time.
    pub fn is_awaiting_verdict(&self, id: &RunId) -> bool {
        self.book()
            .entries
            .get(id)
            .is_some_and(|entry| matches!(entry.phase, Phase::Waiting(_) | Phase::Placing(_)))
    }

    /// The next hold expiry, deadline or member-offer retry. Service timers use
    /// these same values; a virtual executor must not step past them.
    pub fn next_event_at(&self) -> Option<Timestamp> {
        let book = self.book();
        let now = self.clock.now();
        let monotonic = Instant::now();
        let expiries = book
            .entries
            .values()
            .filter_map(|entry| match &entry.phase {
                Phase::Waiting(waiting) => waiting.expires_at,
                Phase::Placing(placing) => Some(placing.expires_at),
                Phase::Active { .. } => None,
            });
        let retries = book
            .offer_retry
            .values()
            .map(|at| now.after(at.saturating_duration_since(monotonic)));
        expiries.chain(retries).min()
    }

    /// Records an observed core speed through the same history as the meter.
    pub fn observe_speed(&self, measured: f64) {
        self.speed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record(measured);
    }

    /// Historical scheduling inputs, including the latest snapshot before the window.
    pub fn settings_history(
        &self,
        since: Timestamp,
        until: Timestamp,
    ) -> Result<Vec<crate::scheduling::records::SchedulerSettings>> {
        self.records.scheduler_settings(since, until)
    }

    /// This member's name: its name in the fleet, or its host name outside one.
    pub fn name(&self) -> String {
        self.members
            .standing()
            .name
            .unwrap_or_else(|| self.hostname.clone())
    }

    fn is_own_name(&self, name: &str, fleet_name: Option<&str>) -> bool {
        fleet_name == Some(name) || name.eq_ignore_ascii_case(&self.hostname)
    }

    fn book(&self) -> MutexGuard<'_, Book> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, record: &Record) {
        if let Err(error) = self.records.save(record) {
            log("record_write_failed", &error);
        }
    }

    /// Saves an entry's record with its current processes and state.
    fn persist(&self, entry: &mut Entry) {
        entry.record.processes = entry.processes();
        entry.record.state = match entry.phase {
            Phase::Waiting(_) | Phase::Placing(_) => State::Queued,
            Phase::Active { .. } => State::Running,
        };
        self.save(&entry.record);
    }

    /// The load gate, given the share of the machine Elsecall's own grants hold.
    fn gate(&self, book: &Book) -> Gate {
        let own = crate::scheduling::load::own_share(
            book.admission.in_use(),
            self.capacity.weighted_cores,
        );
        book.load.gate(own, &self.parameters.admission)
    }

    // ----- Requests ------------------------------------------------------------

    /// Admits a request from the process `peer`, waiting until it starts or fails.
    pub async fn request(self: &Arc<Self>, peer: u32, request: AdmissionRequest) -> Result<Grant> {
        let now = self.clock.now();
        let mut request = request.validate(now)?;
        let size_basis = self.learn_size(&mut request, now);
        self.request_received(peer, request, now, size_basis, HoldResolution::Fresh)
            .await
    }

    /// Restores pending work received before the current clock. It still uses
    /// real admission/placement and process authorization, with its original
    /// resolved size, source, request timestamp and absolute expiry.
    pub async fn request_snapshot(
        self: &Arc<Self>,
        peer: u32,
        snapshot: AdmissionSnapshot,
    ) -> Result<Grant> {
        let hold = match (snapshot.request.urgency, snapshot.hold_limit) {
            (Urgency::Interactive { .. }, Some(limit)) => HoldResolution::Interactive(limit),
            (Urgency::Deferrable { .. }, None) => HoldResolution::Deferrable(snapshot.expected_run),
            _ => {
                return Err(request::invalid(
                    "The pending snapshot lacks its original resolved hold limit or urgency.",
                ));
            }
        };
        self.request_received(
            peer,
            snapshot.request,
            snapshot.received_at,
            snapshot.size_basis,
            hold,
        )
        .await
    }

    async fn request_received(
        self: &Arc<Self>,
        peer: u32,
        request: WorkRequest,
        now: Timestamp,
        size_basis: SizeBasis,
        hold_resolution: HoldResolution,
    ) -> Result<Grant> {
        let restoring = !matches!(hold_resolution, HoldResolution::Fresh);
        let lineage = processes::lineage(&*self.processes, peer);
        let Some((requester, ancestors)) = lineage.split_first() else {
            return Err(request::invalid(
                "The requesting process could not be identified.",
            ));
        };
        let holder = match request.kind {
            Kind::Run => requester.identity,
            Kind::Lease => ancestors
                .iter()
                .find(|p| Some(p.identity.pid) == request.holder_pid)
                .map(|p| p.identity)
                .ok_or_else(|| {
                    request::invalid(
                        "A lease holder must be the process that runs elsecall or one of its ancestors.",
                    )
                })?,
        };
        let expected_run = match hold_resolution {
            HoldResolution::Interactive(limit) => limit.expected_run_seconds,
            HoldResolution::Deferrable(expected) => expected,
            HoldResolution::Fresh => match &request.label {
                Some(label) => self
                    .records
                    .run_times(label)
                    .map(estimates::expected_run)
                    .unwrap_or_else(|error| {
                        log("run_times_unavailable", &error);
                        None
                    }),
                None => None,
            },
        };
        let id = self.ids.next();
        let (mut record, expires_after) = self.new_record(
            &id,
            &request,
            now,
            expected_run,
            requester.identity,
            hold_resolution,
        );
        record.size_basis = Some(size_basis);
        let expired_snapshot = restoring
            && matches!(request.urgency, Urgency::Interactive { .. })
            && now.after(expires_after) <= self.clock.now();
        let route = self.route(&request);
        // Placement asks the other members first, without holding the book;
        // nested work is admitted at once and never asks.
        let nested = Self::enclosing(&self.book(), ancestors).is_some();
        let auto = match &route {
            Ok(Route::Auto) if !nested && !expired_snapshot => {
                let workspace = request.workspace.clone().expect("routed with a workspace");
                self.refresh(Some(&workspace)).await;
                let family = request
                    .settings
                    .as_ref()
                    .and_then(|s| s.rule.as_ref())
                    .map(|rule| rule.pattern.as_str());
                let history_at = if restoring { self.clock.now() } else { now };
                let (runs, syncs, losses) =
                    self.auto_history(request.label.as_deref(), family, history_at);
                let runs = runs.with_plan(
                    request
                        .workload
                        .as_ref()
                        .and_then(crate::workload_metadata::Workload::planned_units),
                );
                Some(Box::new(Auto {
                    workspace,
                    runs,
                    syncs,
                    losses,
                    verdict: None,
                    arrival: None,
                    excluded: Default::default(),
                }))
            }
            _ => None,
        };
        let receiver = {
            let mut book = self.book();
            if book.stopping {
                return Err(stopping());
            }
            if request.kind == Kind::Lease
                && let Some(held) = Self::held_by(&book, holder)
            {
                return Ok(self.reacquire(&mut book, &held, request.size));
            }
            let mut entry = Entry {
                record,
                expected_run,
                phase: Phase::Active {
                    holders: BTreeSet::new(),
                },
                hosted: false,
                auto: None,
            };
            if expired_snapshot {
                let queued = book.admission.queued();
                let failure = self.situation(
                    &book,
                    request.size,
                    expires_after,
                    queued.interactive + queued.deferrable,
                );
                return Err(self.refuse(&book, entry, Verdict::FleetBusy, failure));
            }
            let target = match route {
                Err(failure) => {
                    return Err(self.refuse(&book, entry, Verdict::MemberUnavailable, failure));
                }
                Ok(Route::Here(note)) => {
                    entry.record.placement_note = note;
                    None
                }
                Ok(Route::There(target)) => Some(Some(target)),
                Ok(Route::Auto) if auto.is_some() => Some(None),
                // Its enclosing work ended while it was being routed.
                Ok(Route::Auto) => {
                    entry.record.placement_note = Some(
                        "Requested from inside other work, so it runs on the calling member."
                            .into(),
                    );
                    None
                }
            };
            if let Some(outer) = Self::enclosing(&book, ancestors) {
                if target.is_some() {
                    entry.record.placement_note = Some(format!(
                        "Requested from inside {outer}, so it runs on this member with it."
                    ));
                }
                return Ok(self.nest(&mut book, entry, holder, &outer));
            }
            let (responder, receiver) = oneshot::channel();
            match target {
                Some(None) => {
                    let auto = auto.expect("placeable work carries its placement");
                    let placing = Placing {
                        requester: requester.identity,
                        target: None,
                        workspace: auto.workspace.clone(),
                        expires_at: now.after(expires_after),
                        responder,
                        // Asking members took time out of the hold limit.
                        timer: self.timer(&id, now.after(expires_after).since(self.clock.now())),
                        refusal: None,
                        in_flight: None,
                        config_sync: None,
                        offered: false,
                        pre_run_recorded: false,
                    };
                    entry.auto = Some(auto);
                    self.place_auto(&mut book, entry, placing);
                    if book.entries.contains_key(&id)
                        && !self.bind(&mut book, &id, requester.identity)
                    {
                        self.cancel(&mut book, &id, "The caller exited before admission.");
                        return Err(request::invalid("The requesting process exited."));
                    }
                    self.pump(&mut book);
                }
                Some(Some(target)) => {
                    let workspace = request.workspace.clone().expect("routed with a workspace");
                    self.place(
                        &mut book,
                        entry,
                        target,
                        workspace,
                        now.after(expires_after),
                        responder,
                    );
                    if !self.bind(&mut book, &id, requester.identity) {
                        self.cancel(&mut book, &id, "The caller exited before admission.");
                        return Err(request::invalid("The requesting process exited."));
                    }
                }
                None => {
                    if !book.admission.fits(request.size) {
                        let failure = Failure::new(
                            "request_too_large",
                            format!(
                                "Size {} is larger than this member's capacity of {} cores.",
                                request.size, self.capacity.capacity
                            ),
                            "Request a smaller --size, or lower reserved_cores in the service configuration.",
                        );
                        return Err(self.refuse(&book, entry, Verdict::RequestTooLarge, failure));
                    }
                    entry.phase = Phase::Waiting(Waiting {
                        requester: requester.identity,
                        holder,
                        expires_at: Some(now.after(expires_after)),
                        responder,
                        timer: Some(self.timer(
                            &id,
                            if restoring {
                                now.after(expires_after).since(self.clock.now())
                            } else {
                                expires_after
                            },
                        )),
                    });
                    let class = match request.urgency {
                        Urgency::Interactive { .. } => Class::Interactive,
                        Urgency::Deferrable { .. } => Class::Deferrable,
                    };
                    let processes = entry.processes();
                    self.persist(&mut entry);
                    book.entries.insert(id.clone(), entry);
                    if !(book.entries[&id].record.internal
                        && matches!(request.urgency, Urgency::Deferrable { .. })
                        && book.internal_withheld())
                    {
                        book.admission.enqueue(id.clone(), request.size, class);
                    }
                    for process in processes {
                        if !self.bind(&mut book, &id, process) {
                            self.cancel(&mut book, &id, "The caller exited before admission.");
                            return Err(request::invalid("The requesting process exited."));
                        }
                    }
                    if restoring {
                        self.expire_due(&mut book, self.clock.now());
                    }
                    self.pump(&mut book);
                }
            }
            receiver
        };
        let mut pending = Pending {
            scheduler: self.this.clone(),
            id,
            receiver: Some(receiver),
        };
        let result = pending
            .receiver
            .as_mut()
            .expect("set above")
            .await
            .unwrap_or_else(|_| Err(stopping()));
        pending.receiver = None;
        result
    }

    /// Applies the label's learned size to a run whose caller declared none, and
    /// says where the size came from. A learned size never exceeds this member's
    /// capacity, and a member a learned size is offered to shrinks it to its own,
    /// so learning alone cannot make a request too large.
    fn learn_size(&self, request: &mut WorkRequest, now: Timestamp) -> SizeBasis {
        if request.size_declared {
            return SizeBasis::Declared;
        }
        let Some(label) = request
            .label
            .as_deref()
            .filter(|_| request.kind == Kind::Run)
        else {
            return SizeBasis::Default;
        };
        let trust = &self.parameters.estimates;
        let since = Timestamp::from_millis(
            now.millis()
                .saturating_sub(trust.recency.as_millis() as u64),
        );
        let records = match self
            .records
            .history(label, since, learning::records_read(trust))
        {
            Ok(records) => records,
            Err(error) => {
                log("run_history_unavailable", &error);
                return SizeBasis::Default;
            }
        };
        match learning::size(&learning::samples(&records, now, trust.recency), trust) {
            Some(learned) => {
                request.size = learned
                    .size
                    .min(self.capacity.capacity)
                    .max(Cores::from_millis(500));
                SizeBasis::Learned {
                    samples: learned.samples,
                }
            }
            None => SizeBasis::Default,
        }
    }

    /// A new record, and how long until its hold limit or deadline passes.
    fn new_record(
        &self,
        id: &RunId,
        request: &WorkRequest,
        now: Timestamp,
        expected_run: Option<Duration>,
        requester: ProcessIdentity,
        hold_resolution: HoldResolution,
    ) -> (Record, Duration) {
        let (hold_limit, expires_after) = match hold_resolution {
            HoldResolution::Interactive(limit) => (Some(limit), limit.seconds),
            HoldResolution::Deferrable(_) => {
                let Urgency::Deferrable { deadline, .. } = request.urgency else {
                    unreachable!("snapshot urgency was validated")
                };
                (None, deadline.since(now))
            }
            HoldResolution::Fresh => match request.urgency {
                Urgency::Interactive { hold_limit_ms } => {
                    let limit = estimates::hold_limit(
                        hold_limit_ms.map(Duration::from_millis),
                        expected_run,
                    );
                    let record = HoldLimitRecord {
                        seconds: limit.limit,
                        basis: match limit.basis {
                            HoldBasis::Requested => RecordBasis::Requested,
                            HoldBasis::Default { .. } => RecordBasis::Default,
                        },
                        expected_run_seconds: expected_run,
                    };
                    (Some(record), limit.limit)
                }
                Urgency::Deferrable { deadline, .. } => (None, deadline.since(now)),
            },
        };
        let record = Record {
            run_id: id.clone(),
            kind: request.kind,
            synthetic: crate::rehearsal::synthetic(request.label.as_deref()),
            internal: requester.pid == std::process::id()
                && request
                    .label
                    .as_deref()
                    .is_some_and(|l| l.starts_with("elsecall "))
                && request.placement == Placement::Pinned,
            peer_io: None,
            label: request.label.clone(),
            workload: request.workload.clone(),
            plan_observation: None,
            placement: request.placement.clone(),
            urgency: request.urgency,
            size: request.size,
            size_basis: None,
            calling_member: self.name(),
            workspace_recorded: true,
            workspace: request
                .workspace
                .as_ref()
                .map(|w| crate::scheduling::record::WorkspaceIdentity::of(&self.name(), w)),
            member: None,
            placement_note: None,
            nested_in: None,
            shares_capacity_of: None,
            state: State::Queued,
            outcome: None,
            requested_at: now,
            started_at: None,
            ended_at: None,
            waited_ms: None,
            run_ms: None,
            run_phases: None,
            cpu_ms: None,
            hold_limit,
            decision: None,
            warnings: Vec::new(),
            requested_by: requester,
            processes: Vec::new(),
            placed: None,
            settings: request.settings.clone(),
            reacquired: 0,
            config_sync: None,
            pre_run_history: Some(Default::default()),
            fallbacks: Vec::new(),
        };
        (record, expires_after)
    }

    /// Where a request runs. Only the master places work on other members; a
    /// placeable request made anywhere else, or outside a working tree, runs here.
    fn route(&self, request: &WorkRequest) -> Result<Route> {
        let standing = self.members.standing();
        let member = match &request.placement {
            Placement::Pinned => return Ok(Route::Here(None)),
            Placement::Placeable => {
                let note = if standing
                    .members
                    .iter()
                    .all(|m| Some(m) == standing.name.as_ref())
                {
                    "This member is in no fleet with other members, so placeable work runs on it."
                        .to_owned()
                } else if standing.master != standing.name {
                    format!(
                        "This member is not the master ({}), and only the master places work: the request runs on the calling member.",
                        standing.master.as_deref().unwrap_or("unknown")
                    )
                } else if request.workspace.is_none() {
                    "Requested outside a git working tree, so it runs on the calling member."
                        .to_owned()
                } else {
                    return Ok(Route::Auto);
                };
                return Ok(Route::Here(Some(note)));
            }
            Placement::Member(member) => member,
        };
        if self.is_own_name(member, standing.name.as_deref()) {
            return Ok(Route::Here(Some(
                "The required member is the calling member.".into(),
            )));
        }
        if standing.members.is_empty() {
            return Err(Failure::new(
                "member_unavailable",
                format!("Member {member} is not reachable: this member is not in a fleet yet."),
                format!("Drop --member, or require this member ({}).", self.name()),
            ));
        }
        if !standing.members.iter().any(|m| m == member) {
            return Err(Failure::new(
                "member_unavailable",
                format!("{member} is not a member of this fleet."),
                "List the members with elsecall fleet.",
            ));
        }
        if standing.master != standing.name {
            return Ok(Route::Here(Some(format!(
                "This member is not the master ({}), and only the master places work: the request runs on the calling member.",
                standing.master.as_deref().unwrap_or("unknown")
            ))));
        }
        if request.workspace.is_none() {
            return Ok(Route::Here(Some(
                "Requested outside a git working tree, so it runs on the calling member.".into(),
            )));
        }
        Ok(Route::There(member.clone()))
    }

    /// The nearest ancestor holding active work, which a request from inside it reuses.
    fn enclosing(book: &Book, ancestors: &[ProcessInfo]) -> Option<RunId> {
        ancestors.iter().find_map(|ancestor| {
            book.bindings
                .runs_of(ancestor.identity)
                .find(|id| {
                    book.entries
                        .get(*id)
                        .is_some_and(|e| matches!(e.phase, Phase::Active { .. }) && !e.is_remote())
                })
                .cloned()
        })
    }

    /// Fails a request at once, before it was queued.
    fn refuse(&self, book: &Book, mut entry: Entry, verdict: Verdict, failure: Failure) -> Failure {
        let id = entry.record.run_id.clone();
        entry.record.decision = Some(self.decision(book, &entry, verdict, &failure.message));
        entry.record.state = State::Ended;
        entry.record.ended_at = Some(self.clock.now());
        entry.record.outcome = Some(Outcome::Failed {
            code: failure.code.clone(),
        });
        self.save(&entry.record);
        failure.for_run(id)
    }

    fn nest(
        &self,
        book: &mut Book,
        mut entry: Entry,
        holder: ProcessIdentity,
        outer: &RunId,
    ) -> Grant {
        let id = entry.record.run_id.clone();
        let reason = format!("Requested from inside {outer}, so it reuses that grant.");
        entry.record.decision = Some(self.decision(book, &entry, Verdict::Nested, &reason));
        let root = book.admission.nest(id.clone(), outer);
        let outer_size = book.admission.size_of(&root).unwrap_or_default();
        if entry.record.size > outer_size {
            entry.record.warnings.push(format!(
                "Size {} is larger than the outer grant's {}; nested work runs within the outer grant.",
                entry.record.size, outer_size
            ));
        }
        entry.record.nested_in = Some(outer.clone());
        entry.record.shares_capacity_of = Some(root);
        entry.phase = Phase::Active {
            holders: BTreeSet::from([holder]),
        };
        let grant = self.begin(&mut entry, self.name(), None);
        book.entries.insert(id.clone(), entry);
        if !self.bind(book, &id, holder) {
            self.lost(book, &id, holder);
        }
        grant
    }

    /// Marks an entry as started on `member` and builds its grant.
    fn begin(&self, entry: &mut Entry, member: String, placed: Option<PlacedGrant>) -> Grant {
        let now = self.clock.now();
        let record = &mut entry.record;
        record.started_at = Some(now);
        record.member = Some(member.clone());
        record.waited_ms = Some(now.since(record.requested_at).as_millis() as u64);
        let grant = Grant {
            run_id: record.run_id.clone(),
            member,
            size: record.size,
            nested_in: record.nested_in.clone(),
            waited_ms: record.waited_ms.unwrap_or_default(),
            placement_note: record.placement_note.clone(),
            placed,
            reused: false,
        };
        self.persist(entry);
        grant
    }

    /// The active lease `holder` itself holds, which acquiring again returns.
    fn held_by(book: &Book, holder: ProcessIdentity) -> Option<RunId> {
        book.bindings
            .runs_of(holder)
            .find(|id| {
                book.entries.get(*id).is_some_and(|entry| {
                    entry.record.kind == Kind::Lease
                        && matches!(&entry.phase, Phase::Active { holders } if holders.contains(&holder))
                })
            })
            .cloned()
    }

    /// Returns the lease its holder already holds, as for a script that re-execs
    /// itself: nothing queues, and one release still ends it.
    fn reacquire(&self, book: &mut Book, id: &RunId, size: Cores) -> Grant {
        let entry = book.entries.get_mut(id).expect("held leases have entries");
        let record = &mut entry.record;
        record.reacquired = record.reacquired.saturating_add(1);
        if size > record.size {
            let warning = format!(
                "Acquired again with size {size}, larger than the lease's {}; the lease keeps its size.",
                record.size
            );
            if !record.warnings.contains(&warning) {
                record.warnings.push(warning);
            }
        }
        let grant = Grant {
            run_id: record.run_id.clone(),
            member: record.member.clone().unwrap_or_else(|| self.name()),
            size: record.size,
            nested_in: record.nested_in.clone(),
            waited_ms: 0,
            placement_note: record.placement_note.clone(),
            placed: None,
            reused: true,
        };
        self.persist(entry);
        grant
    }

    // ----- Admission -------------------------------------------------------------

    /// Starts every waiting request that may start now, in order.
    fn pump(&self, book: &mut Book) {
        let gate = self.gate(book);
        while let Some(id) = book.admission.ready(gate.admits()).cloned() {
            let reason = match book.entries[&id].record.urgency {
                Urgency::Deferrable {
                    deadline,
                    at_deadline: AtDeadline::Start,
                } if deadline <= self.clock.now() => {
                    "Deadline passed; started ahead of interactive work once it fit."
                }
                Urgency::Deferrable { .. } => "Capacity was free and no interactive work waited.",
                Urgency::Interactive { .. } => "Fit free capacity and was first in line.",
            };
            let entry = &book.entries[&id];
            let decision = if entry.auto.is_some() {
                let own = self.name();
                let reason = Self::start_reason(entry, &own);
                self.auto_decision(entry, Verdict::Started, Some(&own), &reason)
            } else {
                self.decision(book, entry, Verdict::Started, reason)
            };
            book.admission.start(&id);
            let mut entry = book.entries.remove(&id).expect("waiting work has an entry");
            entry.record.decision = Some(decision);
            let Phase::Waiting(waiting) = std::mem::replace(
                &mut entry.phase,
                Phase::Active {
                    holders: BTreeSet::new(),
                },
            ) else {
                unreachable!("only waiting work is queued")
            };
            entry.phase = Phase::Active {
                holders: BTreeSet::from([waiting.holder]),
            };
            if let Some(timer) = waiting.timer {
                timer.abort();
            }
            // A lease outlives the elsecall process that asked for it.
            if waiting.requester != waiting.holder {
                book.bindings.unbind(&id, waiting.requester);
            }
            let grant = self.begin(&mut entry, self.name(), None);
            book.entries.insert(id.clone(), entry);
            if waiting.responder.send(Ok(grant)).is_err() {
                self.end(
                    book,
                    &id,
                    Outcome::Cancelled {
                        reason: "The caller disconnected before the grant arrived.".into(),
                    },
                );
            }
        }
    }

    /// Work holding this member's capacity, for a `fleet_busy` report.
    fn holding(&self, book: &Book) -> Vec<Work> {
        let mut holding: Vec<_> = book
            .admission
            .holding()
            .filter_map(|(root, size)| {
                // A root that ended still holds capacity for its nested work.
                let work = book.entries.get(root).or_else(|| {
                    book.entries
                        .values()
                        .filter(|e| e.record.shares_capacity_of.as_ref() == Some(root))
                        .min_by_key(|e| &e.record.run_id)
                })?;
                Some(Work {
                    run_id: work.record.run_id.clone(),
                    kind: work.record.kind,
                    label: work.record.label.clone(),
                    size,
                    started_at: work.record.started_at,
                    expected_run: work.expected_run,
                })
            })
            .collect();
        holding.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then(a.run_id.cmp(&b.run_id))
        });
        holding
    }

    /// Why a request of `size` cannot start here now, as `fleet_busy`.
    fn situation(
        &self,
        book: &Book,
        size: Cores,
        hold_limit: Duration,
        others_waiting: usize,
    ) -> Failure {
        busy::fleet_busy(Situation {
            now: self.clock.now(),
            hold_limit,
            size,
            free: book.admission.free(),
            capacity: self.capacity.capacity,
            waiting: others_waiting,
            gate: self.gate(book),
            holding: self.holding(book),
        })
    }

    fn busy(&self, book: &Book, id: &RunId) -> Failure {
        let entry = &book.entries[id];
        let queued = book.admission.queued();
        self.situation(
            book,
            entry.record.size,
            entry
                .record
                .hold_limit
                .map_or(Duration::ZERO, |limit| limit.seconds),
            // Other requests still waiting, besides this one.
            (queued.interactive + queued.deferrable).saturating_sub(1),
        )
    }

    /// Ends waiting work with a failure, answering its caller.
    fn fail(&self, book: &mut Book, id: &RunId, verdict: Verdict, failure: Failure) {
        let failure = failure.for_run(id);
        let decision = self.decision(book, &book.entries[id], verdict, &failure.message);
        let outcome = Outcome::Failed {
            code: failure.code.clone(),
        };
        if let Some(responder) = self
            .close(book, id, outcome, Some(decision))
            .and_then(Phase::responder)
        {
            let _ = responder.send(Err(failure));
        }
        self.pump(book);
    }

    fn cancel(&self, book: &mut Book, id: &RunId, reason: &str) {
        let failure = Failure::new(
            "cancelled",
            reason,
            "Request again if the work is still needed.",
        );
        self.cancel_with(book, id, reason, failure);
    }

    fn cancel_with(&self, book: &mut Book, id: &RunId, reason: &str, failure: Failure) {
        let outcome = Outcome::Cancelled {
            reason: reason.into(),
        };
        if let Some(responder) = self
            .close(book, id, outcome, None)
            .and_then(Phase::responder)
        {
            let _ = responder.send(Err(failure.for_run(id)));
        }
        self.pump(book);
    }

    /// Ends active work and frees its capacity once its family has ended.
    fn end(&self, book: &mut Book, id: &RunId, outcome: Outcome) {
        self.close(book, id, outcome, None);
        self.pump(book);
    }

    /// Removes an entry in any phase and records how it ended.
    fn close(
        &self,
        book: &mut Book,
        id: &RunId,
        outcome: Outcome,
        decision: Option<Decision>,
    ) -> Option<Phase> {
        let mut entry = book.entries.remove(id)?;
        for process in entry.processes() {
            book.bindings.unbind(id, process);
        }
        match &entry.phase {
            Phase::Waiting(waiting) => {
                book.admission.withdraw(id);
                if let Some(timer) = &waiting.timer {
                    timer.abort();
                }
            }
            Phase::Placing(placing) => {
                placing.timer.abort();
                if let Some(target) = &placing.target
                    && let Some(queue) = book.placing.get_mut(target)
                {
                    queue.retain(|queued| queued != id);
                }
                // A sync that happened before an offer that then failed is kept.
                if entry.record.config_sync.is_none() {
                    entry.record.config_sync = placing.config_sync.as_ref().map(|d| d.record());
                }
            }
            // Work placed elsewhere holds no capacity here; ending it is harmless.
            Phase::Active { .. } => book.admission.end(id),
        }
        let now = self.clock.now();
        let record = &mut entry.record;
        record.state = State::Ended;
        record.ended_at = Some(now);
        record.run_ms = record
            .started_at
            .map(|started| now.since(started).as_millis() as u64);
        if record.workload.is_some() && record.plan_observation.is_none() {
            use crate::workload_metadata::{CensorReason, Observation};
            record.plan_observation = Some(match &outcome {
                _ if record.started_at.is_none() => Observation::Unknown {},
                Outcome::Signaled { .. } => Observation::Censored {
                    coverage: None,
                    reason: CensorReason::Interrupted,
                },
                Outcome::HolderExited => Observation::Censored {
                    coverage: None,
                    reason: CensorReason::HolderExited,
                },
                Outcome::Failed { code } if code == "connection_lost" => Observation::Censored {
                    coverage: None,
                    reason: CensorReason::TransportLost,
                },
                Outcome::Cancelled { .. } => Observation::Censored {
                    coverage: None,
                    reason: CensorReason::Cancelled,
                },
                _ => Observation::Unknown {},
            });
        }
        record.outcome = Some(outcome);
        if decision.is_some() {
            record.decision = decision;
        }
        self.save(record);
        Some(entry.phase)
    }

    fn decision(&self, book: &Book, entry: &Entry, verdict: Verdict, reason: &str) -> Decision {
        let now = self.clock.now();
        let chosen = matches!(verdict, Verdict::Started | Verdict::Nested);
        Decision {
            decided_at: now,
            verdict,
            reason: reason.into(),
            candidates: vec![Candidate {
                member: self.name(),
                chosen,
                why: if chosen {
                    "The calling member runs this request.".into()
                } else {
                    reason.into()
                },
                capacity: self.capacity,
                in_use: book.admission.in_use(),
                free: book.admission.free(),
                queued: book.admission.queued(),
                load: book.load.latest().cloned(),
                cpu_utilization_window: book.load.cpu_utilization(),
                gate: self.gate(book),
                expected_run_seconds: entry.expected_run,
                expected_completion: entry.expected_run.map(|run| now.after(run)),
                estimate: None,
                inputs: None,
            }],
            unreachable: Vec::new(),
            parameters: Some(self.parameters.clone()),
            scoring: None,
            arrival: None,
        }
    }

    /// The parameters this member decides with.
    pub fn parameters(&self) -> &Parameters {
        &self.parameters
    }

    // ----- Processes -------------------------------------------------------------

    /// Binds `id` to a process. False if the process already exited.
    fn bind(&self, book: &mut Book, id: &RunId, process: ProcessIdentity) -> bool {
        match book.bindings.bind(&*self.processes, id, process) {
            None => false,
            Some(lost) => {
                self.lose(book, lost);
                true
            }
        }
    }

    fn lose(&self, book: &mut Book, lost: Lost) {
        for (id, process) in lost {
            self.lost(book, &id, process);
        }
    }

    /// Takes a process exit from the process observation adapter. Controlled
    /// runtimes deliver exits directly instead of starting a channel task.
    pub fn process_exited(&self, pid: u32) {
        self.exited(pid);
    }

    fn exited(&self, pid: u32) {
        let mut book = self.book();
        let lost = book.bindings.exited(&*self.processes, pid);
        self.lose(&mut book, lost);
    }

    /// A process bound to `id` exited.
    fn lost(&self, book: &mut Book, id: &RunId, process: ProcessIdentity) {
        let (holders_left, hosted) = match book.entries.get_mut(id) {
            None => return,
            Some(Entry {
                phase: Phase::Waiting(_) | Phase::Placing(_),
                ..
            }) => (None, false),
            Some(Entry {
                phase: Phase::Active { holders },
                hosted,
                ..
            }) => {
                holders.remove(&process);
                (Some(holders.len()), *hosted)
            }
        };
        match holders_left {
            None => self.cancel(book, id, "The caller exited while waiting."),
            // A hosted run's runner reports how it ended.
            Some(0) if !hosted => self.end(book, id, Outcome::HolderExited),
            Some(_) => (),
        }
    }

    // ----- Reading state ---------------------------------------------------------

    pub fn member_state(&self) -> MemberState {
        let book = self.book();
        self.state_of(&book)
    }

    /// The gate now, and the processes holding active grants on this member
    /// with the earliest grant's start, read together.
    pub fn own_load(&self) -> OwnLoad {
        let book = self.book();
        let active: Vec<&Entry> = book
            .entries
            .values()
            .filter(|entry| matches!(entry.phase, Phase::Active { .. }))
            .collect();
        OwnLoad {
            at: self.clock.now(),
            gate: self.gate(&book),
            holders: active
                .iter()
                .flat_map(|entry| entry.processes())
                .map(|holder| holder.pid)
                .collect(),
            granted_since: active
                .iter()
                .filter_map(|entry| entry.record.started_at)
                .min(),
        }
    }

    pub fn effective_settings_fingerprint(&self) -> String {
        super::settings_identity::fingerprint(&self.parameters, self.capacity)
    }

    fn state_of(&self, book: &Book) -> MemberState {
        MemberState {
            member: self.name(),
            capacity: self.capacity,
            in_use: book.admission.in_use(),
            free: book.admission.free(),
            queued: book.admission.queued(),
            load: book.load.latest().cloned(),
            gate: self.gate(book),
            speed: self.speed(),
        }
    }

    /// What the load history keeps of this second.
    fn second(&self, book: &Book) -> Second {
        let latest = book.load.latest();
        let queued = book.admission.queued();
        Second {
            at: self.clock.now(),
            cpu_utilization: latest.and_then(|l| l.cpu_utilization),
            memory_pressure: latest.map(|l| l.memory_pressure),
            thermal_state: latest.map(|l| l.thermal_state),
            weighted_cores: self.capacity.weighted_cores,
            capacity: self.capacity.capacity,
            in_use: book.admission.in_use(),
            queued: queued.interactive + queued.deferrable,
            queued_interactive: queued.interactive,
            gate: self.gate(book),
            speed: self.speed(),
        }
    }

    /// One core's measured speed, the best of the recent measurements.
    pub fn speed(&self) -> Option<f64> {
        self.speed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current()
    }

    /// Durable live process bindings, including local grants restored after a
    /// service restart. Used by synthetic-execution recovery, without exposing
    /// the admission book or changing its recovery policy.
    pub fn active_records(&self) -> Result<Vec<Record>> {
        self.records.unfinished()
    }

    pub fn production_activity(&self, since: Timestamp) -> Result<(usize, usize)> {
        self.records.production_activity(since)
    }

    pub fn runs(&self, limit: u32) -> Result<Runs> {
        Ok(Runs {
            member: self.member_state(),
            runs: self
                .records
                .recent(limit)?
                .into_iter()
                .map(Into::into)
                .collect(),
        })
    }

    /// The latest `limit` placeable requests since `since`, oldest first, for
    /// replay.
    pub fn placeable_records(&self, since: Timestamp, limit: u32) -> Result<Vec<Record>> {
        self.records.placeable_since(since, limit)
    }

    /// Records of requests made from `since` to before `until`, newest first,
    /// continuing below `before` (a request time and run ID) when given.
    pub fn trace_records(
        &self,
        since: Timestamp,
        until: Timestamp,
        before: Option<(Timestamp, &str)>,
        limit: u32,
    ) -> Result<Vec<Record>> {
        self.records.overlapping(since, until, before, limit)
    }

    pub fn records_between(
        &self,
        since: Timestamp,
        until: Timestamp,
        before: Option<(Timestamp, &str)>,
        limit: u32,
    ) -> Result<Vec<Record>> {
        self.records.between(since, until, before, limit)
    }

    pub fn record(&self, id: &RunId) -> Result<Record> {
        let mut record = self.records.get(id)?.ok_or_else(|| {
            Failure::new(
                "not_found",
                format!("No record of {id} on this member."),
                "List recent work with elsecall runs.",
            )
        })?;
        record.peer_io = Some(crate::peer_diagnostics::between(
            record.requested_at,
            record.ended_at.unwrap_or_else(|| self.clock.now()),
        ));
        Ok(record)
    }

    // ----- Startup ---------------------------------------------------------------

    fn recover(&self) -> Result<()> {
        let now = self.clock.now();
        // What placed commands left running ends with the service that watched
        // it: nothing would cap it any more.
        for mut record in self.records.running_leftovers()? {
            let Some(placed) = record.placed.as_mut() else {
                continue;
            };
            let Some(
                leftovers @ Leftovers::Running {
                    group, processes, ..
                },
            ) = &placed.leftovers
            else {
                continue;
            };
            let mut stopped = false;
            for process in processes {
                stopped |= self.processes.kill_group(*process, *group);
            }
            placed.leftovers = Some(leftovers.restarted(now, stopped));
            if stopped {
                record.warnings.push(
                    "The service restarted while processes the command left running ran; they were stopped."
                        .into(),
                );
            }
            self.save(&record);
        }
        let (placed, unfinished): (Vec<Record>, Vec<Record>) = self
            .records
            .unfinished()?
            .into_iter()
            .partition(|record| record.placed.is_some());
        // A placed run's connection did not survive the restart: the command it
        // ran here is stopped, and either side records the loss.
        for mut record in placed {
            if record
                .placed
                .as_ref()
                .is_some_and(|p| p.side == Side::Member)
            {
                for process in &record.processes {
                    self.processes.kill_group(*process, process.pid);
                }
            }
            record.state = State::Ended;
            record.ended_at = Some(now);
            record.run_ms = record.started_at.map(|s| now.since(s).as_millis() as u64);
            record.outcome = Some(Outcome::Failed {
                code: "connection_lost".into(),
            });
            if let Some(placed) = &mut record.placed {
                placed.interrupted = Some(crate::scheduling::placed::Interruption::ServiceRestart);
            }
            record.warnings.push(
                "The service restarted during the run, which ended the connection; the command was stopped."
                    .into(),
            );
            self.save(&record);
        }
        let plan = recovery::plan(
            unfinished,
            now,
            |process| processes::alive(&*self.processes, process),
            |root| self.records.get(root).ok().flatten().map(|r| r.size),
        );
        for record in &plan.closed {
            self.save(record);
        }
        let mut book = self.book();
        for family in plan.families {
            book.admission.adopt(family.root.clone(), family.size);
            for survivor in family.members {
                let id = survivor.record.run_id.clone();
                if id != family.root {
                    book.admission.nest(id.clone(), &family.root);
                }
                book.entries.insert(
                    id.clone(),
                    Entry {
                        record: survivor.record,
                        expected_run: None,
                        phase: Phase::Active {
                            holders: survivor.alive.clone(),
                        },
                        hosted: false,
                        auto: None,
                    },
                );
                for process in survivor.alive {
                    if !self.bind(&mut book, &id, process) {
                        self.lost(&mut book, &id, process);
                    }
                }
            }
            if !family.root_alive {
                book.admission.end(&family.root);
            }
        }
        drop(book);
        self.records.prune(now)
    }
}

impl crate::fleet_view::PlacedRuns for Scheduler {
    fn latest_with(&self, member: &str) -> Option<crate::fleet_view::PlacedRunSummary> {
        let records = self.records.recent(200).ok()?;
        records
            .into_iter()
            .find(|record| {
                record
                    .placed
                    .as_ref()
                    .is_some_and(|placed| match placed.side {
                        Side::Caller => placed.member == member,
                        Side::Member => placed.calling_member == member,
                    })
            })
            .map(|record| crate::fleet_view::PlacedRunSummary {
                run_id: record.run_id.to_string(),
                started_at: record.started_at,
                warnings: record.warnings,
            })
    }
}

impl Entry {
    /// Work this member placed on another member.
    fn is_remote(&self) -> bool {
        self.record
            .placed
            .as_ref()
            .is_some_and(|placed| placed.side == Side::Caller)
    }
}

/// A request waiting for its decision. If the caller goes away first, as when its
/// connection closes and the handler is dropped, the request is withdrawn, or a
/// grant that was sent but never delivered is ended.
struct Pending {
    scheduler: Weak<Scheduler>,
    id: RunId,
    receiver: Option<oneshot::Receiver<Result<Grant>>>,
}
impl Drop for Pending {
    fn drop(&mut self) {
        let (Some(mut receiver), Some(scheduler)) =
            (self.receiver.take(), self.scheduler.upgrade())
        else {
            return;
        };
        let mut book = scheduler.book();
        match receiver.try_recv() {
            Ok(Ok(grant)) => scheduler.end(
                &mut book,
                &grant.run_id,
                Outcome::Cancelled {
                    reason: "The caller disconnected before the grant arrived.".into(),
                },
            ),
            Err(oneshot::error::TryRecvError::Empty) => {
                scheduler.cancel(
                    &mut book,
                    &self.id,
                    "The caller disconnected while waiting.",
                );
            }
            Ok(Err(_)) | Err(oneshot::error::TryRecvError::Closed) => (),
        }
    }
}

async fn sample(scheduler: Weak<Scheduler>) {
    let mut interval = tokio::time::interval(SAMPLE_INTERVAL);
    // After a stall, sample on, rather than several times in one second.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pruned = Instant::now();
    loop {
        interval.tick().await;
        let Some(scheduler) = scheduler.upgrade() else {
            return;
        };
        if let Err(error) = scheduler.tick() {
            log("load_history_write_failed", &error);
        }
        if pruned.elapsed() >= PRUNE_INTERVAL {
            pruned = Instant::now();
            if let Err(error) = scheduler.records.prune(scheduler.clock.now()) {
                log("record_prune_failed", &error);
            }
            if let Err(error) = scheduler.load_log.prune(scheduler.clock.now()) {
                log("load_history_prune_failed", &error);
            }
        }
    }
}

/// Measures this member's speed now and then. The workload is short and runs on
/// a blocking thread, so admission never waits for it.
async fn measure_speed(scheduler: Weak<Scheduler>, meter: Arc<dyn SpeedMeter>) {
    let mut interval = tokio::time::interval(speed::MEASURE_EVERY);
    loop {
        interval.tick().await;
        let meter = meter.clone();
        let measured = tokio::task::spawn_blocking(move || meter.measure()).await;
        let Some(scheduler) = scheduler.upgrade() else {
            return;
        };
        match measured {
            Ok(Ok(speed)) => scheduler
                .speed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record(speed),
            Ok(Err(error)) => log("speed_unavailable", &error),
            Err(_) => log("speed_unavailable", "the measurement panicked"),
        }
    }
}

async fn follow_exits(scheduler: Weak<Scheduler>, mut exits: mpsc::UnboundedReceiver<u32>) {
    while let Some(pid) = exits.recv().await {
        let Some(scheduler) = scheduler.upgrade() else {
            return;
        };
        scheduler.exited(pid);
    }
}

#[cfg(test)]
mod pending_snapshot_tests {
    use super::*;
    use crate::{
        inspection::{LoadSample, Machine, MemoryPressure, ThermalState},
        scheduling::{
            binding::fake::FakeProcesses, clock::VirtualClock, placed::Alone, pre_run_sync::Synced,
        },
    };
    use futures_util::future::BoxFuture;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct ObservedMachine;
    impl MachineInspector for ObservedMachine {
        fn inspect(&self) -> Result<Machine> {
            Err(Failure::new(
                "unavailable",
                "No host inspection in this fixture.",
                "",
            ))
        }
        fn sample_load(&self) -> Result<LoadSample> {
            Ok(LoadSample {
                cores: CoreCounts {
                    performance: 2,
                    efficiency: 0,
                },
                cpu_utilization: Some(0.0),
                memory_pressure: MemoryPressure::Normal,
                thermal_state: ThermalState::Nominal,
            })
        }
    }
    struct Sync;
    impl PreRunSync for Sync {
        fn bring_up_to_date(&self, _: &str) -> BoxFuture<'static, Result<Synced>> {
            Box::pin(async { Ok(Synced::NothingSynced) })
        }
    }
    struct Ids(AtomicU64);
    impl RunIds for Ids {
        fn next(&self) -> RunId {
            RunId::parse(&format!(
                "r-{:012x}",
                self.0.fetch_add(1, Ordering::Relaxed)
            ))
            .unwrap()
        }
    }
    fn request(size: u32, hold_ms: u64) -> AdmissionRequest {
        AdmissionRequest {
            workload: None,
            kind: Kind::Run,
            holder_pid: None,
            size: Some(Cores::from_millis(size)),
            label: Some("snapshot fixture".into()),
            placement: Placement::Pinned,
            urgency: Urgency::Interactive {
                hold_limit_ms: Some(hold_ms),
            },
            workspace: None,
            settings: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn pending_snapshot_keeps_original_expiry_resolved_size_and_basis() {
        for basis in [SizeBasis::Declared, SizeBasis::Learned { samples: 7 }] {
            let temp = tempfile::tempdir().unwrap();
            let clock = Arc::new(VirtualClock::new(Timestamp::from_millis(1000)));
            let processes = Arc::new(FakeProcesses::default());
            processes.start(100, 100, 0);
            processes.start(101, 101, 0);
            let (_, exits) = mpsc::unbounded_channel();
            let (scheduler, _tasks) = Scheduler::start_with_runtime(
                &temp.path().join("records.sqlite"),
                ServiceConfiguration::default(),
                Adapters {
                    machine: Arc::new(ObservedMachine),
                    speed: Arc::new(crate::machine_script::FixedSpeed(1.0)),
                    processes,
                    exits,
                },
                Arc::new(Alone),
                Arc::new(Sync),
                Arc::new(LoadLog::open(&temp.path().join("load.sqlite")).unwrap()),
                Runtime {
                    clock: clock.clone(),
                    ids: Arc::new(Ids(AtomicU64::new(1))),
                    driving: Driving::Controlled,
                },
            )
            .unwrap();
            scheduler.request(100, request(2000, 5000)).await.unwrap();
            tokio::time::advance(Duration::from_millis(500)).await;
            let received_at = Timestamp::from_millis(1000);
            let mut resolved = request(1250, 750).validate(received_at).unwrap();
            resolved.size_declared = matches!(basis, SizeBasis::Declared);
            let hold_limit = HoldLimitRecord {
                seconds: Duration::from_millis(750),
                basis: RecordBasis::Requested,
                expected_run_seconds: Some(Duration::from_millis(333)),
            };
            let pending = tokio::spawn({
                let scheduler = scheduler.clone();
                async move {
                    scheduler
                        .request_snapshot(
                            101,
                            AdmissionSnapshot {
                                request: resolved,
                                received_at,
                                size_basis: basis,
                                hold_limit: Some(hold_limit),
                                expected_run: Some(Duration::from_millis(333)),
                            },
                        )
                        .await
                }
            });
            while scheduler.member_state().queued.interactive != 1 {
                tokio::task::yield_now().await;
            }
            let id = RunId::parse("r-000000000002").unwrap();
            assert_eq!(
                scheduler.next_event_at(),
                Some(Timestamp::from_millis(1750))
            );
            let recorded = scheduler.record(&id).unwrap();
            assert_eq!(recorded.requested_at, received_at);
            assert_eq!(recorded.size, Cores::from_millis(1250));
            assert_eq!(recorded.size_basis, Some(basis));
            assert_eq!(recorded.hold_limit, Some(hold_limit));
            assert_eq!(
                scheduler.quote_state().waiting[0].expected_run_ms,
                Some(333)
            );
            tokio::time::advance(Duration::from_millis(249)).await;
            assert!(!pending.is_finished());
            tokio::time::advance(Duration::from_millis(1)).await;
            assert_eq!(pending.await.unwrap().unwrap_err().code, "fleet_busy");
            assert_eq!(
                scheduler.record(&id).unwrap().ended_at,
                Some(Timestamp::from_millis(1750))
            );
            assert_eq!(clock.now(), Timestamp::from_millis(1750));
            scheduler
                .run_ended(
                    100,
                    &RunId::parse("r-000000000001").unwrap(),
                    crate::scheduling::messages::RunEnded::Exited {
                        plan_observation: None,
                        exit_code: 0,
                        cpu_ms: None,
                        run_phases: None,
                    },
                )
                .unwrap();
            let expired = scheduler
                .request_snapshot(
                    101,
                    AdmissionSnapshot {
                        request: request(1250, 750).validate(received_at).unwrap(),
                        received_at,
                        size_basis: basis,
                        hold_limit: Some(hold_limit),
                        expected_run: Some(Duration::from_millis(333)),
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(expired.code, "fleet_busy");
            assert_eq!(scheduler.member_state().in_use, Cores::ZERO);
            assert!(
                scheduler.request(101, request(1250, 0)).await.is_ok(),
                "fresh zero-hold semantics stay unchanged"
            );
            for action in [AtDeadline::Skip, AtDeadline::Start] {
                let mut deferred = request(500, 5000);
                deferred.urgency = Urgency::Deferrable {
                    deadline: Timestamp::from_millis(1500),
                    at_deadline: action,
                };
                let result = scheduler
                    .request_snapshot(
                        100,
                        AdmissionSnapshot {
                            request: deferred.validate(received_at).unwrap(),
                            received_at,
                            size_basis: SizeBasis::Declared,
                            hold_limit: None,
                            expected_run: Some(Duration::from_millis(333)),
                        },
                    )
                    .await;
                match action {
                    AtDeadline::Skip => assert_eq!(result.unwrap_err().code, "deadline_skipped"),
                    AtDeadline::Start => assert_eq!(result.unwrap().waited_ms, 750),
                }
            }
        }
    }
}
