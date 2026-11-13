//! Where a placeable request runs: the member with the earliest expected
//! completion. Pure: the scheduler gathers what each member reported and what
//! history says, and acts on the verdict. Every value it decides with comes from
//! `parameters::Placement`; the defaults are named below.
//!
//! A candidate's **expected completion** is wait + sync + run:
//! - **Wait:** until enough of its capacity is free, assuming its running work
//!   ends at its expected run time and the requests ahead of this one start in
//!   order and run their expected time. Running work its member expects nothing
//!   of takes what history expects of its whole run (see `Held`). Running work
//!   nobody expects anything of is assumed to run `unknown_remaining` (60 s)
//!   more, or `unknown_remaining_per_elapsed` (0.5) of the time it has already
//!   run once that is longer; queued work, `unknown_remaining`.
//! - **Sync:** none on the caller; on a member, learned from history (see
//!   `sync_history`), or a prior by the copy's state: `sync_prior_warm` (3 s), or
//!   for a copy that does not exist yet `sync_prior_cold_with_history` (30 s) when
//!   the repository's history is there and `sync_prior_cold` (120 s) when it is not.
//! - **Run:** learned from history (see `learning`), else `run_prior` (60 s of work
//!   at speed 1) divided by the member's measured speed. A cold copy runs a cold
//!   build: learned from cold runs where they exist, else the warm estimate times
//!   `cold_run_factor` (3). The run is then slowed by contention: when outside load
//!   and Elsecall's own work leave fewer idle cores than the request's size, it runs
//!   that much slower, at most `maximum_slowdown` (4) times.
//!
//! - **Losses:** on another member, sync and run are then weighed by how often
//!   the member lost the connection to runs placed on it (see `loss_history`): at
//!   `λ` lost runs per second of placed work there, counted over at least
//!   `minimum_loss_exposure` (1 h) of it, work of `t` takes `(e^(λt) − 1) / λ` on
//!   average, since a lost run is run again from the start. Two-minute work on a
//!   member that loses a run an hour takes 2% longer; hour-long work, 72%. The
//!   calling member loses nothing.
//!
//! A candidate is **eligible** when it can start the request (wait plus sync)
//! within what is left of the hold limit. It never is while saturated by outside
//! load or under memory pressure ("never sent somewhere already saturated"), while
//! another run uses its copy of the workspace, or when the request is larger than
//! its capacity, which already leaves out its reserve.
//!
//! The **winner** is the eligible candidate with the earliest completion, except
//! that work stays on the caller unless another member completes earlier by the
//! keep-local margin (10%, at least 2 s): leaving costs a copy and warm state the
//! estimates cannot see. The cold priors are deliberately pessimistic (several
//! times the measured first syncs of large repositories), so a new workspace stays
//! on the caller unless the caller cannot start it within its hold limit or is
//! clearly worse off.
//!
//! **Inputs are exact.** Scoring first rounds every duration it was given to whole
//! milliseconds, and records exactly what it scored with (`Inputs`), so the same
//! inputs and parameters always score the same, also when read back from a record.
use crate::scheduling::{
    capacity::Cores,
    learning::{self, Conversions, RunHistory},
    load::Gate,
    loss_history::{LossHistory, Losses},
    messages::MemberState,
    parameters::{Admission, Placement},
    placed::{Warmth, WorkspaceRef},
    sync_history::{SyncBasis, SyncHistory},
    timestamp::{millis, seconds, whole_millis},
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Work holding a member's capacity, and what says how much longer it holds it.
///
/// Records keep it as `{"size", "remaining_ms", "elapsed_ms", "learned_run"}`,
/// the last two absent in records from before they were recorded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "HeldRecord", into = "HeldRecord")]
pub struct Held {
    pub size: Cores,
    /// How long its command has run. Absent in records from before it was
    /// recorded, whose work without an expected time keeps `unknown_remaining`,
    /// as it was scored then.
    pub elapsed: Option<Duration>,
    pub expectation: Expectation,
}

/// What is expected of running work's run time.
#[derive(Clone, Debug, PartialEq)]
pub enum Expectation {
    /// What is left of the run time its member expects (its label's median).
    Member { remaining: Duration },
    /// Its member expects nothing: its whole run as history estimates it on its
    /// member, by the chain new work's estimates take.
    Learned(LearnedRun),
    /// Nothing at all.
    Unknown,
}

/// A running request's whole run on its member, learned from history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LearnedRun {
    #[serde(flatten)]
    pub learned: LearnedTime,
    /// Its copy was cold and only warm runs were learned: the warm estimate times
    /// `cold_run_factor`, as for new work.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cold_from_warm: bool,
}

impl LearnedRun {
    /// What history says of a run into a warm or `cold` copy, as scoring takes it
    /// for new work: a learned cold run, else the warm one (scaled by
    /// `cold_run_factor` for a cold copy); `None` without either.
    pub fn for_copy(
        cold: bool,
        warm_run: Option<LearnedTime>,
        cold_run: Option<LearnedTime>,
    ) -> Option<Self> {
        match (cold, cold_run, warm_run) {
            (true, Some(cold_run), _) => Some(Self {
                learned: cold_run,
                cold_from_warm: false,
            }),
            (cold, _, Some(warm_run)) => Some(Self {
                learned: warm_run,
                cold_from_warm: cold,
            }),
            (_, _, None) => None,
        }
    }

    /// The whole run it stands for.
    pub fn run(&self, parameters: &Placement) -> Duration {
        let time = self.learned.time;
        if self.cold_from_warm {
            time.mul_f64(parameters.cold_run_factor)
        } else {
            time
        }
    }
}

#[derive(Serialize, Deserialize)]
struct HeldRecord {
    size: Cores,
    #[serde(rename = "remaining_ms", with = "millis::optional")]
    remaining: Option<Duration>,
    #[serde(
        rename = "elapsed_ms",
        default,
        with = "millis::optional",
        skip_serializing_if = "Option::is_none"
    )]
    elapsed: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    learned_run: Option<LearnedRun>,
}

impl From<HeldRecord> for Held {
    fn from(record: HeldRecord) -> Self {
        Held {
            size: record.size,
            elapsed: record.elapsed,
            expectation: match (record.remaining, record.learned_run) {
                (Some(remaining), _) => Expectation::Member { remaining },
                (None, Some(learned)) => Expectation::Learned(learned),
                (None, None) => Expectation::Unknown,
            },
        }
    }
}

impl From<Held> for HeldRecord {
    fn from(held: Held) -> Self {
        let (remaining, learned_run) = match held.expectation {
            Expectation::Member { remaining } => (Some(remaining), None),
            Expectation::Learned(learned) => (None, Some(learned)),
            Expectation::Unknown => (None, None),
        };
        HeldRecord {
            size: held.size,
            remaining,
            elapsed: held.elapsed,
            learned_run,
        }
    }
}

/// Where a running request's remaining time came from.
#[derive(Clone, Copy, Debug, PartialEq)]
enum RemainingFrom<'a> {
    Member,
    Learned(&'a LearnedRun),
    /// Past its expected run time (of this length, when history expected it):
    /// an estimate it has outlived says nothing more, so it is treated as work
    /// without one.
    Outlived(Option<Duration>),
    Unknown,
}

impl Held {
    /// How much longer it holds its cores: what is left of what its member
    /// expects; else of what history expects of its whole run; else, without any
    /// expected time, or once it has outlived the one it had,
    /// `unknown_remaining`, growing with the time it has run.
    pub fn holds_for(&self, parameters: &Placement) -> Duration {
        self.remaining_from(parameters).0
    }

    fn remaining_from(&self, parameters: &Placement) -> (Duration, RemainingFrom<'_>) {
        let unknown = || parameters.unknown_remaining_after(self.elapsed);
        match (&self.expectation, self.elapsed) {
            // A member's expectation already has the elapsed time taken out; at
            // zero with elapsed time recorded, the work has outlived it.
            (Expectation::Member { remaining }, Some(_)) if remaining.is_zero() => {
                (unknown(), RemainingFrom::Outlived(None))
            }
            (Expectation::Member { remaining }, _) => (*remaining, RemainingFrom::Member),
            (Expectation::Learned(learned), elapsed) => {
                let run = learned.run(parameters);
                let elapsed = elapsed.unwrap_or_default();
                if elapsed < run {
                    (run - elapsed, RemainingFrom::Learned(learned))
                } else {
                    (unknown(), RemainingFrom::Outlived(Some(run)))
                }
            }
            (Expectation::Unknown, _) => (unknown(), RemainingFrom::Unknown),
        }
    }
}

/// A request that starts on a member before this one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ahead {
    pub size: Cores,
    #[serde(rename = "run_ms", with = "millis::optional")]
    pub run: Option<Duration>,
}

/// The member's copy of the workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Copy {
    /// The calling member runs in the caller's own tree.
    Caller,
    Remote {
        warmth: Warmth,
        busy: bool,
    },
    /// The member cannot host this workspace.
    Unavailable,
}

/// A time learned from history, and where it came from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LearnedTime {
    #[serde(rename = "ms", with = "millis")]
    pub time: Duration,
    #[serde(flatten)]
    pub source: Source,
}

/// What history says about one candidate, resolved before scoring: its warm run,
/// its cold run when its copy is cold, and its sync when it is another member.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Learned {
    pub warm_run: Option<LearnedTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cold_run: Option<LearnedTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<LearnedTime>,
    /// How often another member lost runs placed on it, recently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub losses: Option<Losses>,
}

/// One member as placement sees it at decision time.
#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub state: MemberState,
    pub caller: bool,
    /// Average CPU utilization over the saturation window.
    pub utilization: Option<f64>,
    pub holding: Vec<Held>,
    pub ahead: Vec<Ahead>,
    pub copy: Copy,
    pub learned: Learned,
    /// The thresholds the member's own gate used, when it reported them. Scoring
    /// takes the gate as reported; replay re-derives it from these.
    pub admission: Option<Admission>,
}

/// What a candidate's record keeps beside its member state, so that scoring can
/// be repeated exactly: everything in its `Observation` that the state lacks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Inputs {
    pub caller: bool,
    /// The member's measured speed; `None` when it reported none.
    pub speed: Option<f64>,
    pub utilization: Option<f64>,
    pub holding: Vec<Held>,
    pub ahead: Vec<Ahead>,
    pub copy: Copy,
    pub learned: Learned,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<Admission>,
}

impl Observation {
    pub fn inputs(&self) -> Inputs {
        Inputs {
            caller: self.caller,
            speed: self.state.speed,
            utilization: self.utilization,
            holding: self.holding.clone(),
            ahead: self.ahead.clone(),
            copy: self.copy,
            learned: self.learned.clone(),
            admission: self.admission.clone(),
        }
    }

    /// The observation that `inputs` were recorded from, with the member's
    /// state at the time.
    pub fn from_inputs(mut state: MemberState, inputs: Inputs) -> Self {
        state.speed = inputs.speed;
        Self {
            state,
            caller: inputs.caller,
            utilization: inputs.utilization,
            holding: inputs.holding,
            ahead: inputs.ahead,
            copy: inputs.copy,
            learned: inputs.learned,
            admission: inputs.admission,
        }
    }

    /// The same observation with every duration in whole milliseconds.
    fn exact(mut self) -> Self {
        for held in &mut self.holding {
            held.elapsed = held.elapsed.map(whole_millis);
            match &mut held.expectation {
                Expectation::Member { remaining } => *remaining = whole_millis(*remaining),
                Expectation::Learned(learned) => {
                    learned.learned.time = whole_millis(learned.learned.time)
                }
                Expectation::Unknown => (),
            }
        }
        for ahead in &mut self.ahead {
            ahead.run = ahead.run.map(whole_millis);
        }
        for learned in [
            &mut self.learned.warm_run,
            &mut self.learned.cold_run,
            &mut self.learned.sync,
        ]
        .into_iter()
        .flatten()
        {
            learned.time = whole_millis(learned.time);
        }
        self
    }
}

/// A member placement could not ask.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Unreachable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_io: Option<crate::peer_diagnostics::Attempt>,
    pub member: String,
    pub why: String,
}

/// What is being placed.
pub struct Wanted<'a> {
    pub size: Cores,
    /// What is left of the hold limit.
    pub hold_left: Duration,
    pub parameters: &'a Placement,
}

/// Where an estimate came from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum Source {
    /// The member's own runs of the label.
    MemberRuns { samples: usize },
    /// Every member's runs of the label, converted to this member. Records from
    /// before learned speed ratios converted every run by measured speed and
    /// counted no conversions.
    FleetRuns {
        samples: usize,
        #[serde(default)]
        conversions: Conversions,
    },
    /// A line of run time against units through the observed plans of the label
    /// and its family, converted to this member, at the request's declared
    /// `units`.
    PlanRuns {
        samples: usize,
        units: u32,
        conversions: Conversions,
    },
    /// The runs of other labels of the label's family on the member, standing in
    /// for the runs of its own (`label_samples`) it lacks.
    FamilyRuns {
        samples: usize,
        label_samples: usize,
        /// How the label's own runs were converted, when they ran elsewhere.
        #[serde(default)]
        conversions: Conversions,
    },
    /// The family's runs on every member, converted to this member.
    FleetFamilyRuns {
        samples: usize,
        label_samples: usize,
        conversions: Conversions,
    },
    /// This workspace's warm syncs to the member.
    WorkspaceSyncs { samples: usize },
    /// Every workspace's warm syncs to the member.
    MemberSyncs { samples: usize },
    /// First syncs of the same repository.
    RepositorySyncs { samples: usize },
    /// No trusted history: the documented prior.
    Prior,
}

/// Which level of history a run estimate came from, in the order they are
/// tried: the label's own runs on the member, the label's runs on other members
/// converted, the observed plans of a declared plan's label and family, its
/// family's runs (on the member, else converted), or the prior.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Label,
    Converted,
    Plan,
    Family,
    Prior,
}

impl Source {
    /// The source of a run estimate learned from history.
    pub fn of(basis: learning::Basis) -> Self {
        match basis {
            learning::Basis::Label { samples } => Source::MemberRuns { samples },
            learning::Basis::Converted {
                samples,
                conversions,
            } => Source::FleetRuns {
                samples,
                conversions,
            },
            learning::Basis::Plan {
                samples,
                units,
                conversions,
            } => Source::PlanRuns {
                samples,
                units,
                conversions,
            },
            learning::Basis::Family {
                samples,
                label_samples,
                conversions,
            } => Source::FamilyRuns {
                samples,
                label_samples,
                conversions,
            },
            learning::Basis::ConvertedFamily {
                samples,
                label_samples,
                conversions,
            } => Source::FleetFamilyRuns {
                samples,
                label_samples,
                conversions,
            },
        }
    }

    /// The level of a run estimate from this source; `None` for sync sources.
    pub fn level(&self) -> Option<Level> {
        match self {
            Source::MemberRuns { .. } => Some(Level::Label),
            Source::FamilyRuns { .. } | Source::FleetFamilyRuns { .. } => Some(Level::Family),
            Source::FleetRuns { .. } => Some(Level::Converted),
            Source::PlanRuns { .. } => Some(Level::Plan),
            Source::Prior => Some(Level::Prior),
            Source::WorkspaceSyncs { .. }
            | Source::MemberSyncs { .. }
            | Source::RepositorySyncs { .. } => None,
        }
    }
}

/// A run estimate's source with its level beside it, as records keep it:
/// `{"from": "family_runs", "samples": 4, "label_samples": 1, "level": "family"}`.
/// Reading ignores the level, which follows from the source.
mod with_level {
    use super::{Level, Source};
    use serde::{Serialize, Serializer};

    pub fn serialize<S: Serializer>(source: &Source, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct WithLevel<'a> {
            #[serde(flatten)]
            source: &'a Source,
            #[serde(skip_serializing_if = "Option::is_none")]
            level: Option<Level>,
        }
        WithLevel {
            source,
            level: source.level(),
        }
        .serialize(serializer)
    }
}

/// When a candidate could start the request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "at", rename_all = "snake_case")]
pub enum Start {
    Now,
    /// Once running work ends, after the estimated wait.
    AfterWait,
    /// Not while this lasts; placement asks again.
    Blocked {
        why: String,
    },
    /// Larger than the member's capacity.
    Never {
        why: String,
    },
}

/// The copy's state as a candidate's estimate records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyState {
    Caller,
    Warm,
    ColdWithHistory,
    Cold,
}

/// The components of one candidate's expected completion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    pub speed: f64,
    /// False when the member reported no speed and the fleet's median was assumed.
    pub speed_measured: bool,
    pub copy: CopyState,
    pub copy_busy: bool,
    /// The share of the machine busy with work Elsecall did not start.
    pub outside_utilization: Option<f64>,
    pub start: Start,
    #[serde(with = "seconds::optional")]
    pub wait_seconds: Option<Duration>,
    #[serde(with = "seconds")]
    pub sync_seconds: Duration,
    pub sync_from: Option<Source>,
    #[serde(with = "seconds")]
    pub run_seconds: Duration,
    #[serde(serialize_with = "with_level::serialize")]
    pub run_from: Source,
    /// The cold-run factor applied to a warm estimate, when one was.
    pub cold_factor: Option<f64>,
    pub slowdown: f64,
    /// How much longer sync and run are expected to take for the member's lost
    /// runs, when it lost any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss_factor: Option<f64>,
    #[serde(with = "seconds::optional")]
    pub completion_seconds: Option<Duration>,
    pub eligible: bool,
}

/// One candidate with its estimate and the sentence `explain` shows.
#[derive(Clone, Debug, PartialEq)]
pub struct Scored {
    pub observation: Observation,
    pub estimate: Estimate,
    pub why: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    pub candidates: Vec<Scored>,
    pub unreachable: Vec<Unreachable>,
    /// The winning member, if any member can take the request in time.
    pub winner: Option<String>,
    pub reason: String,
}
impl Verdict {
    pub fn scored(&self, member: &str) -> Option<&Scored> {
        self.candidates
            .iter()
            .find(|c| c.observation.state.member == member)
    }
}

/// When a request of `size` can start, given the free capacity, running work and
/// the requests ahead of it; `None` if it never fits. Running work ends as
/// [`Held::holds_for`] says; queued work without an expected time is assumed to
/// take `unknown_remaining`.
pub fn wait(
    capacity: Cores,
    free: Cores,
    size: Cores,
    held: &[Held],
    ahead: &[Ahead],
    parameters: &Placement,
) -> Option<Duration> {
    if size > capacity {
        return None;
    }
    let unknown = parameters.unknown_remaining;
    let mut ends: Vec<(Duration, Cores)> = held
        .iter()
        .map(|h| (h.holds_for(parameters), h.size))
        .collect();
    let mut free = free;
    let mut now = Duration::ZERO;
    let mut start = |size: Cores, ends: &mut Vec<(Duration, Cores)>, free: &mut Cores| {
        while *free < size {
            ends.sort();
            if ends.is_empty() {
                // Nothing holds the capacity it lacks: it was never there.
                return None;
            }
            let (end, released) = ends.remove(0);
            now = now.max(end);
            *free = free.saturating_add(released);
        }
        *free = free.saturating_sub(size);
        Some(now)
    };
    for request in ahead.iter().filter(|a| a.size <= capacity) {
        let at = start(request.size, &mut ends, &mut free)?;
        ends.push((at + request.run.unwrap_or(unknown), request.size));
    }
    start(size, &mut ends, &mut free)
}

/// How much slower the run goes when outside load and Elsecall's work leave
/// fewer idle weighted cores than its size, at most `maximum` times.
pub fn slowdown(observation: &Observation, size: Cores, starts_now: bool, maximum: f64) -> f64 {
    let capacity = observation.state.capacity;
    let weighted = capacity.weighted_cores.as_f64();
    let in_use = observation.state.in_use.as_f64();
    let outside = observation
        .utilization
        .map_or(0.0, |u| (u * weighted - in_use).max(0.0));
    let ours = if starts_now {
        in_use
    } else {
        (capacity.capacity.as_f64() - size.as_f64()).max(0.0)
    };
    let room = (weighted - outside - ours).max(0.25);
    (size.as_f64() / room).clamp(1.0, maximum)
}

fn outside_utilization(observation: &Observation) -> Option<f64> {
    let weighted = observation.state.capacity.weighted_cores.as_f64();
    let utilization = observation.utilization?;
    (weighted > 0.0).then(|| (utilization - observation.state.in_use.as_f64() / weighted).max(0.0))
}

fn seconds_text(duration: Duration) -> String {
    let s = duration.as_secs_f64();
    if s < 10.0 {
        format!("{s:.1} s")
    } else {
        format!("{s:.0} s")
    }
}

/// The running work a wait is behind whose remaining time is not its member's
/// expectation: what history expects of it, the assumption for work without an
/// expected time, or for work that has outlived its own, with the time each has
/// run. Empty for records from before that time was recorded, whose text stays
/// as it was.
fn behind_text(holding: &[Held], parameters: &Placement) -> String {
    const LISTED: usize = 3;
    let described: Vec<String> = holding
        .iter()
        .filter_map(|held| {
            let elapsed = held.elapsed?;
            let (remaining, from) = held.remaining_from(parameters);
            let basis = match from {
                RemainingFrom::Member => return None,
                RemainingFrom::Learned(learned) => format!(
                    "{}{}",
                    source_text(&learned.learned.source),
                    if learned.cold_from_warm {
                        format!(" × {} cold", parameters.cold_run_factor)
                    } else {
                        String::new()
                    }
                ),
                RemainingFrom::Outlived(Some(run)) => {
                    format!("past its {} estimate", seconds_text(run))
                }
                RemainingFrom::Outlived(None) => "past its expected run time".to_owned(),
                RemainingFrom::Unknown => "no estimate".to_owned(),
            };
            Some(format!(
                "{} {} {} in, {} more ({basis})",
                held.size,
                if held.size == Cores::ONE {
                    "core"
                } else {
                    "cores"
                },
                seconds_text(elapsed),
                seconds_text(remaining)
            ))
        })
        .collect();
    if described.is_empty() {
        return String::new();
    }
    let more = described.len().saturating_sub(LISTED);
    let listed: Vec<String> = described.into_iter().take(LISTED).collect();
    format!(
        " behind running work: {}{}",
        listed.join(", "),
        if more > 0 {
            format!(", and {more} more")
        } else {
            String::new()
        }
    )
}

fn source_text(source: &Source) -> String {
    match source {
        Source::MemberRuns { samples } => format!("learned from {samples} runs there"),
        Source::FleetRuns {
            samples,
            conversions,
        } => format!(
            "learned from {samples} runs, {}",
            conversions_text(conversions)
        ),
        Source::PlanRuns {
            samples,
            units,
            conversions,
        } => format!(
            "plan of {units} units, from {samples} observed plans of this label and family{}",
            match conversions.learned + conversions.measured {
                0 => String::new(),
                elsewhere => format!(
                    " ({elsewhere} from other members, {})",
                    conversions_text(conversions)
                ),
            }
        ),
        Source::FamilyRuns {
            samples,
            label_samples,
            conversions,
        } => format!(
            "family: {samples} runs of other labels there{}{}",
            own_text(*label_samples),
            match conversions.learned + conversions.measured {
                0 => String::new(),
                elsewhere => format!(
                    " ({elsewhere} from other members, {})",
                    conversions_text(conversions)
                ),
            }
        ),
        Source::FleetFamilyRuns {
            samples,
            label_samples,
            conversions,
        } => format!(
            "family: {samples} runs of other labels, {}{}",
            conversions_text(conversions),
            own_text(*label_samples)
        ),
        Source::WorkspaceSyncs { samples } => {
            format!("learned from {samples} syncs of this workspace")
        }
        Source::MemberSyncs { samples } => format!("learned from {samples} syncs there"),
        Source::RepositorySyncs { samples } => {
            format!("learned from {samples} first syncs of this repository")
        }
        Source::Prior => "prior".into(),
    }
}

fn conversions_text(conversions: &Conversions) -> String {
    match (conversions.learned, conversions.measured) {
        (0, 0) => "converted by speed".into(),
        (_, 0) => "converted by learned speed ratios".into(),
        (0, _) => "converted by measured speed".into(),
        (learned, measured) => {
            format!("{learned} converted by learned speed ratios, {measured} by measured speed")
        }
    }
}

fn own_text(label_samples: usize) -> String {
    match label_samples {
        0 => String::new(),
        1 => ", with 1 of this label".into(),
        n => format!(", with {n} of this label"),
    }
}

/// The speed assumed for a member that reported none: the median of the
/// measured ones, or 1.
fn typical_speed(observations: &[Observation]) -> f64 {
    let mut measured: Vec<f64> = observations.iter().filter_map(|o| o.state.speed).collect();
    measured.sort_by(f64::total_cmp);
    measured.get(measured.len() / 2).copied().unwrap_or(1.0)
}

/// Fills in what history says about each candidate: runs of the label on it,
/// warm or cold as its copy is, syncs of `workspace` to it, and the runs placed
/// on another member that it lost.
pub fn learn(
    observations: &mut [Observation],
    workspace: &WorkspaceRef,
    runs: &RunHistory,
    syncs: &SyncHistory,
    losses: &LossHistory,
) {
    let speeds: Vec<(String, f64)> = observations
        .iter()
        .filter_map(|o| Some((o.state.member.clone(), o.state.speed?)))
        .collect();
    let speed_of = |member: &str| speeds.iter().find(|(m, _)| m == member).map(|(_, s)| *s);
    let run = |estimate: learning::RunEstimate| LearnedTime {
        time: estimate.run,
        source: Source::of(estimate.basis),
    };
    for observation in observations.iter_mut() {
        let member = observation.state.member.clone();
        let (cold, sync) = match observation.copy {
            Copy::Caller => (false, None),
            // Scored as a cold copy, though it never starts there.
            Copy::Unavailable => (true, None),
            Copy::Remote { warmth, .. } => (
                warmth != Warmth::Warm,
                syncs
                    .sync_to(&member, workspace, warmth)
                    .map(|estimate| LearnedTime {
                        time: estimate.sync,
                        source: match estimate.basis {
                            SyncBasis::Workspace { samples } => Source::WorkspaceSyncs { samples },
                            SyncBasis::Member { samples } => Source::MemberSyncs { samples },
                            SyncBasis::Repository { samples } => {
                                Source::RepositorySyncs { samples }
                            }
                        },
                    }),
            ),
        };
        let estimates = runs.estimates_on(&member, cold, &speed_of);
        observation.learned = Learned {
            warm_run: estimates.warm.map(run),
            cold_run: estimates.cold.map(run),
            sync,
            losses: (!observation.caller).then(|| losses.of(&member)).flatten(),
        };
    }
}

/// Scores every candidate and picks the winner.
pub fn decide(
    observations: Vec<Observation>,
    unreachable: Vec<Unreachable>,
    wanted: &Wanted,
) -> Verdict {
    let observations: Vec<Observation> = observations.into_iter().map(Observation::exact).collect();
    let typical = typical_speed(&observations);
    let candidates: Vec<Scored> = observations
        .into_iter()
        .map(|observation| score(observation, wanted, typical))
        .collect();
    let contenders: Vec<Contender> = candidates
        .iter()
        .map(|c| Contender {
            member: &c.observation.state.member,
            caller: c.observation.caller,
            eligible: c.estimate.eligible,
            completion: c.estimate.completion_seconds,
            copy: c.estimate.copy,
        })
        .collect();
    let (winner, reason) = pick(&contenders, wanted.hold_left, wanted.parameters);
    Verdict {
        candidates,
        unreachable,
        winner,
        reason,
    }
}

/// A scored candidate as picking the winner sees it.
#[derive(Clone, Debug)]
pub struct Contender<'a> {
    pub member: &'a str,
    pub caller: bool,
    pub eligible: bool,
    pub completion: Option<Duration>,
    pub copy: CopyState,
}

/// The winner among scored candidates and why it won: the eligible one that
/// completes earliest, where the caller keeps the work unless another member
/// completes earlier by the keep-local margin.
pub fn pick(
    contenders: &[Contender],
    hold_left: Duration,
    parameters: &Placement,
) -> (Option<String>, String) {
    let best = |caller: bool| {
        contenders
            .iter()
            .filter(|c| c.caller == caller && c.eligible)
            .min_by(|a, b| {
                (a.completion, warmth_rank(a.copy), a.member).cmp(&(
                    b.completion,
                    warmth_rank(b.copy),
                    b.member,
                ))
            })
    };
    let caller = best(true);
    let other = best(false);
    let completion = |c: &Contender| c.completion.unwrap_or(Duration::MAX);
    let (winner, reason) = match (caller, other) {
        (None, None) => (
            None,
            format!(
                "No member can start it within the {} left of its hold limit.",
                seconds_text(hold_left)
            ),
        ),
        (Some(caller), None) => (
            Some(caller),
            format!(
                "Only the calling member, {}, can start it in time; it completes in {}.",
                caller.member,
                seconds_text(completion(caller))
            ),
        ),
        (None, Some(other)) => (
            Some(other),
            format!(
                "{} completes earliest, in {}; the calling member cannot start it in time.",
                other.member,
                seconds_text(completion(other))
            ),
        ),
        (Some(caller), Some(other)) => {
            let margin = parameters.keep_local_margin(completion(caller));
            if completion(other) + margin <= completion(caller) {
                (
                    Some(other),
                    format!(
                        "{} completes earliest, in {}, against {} on the calling member {}.",
                        other.member,
                        seconds_text(completion(other)),
                        seconds_text(completion(caller)),
                        caller.member
                    ),
                )
            } else {
                (
                    Some(caller),
                    format!(
                        "The calling member {} completes in {}; {} would take {}, not earlier by the {} margin for leaving the caller.",
                        caller.member,
                        seconds_text(completion(caller)),
                        other.member,
                        seconds_text(completion(other)),
                        seconds_text(margin)
                    ),
                )
            }
        }
    };
    (winner.map(|w| w.member.to_owned()), reason)
}

/// Among other members, warm copies win ties.
pub fn warmth_rank(copy: CopyState) -> u8 {
    match copy {
        CopyState::Caller | CopyState::Warm => 0,
        CopyState::ColdWithHistory => 1,
        CopyState::Cold => 2,
    }
}

fn score(observation: Observation, wanted: &Wanted, typical: f64) -> Scored {
    let parameters = wanted.parameters;
    let state = &observation.state;
    let speed = state.speed.unwrap_or(typical);
    let (copy, busy) = match observation.copy {
        Copy::Caller => (CopyState::Caller, false),
        Copy::Unavailable => (CopyState::Cold, false),
        Copy::Remote { warmth, busy } => (
            match warmth {
                Warmth::Warm => CopyState::Warm,
                Warmth::ColdWithHistory => CopyState::ColdWithHistory,
                Warmth::Cold => CopyState::Cold,
            },
            busy,
        ),
    };
    let cold = matches!(copy, CopyState::Cold | CopyState::ColdWithHistory);
    // Run time, learned or prior, warm or cold.
    let learned = &observation.learned;
    let warm_run = learned.warm_run.clone().unwrap_or_else(|| LearnedTime {
        time: parameters.run_prior.div_f64(speed),
        source: Source::Prior,
    });
    let chosen = LearnedRun::for_copy(cold, Some(warm_run), learned.cold_run.clone())
        .expect("a warm run is always given");
    let run = chosen.run(parameters);
    let cold_factor = chosen.cold_from_warm.then_some(parameters.cold_run_factor);
    let run_from = chosen.learned.source;
    // Sync.
    let (sync, sync_from) = match observation.copy {
        Copy::Caller | Copy::Unavailable => (Duration::ZERO, None),
        Copy::Remote { warmth, .. } => match &learned.sync {
            Some(learned) => (learned.time, Some(learned.source.clone())),
            None => (
                match warmth {
                    Warmth::Warm => parameters.sync_prior_warm,
                    Warmth::ColdWithHistory => parameters.sync_prior_cold_with_history,
                    Warmth::Cold => parameters.sync_prior_cold,
                },
                Some(Source::Prior),
            ),
        },
    };
    // When it can start.
    let capacity = state.capacity.capacity;
    let start = if observation.copy == Copy::Unavailable {
        Start::Never {
            why: "It cannot host this workspace.".into(),
        }
    } else if wanted.size > capacity {
        Start::Never {
            why: format!(
                "Size {} is larger than its capacity of {} cores.",
                wanted.size, capacity
            ),
        }
    } else {
        match state.gate {
            Gate::Saturated {
                outside_utilization,
                ..
            } => Start::Blocked {
                why: format!(
                    "Saturated by outside load ({:.0}% CPU).",
                    outside_utilization * 100.0
                ),
            },
            Gate::MemoryPressure { .. } => Start::Blocked {
                why: "Under memory pressure.".into(),
            },
            Gate::Open | Gate::Unknown if busy => Start::Blocked {
                why: "Another run is using its copy of this workspace.".into(),
            },
            Gate::Open | Gate::Unknown => {
                if observation.ahead.is_empty() && wanted.size <= state.free {
                    Start::Now
                } else {
                    Start::AfterWait
                }
            }
        }
    };
    let wait_time = match start {
        Start::Now => Some(Duration::ZERO),
        Start::AfterWait => wait(
            capacity,
            state.free,
            wanted.size,
            &observation.holding,
            &observation.ahead,
            parameters,
        ),
        Start::Blocked { .. } | Start::Never { .. } => None,
    };
    let slowdown = slowdown(
        &observation,
        wanted.size,
        start == Start::Now,
        parameters.maximum_slowdown,
    );
    let run = run.mul_f64(slowdown);
    let losses = learned.losses.filter(|_| !observation.caller);
    let work = sync + run;
    let exposed = Losses::exposed(
        work,
        losses.map_or(0.0, |l| l.per_second(parameters.minimum_loss_exposure)),
    );
    let loss_factor = (exposed > work).then(|| exposed.as_secs_f64() / work.as_secs_f64());
    let completion = wait_time.map(|w| w + exposed);
    let eligible = wait_time.is_some_and(|w| w + sync <= wanted.hold_left);
    let why = match (&start, wait_time) {
        (Start::Blocked { why } | Start::Never { why }, _) => why.clone(),
        (_, Some(w)) => {
            let mut parts = Vec::new();
            if !w.is_zero() {
                parts.push(format!(
                    "wait {}{}",
                    seconds_text(w),
                    behind_text(&observation.holding, parameters)
                ));
            }
            if let Some(from) = &sync_from {
                parts.push(format!(
                    "{} sync {} ({})",
                    match copy {
                        CopyState::Warm => "warm",
                        CopyState::ColdWithHistory => "first, with history,",
                        _ => "first",
                    },
                    seconds_text(sync),
                    source_text(from)
                ));
            }
            parts.push(format!(
                "{}run {} ({}{}{})",
                if cold { "cold " } else { "" },
                seconds_text(run),
                source_text(&run_from),
                cold_factor.map_or(String::new(), |f| format!(" × {f} cold")),
                if slowdown > 1.0 {
                    format!(", {slowdown:.1}× slower under load")
                } else {
                    String::new()
                }
            ));
            let lost = match (loss_factor, losses) {
                (Some(factor), Some(losses)) => format!(
                    ", sync and run {factor:.2}× for {} lost of its placed runs in {:.1} h of them",
                    losses.lost,
                    losses.run.as_secs_f64() / 3600.0
                ),
                _ => String::new(),
            };
            let total = completion.unwrap_or_default();
            if eligible {
                format!(
                    "{} in {}: {}{lost}.",
                    if start == Start::Now {
                        "Starts now, completes"
                    } else {
                        "Completes"
                    },
                    seconds_text(total),
                    parts.join(" + ")
                )
            } else {
                format!(
                    "Cannot start within the {} left of the hold limit: {}{lost}.",
                    seconds_text(wanted.hold_left),
                    parts.join(" + ")
                )
            }
        }
        (_, None) => "Its running work never frees enough capacity.".into(),
    };
    let estimate = Estimate {
        speed,
        speed_measured: state.speed.is_some(),
        copy,
        copy_busy: busy,
        outside_utilization: outside_utilization(&observation),
        start,
        wait_seconds: wait_time,
        sync_seconds: sync,
        sync_from,
        run_seconds: run,
        run_from,
        cold_factor,
        slowdown,
        loss_factor,
        completion_seconds: completion,
        eligible,
    };
    Scored {
        observation,
        estimate,
        why,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        inspection::{CoreCounts, LoadSample, MemoryPressure, ThermalState},
        scheduling::{
            admission::QueueCounts, capacity::Capacity, learning::RunSample,
            parameters::Parameters, sync_history::SyncSample, timestamp::Timestamp,
        },
    };

    fn cores(n: f64) -> Cores {
        Cores::from_f64(n).unwrap()
    }
    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }
    /// Running work as records from before elapsed time kept it.
    fn held(size: Cores, remaining: Option<Duration>) -> Held {
        Held {
            size,
            elapsed: None,
            expectation: remaining.map_or(Expectation::Unknown, |remaining| Expectation::Member {
                remaining,
            }),
        }
    }
    fn member(name: &str, caller: bool, free: f64, speed: f64, copy: Copy) -> Observation {
        let capacity = Capacity::of(
            CoreCounts {
                performance: 10,
                efficiency: 0,
            },
            Cores::ZERO,
        );
        Observation {
            state: MemberState {
                member: name.into(),
                capacity,
                in_use: capacity.capacity.saturating_sub(cores(free)),
                free: cores(free),
                queued: QueueCounts::default(),
                load: Some(LoadSample {
                    cores: CoreCounts {
                        performance: 10,
                        efficiency: 0,
                    },
                    cpu_utilization: Some(0.0),
                    memory_pressure: MemoryPressure::Normal,
                    thermal_state: ThermalState::Nominal,
                }),
                gate: Gate::Open,
                speed: Some(speed),
            },
            caller,
            utilization: Some((10.0 - free) / 10.0),
            holding: if free < 10.0 {
                vec![held(cores(10.0 - free), Some(secs(100)))]
            } else {
                Vec::new()
            },
            ahead: Vec::new(),
            copy,
            learned: Learned::default(),
            admission: None,
        }
    }
    fn defaults() -> Placement {
        Parameters::default().placement
    }
    fn warm() -> Copy {
        Copy::Remote {
            warmth: Warmth::Warm,
            busy: false,
        }
    }
    fn cold() -> Copy {
        Copy::Remote {
            warmth: Warmth::Cold,
            busy: false,
        }
    }
    fn workspace() -> WorkspaceRef {
        WorkspaceRef {
            root: "/w/app".into(),
            common_dir: "/w/app/.git".into(),
        }
    }
    fn decide_with(
        mut observations: Vec<Observation>,
        size: f64,
        runs: &RunHistory,
        syncs: &SyncHistory,
    ) -> Verdict {
        learn(
            &mut observations,
            &workspace(),
            runs,
            syncs,
            &LossHistory::default(),
        );
        decide(
            observations,
            Vec::new(),
            &Wanted {
                size: cores(size),
                hold_left: secs(600),
                parameters: &defaults(),
            },
        )
    }
    fn none() -> (RunHistory, SyncHistory) {
        (RunHistory::default(), SyncHistory::default())
    }
    /// The history of the label "build", without a family.
    fn label_history(
        runs: Vec<RunSample>,
        trust: &crate::scheduling::parameters::Estimates,
    ) -> RunHistory {
        RunHistory::new(
            "build",
            None,
            runs,
            Vec::new(),
            learning::ratios::SpeedRatios::default(),
            trust,
        )
    }

    #[test]
    fn a_run_source_is_recorded_with_its_level_and_reads_back() {
        let conversions = Conversions {
            learned: 2,
            measured: 1,
        };
        for (source, level) in [
            (Source::MemberRuns { samples: 3 }, "label"),
            (
                Source::FamilyRuns {
                    samples: 4,
                    label_samples: 1,
                    conversions,
                },
                "family",
            ),
            (
                Source::FleetRuns {
                    samples: 3,
                    conversions,
                },
                "converted",
            ),
            (
                Source::FleetFamilyRuns {
                    samples: 5,
                    label_samples: 2,
                    conversions,
                },
                "family",
            ),
            (Source::Prior, "prior"),
        ] {
            let mut estimate: Estimate = serde_json::from_value(serde_json::json!({
                "speed": 1.0, "speed_measured": true, "copy": "caller", "copy_busy": false,
                "outside_utilization": null, "start": {"at": "now"}, "wait_seconds": 0.0,
                "sync_seconds": 0.0, "sync_from": null, "run_seconds": 1.0,
                "run_from": {"from": "prior"}, "cold_factor": null, "slowdown": 1.0,
                "completion_seconds": 1.0, "eligible": true
            }))
            .unwrap();
            estimate.run_from = source.clone();
            let json = serde_json::to_value(&estimate).unwrap();
            assert_eq!(json["run_from"]["level"], level, "{json}");
            let back: Estimate = serde_json::from_value(json).unwrap();
            assert_eq!(back, estimate);
        }
        // Records from before conversions were counted read as none.
        let old: Source = serde_json::from_str(r#"{"from": "fleet_runs", "samples": 3}"#).unwrap();
        assert_eq!(
            old,
            Source::FleetRuns {
                samples: 3,
                conversions: Conversions::default()
            }
        );
        assert_eq!(old.level(), Some(Level::Converted));
        assert_eq!(Source::WorkspaceSyncs { samples: 2 }.level(), None);
    }

    #[test]
    fn waits_for_running_work_and_requests_ahead_in_order() {
        let held = [
            held(cores(2.0), Some(secs(10))),
            held(cores(4.0), Some(secs(30))),
            held(cores(2.0), None),
        ];
        let ten = cores(10.0);
        let parameters = defaults();
        let unknown = parameters.unknown_remaining;
        assert_eq!(
            wait(ten, cores(2.0), cores(2.0), &held, &[], &parameters),
            Some(secs(0))
        );
        assert_eq!(
            wait(ten, cores(2.0), cores(4.0), &held, &[], &parameters),
            Some(secs(10))
        );
        assert_eq!(
            wait(ten, cores(2.0), cores(8.0), &held, &[], &parameters),
            Some(secs(30))
        );
        assert_eq!(
            wait(ten, cores(2.0), cores(10.0), &held, &[], &parameters),
            Some(unknown)
        );
        let ahead = [Ahead {
            size: cores(4.0),
            run: Some(secs(50)),
        }];
        assert_eq!(
            wait(ten, cores(2.0), cores(4.0), &held, &ahead, &parameters),
            Some(secs(30)),
            "the request ahead takes the first room"
        );
        assert_eq!(
            wait(ten, cores(2.0), cores(11.0), &held, &[], &parameters),
            None
        );
        assert_eq!(
            wait(ten, cores(0.0), cores(1.0), &[], &[], &parameters),
            None,
            "capacity nothing holds and nothing frees"
        );
    }

    #[test]
    fn running_work_holds_its_cores_by_its_expectation_then_history_then_elapsed_time() {
        let parameters = defaults();
        let family = LearnedRun {
            learned: LearnedTime {
                time: secs(300),
                source: Source::FamilyRuns {
                    samples: 3,
                    label_samples: 0,
                    conversions: Conversions::default(),
                },
            },
            cold_from_warm: false,
        };
        let running = |elapsed, expectation| Held {
            size: cores(9.0),
            elapsed,
            expectation,
        };
        let member = |s| Expectation::Member { remaining: secs(s) };
        // Its member's expectation, as before.
        assert_eq!(
            running(Some(secs(644)), member(10)).holds_for(&parameters),
            secs(10)
        );
        // Else what history expects of its whole run, less the time it has run…
        let learned = running(Some(secs(100)), Expectation::Learned(family.clone()));
        assert_eq!(learned.holds_for(&parameters), secs(200));
        // …times the cold factor when its copy was cold and only warm runs were
        // learned, as the parameters say at scoring.
        let cold = running(
            Some(secs(100)),
            Expectation::Learned(LearnedRun {
                cold_from_warm: true,
                ..family.clone()
            }),
        );
        assert_eq!(cold.holds_for(&parameters), secs(800));
        let hotter = Placement {
            cold_run_factor: 2.0,
            ..parameters.clone()
        };
        assert_eq!(cold.holds_for(&hotter), secs(500));
        // Without any expectation, the assumption grows with the time it has run:
        // the real fleet's 9-core run 644 s in holds for 322 s, not 60 s.
        let unknown = running(Some(secs(644)), Expectation::Unknown);
        assert_eq!(unknown.holds_for(&parameters), secs(322));
        assert_eq!(
            running(Some(secs(30)), Expectation::Unknown).holds_for(&parameters),
            secs(60)
        );
        // Work that outlived its estimate, learned or its member's, is work
        // without one: not ending now, which would bring the wait to nothing.
        let outlived = running(Some(secs(644)), Expectation::Learned(family));
        assert_eq!(outlived.holds_for(&parameters), secs(322));
        assert_eq!(
            running(Some(secs(644)), member(0)).holds_for(&parameters),
            secs(322)
        );
        // Records from before elapsed time was recorded score as they did.
        assert_eq!(
            running(None, Expectation::Unknown).holds_for(&parameters),
            secs(60)
        );
        assert_eq!(
            running(None, member(0)).holds_for(&parameters),
            Duration::ZERO
        );
    }

    #[test]
    fn a_learned_run_for_a_cold_copy_is_chosen_as_new_works_is() {
        let time = |s, samples| LearnedTime {
            time: secs(s),
            source: Source::MemberRuns { samples },
        };
        let chosen = |cold, warm, cold_run| {
            LearnedRun::for_copy(cold, warm, cold_run).map(|r| (r.learned.time, r.cold_from_warm))
        };
        assert_eq!(
            chosen(false, Some(time(10, 3)), Some(time(30, 3))),
            Some((secs(10), false))
        );
        assert_eq!(
            chosen(true, Some(time(10, 3)), Some(time(30, 3))),
            Some((secs(30), false))
        );
        assert_eq!(
            chosen(true, Some(time(10, 3)), None),
            Some((secs(10), true))
        );
        assert_eq!(
            chosen(true, None, Some(time(30, 3))),
            Some((secs(30), false))
        );
        assert_eq!(chosen(false, None, Some(time(30, 3))), None);
        // Records keep it, and read it back, as before plus the new fields.
        let held = Held {
            size: cores(2.0),
            elapsed: Some(secs(5)),
            expectation: Expectation::Learned(LearnedRun {
                learned: time(10, 3),
                cold_from_warm: true,
            }),
        };
        let json = serde_json::to_value(&held).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"size": 2.0, "remaining_ms": null, "elapsed_ms": 5000,
                "learned_run": {"ms": 10000, "from": "member_runs", "samples": 3,
                                "cold_from_warm": true}})
        );
        assert_eq!(serde_json::from_value::<Held>(json).unwrap(), held);
        let earlier: Held =
            serde_json::from_value(serde_json::json!({"size": 9.0, "remaining_ms": null})).unwrap();
        assert_eq!(earlier.expectation, Expectation::Unknown);
        assert_eq!(earlier.elapsed, None);
    }

    #[test]
    fn a_wait_behind_work_without_an_estimate_grows_and_says_why() {
        let parameters = defaults();
        let mut caller = member("mini", true, 0.0, 1.0, Copy::Caller);
        let devbox = member("devbox", false, 10.0, 1.0, warm());
        let wanted = |hold_left| Wanted {
            size: cores(3.0),
            hold_left,
            parameters: &parameters,
        };
        let scored = |caller: &Observation| {
            decide(
                vec![caller.clone(), devbox.clone()],
                Vec::new(),
                &wanted(secs(530)),
            )
        };
        // Just started: the assumption's floor, 60 s, named in the reason.
        caller.holding = vec![Held {
            size: cores(10.0),
            elapsed: Some(secs(10)),
            expectation: Expectation::Unknown,
        }];
        let early = scored(&caller);
        let mine = early.scored("mini").unwrap();
        assert_eq!(mine.estimate.wait_seconds, Some(secs(60)));
        assert!(
            mine.why.contains(
                "wait 60 s behind running work: 10 cores 10 s in, 60 s more (no estimate)"
            ),
            "{}",
            mine.why
        );
        // 644 s in: 322 s, and the idle devbox completes first by far.
        caller.holding[0].elapsed = Some(secs(644));
        let late = scored(&caller);
        assert_eq!(
            late.scored("mini").unwrap().estimate.wait_seconds,
            Some(secs(322))
        );
        assert_eq!(late.winner.as_deref(), Some("devbox"));
        // A record without elapsed time scores and reads as before.
        caller.holding[0].elapsed = None;
        let before = scored(&caller);
        let mine = before.scored("mini").unwrap();
        assert_eq!(mine.estimate.wait_seconds, Some(secs(60)));
        assert!(mine.why.contains("wait 60 s + run"), "{}", mine.why);
    }

    #[test]
    fn contention_slows_the_run_down() {
        let mut busy = member("m", true, 10.0, 1.0, Copy::Caller);
        assert_eq!(slowdown(&busy, cores(4.0), true, 4.0), 1.0);
        busy.utilization = Some(0.8);
        assert_eq!(
            slowdown(&busy, cores(4.0), true, 4.0),
            2.0,
            "two idle cores for four"
        );
        busy.utilization = Some(1.0);
        assert_eq!(slowdown(&busy, cores(4.0), true, 4.0), 4.0);
        assert_eq!(slowdown(&busy, cores(4.0), true, 3.0), 3.0);
    }

    #[test]
    fn a_new_workspace_stays_on_an_idle_caller_even_beside_a_faster_member() {
        let (runs, syncs) = none();
        let verdict = decide_with(
            vec![
                member("mini", true, 10.0, 1.0, Copy::Caller),
                member("devbox", false, 10.0, 2.0, cold()),
            ],
            2.0,
            &runs,
            &syncs,
        );
        assert_eq!(verdict.winner.as_deref(), Some("mini"), "{verdict:#?}");
        let devbox = verdict.scored("devbox").unwrap();
        assert_eq!(devbox.estimate.sync_seconds, defaults().sync_prior_cold);
        assert_eq!(
            devbox.estimate.cold_factor,
            Some(defaults().cold_run_factor)
        );
        assert_eq!(devbox.estimate.run_seconds, secs(90));
    }

    #[test]
    fn a_member_that_loses_runs_keeps_short_work_and_loses_long_work() {
        // The devbox is twice as fast and warm; it lost 20 runs in 10 hours of
        // placed work, two an hour.
        let losses = Losses {
            lost: 20,
            run: secs(10 * 3600),
        };
        let score_with = |run_prior: Duration, caller_losses: Option<Losses>| {
            let mut observations = vec![
                member("mini", true, 10.0, 1.0, Copy::Caller),
                member("devbox", false, 10.0, 2.0, warm()),
            ];
            observations[0].learned.losses = caller_losses;
            observations[1].learned.losses = Some(losses);
            let parameters = Placement {
                run_prior,
                ..defaults()
            };
            decide(
                observations,
                Vec::new(),
                &Wanted {
                    size: cores(2.0),
                    hold_left: secs(600),
                    parameters: &parameters,
                },
            )
        };
        // A minute of work: 33 s there, 0.9% longer for the losses.
        let short = score_with(secs(60), None);
        assert_eq!(short.winner.as_deref(), Some("devbox"), "{}", short.reason);
        let devbox = short.scored("devbox").unwrap();
        let factor = devbox.estimate.loss_factor.unwrap();
        assert!((factor - 1.0092).abs() < 0.0001, "{factor}");
        assert!(
            devbox
                .why
                .ends_with("sync and run 1.01× for 20 lost of its placed runs in 10.0 h of them."),
            "{}",
            devbox.why
        );
        // Two hours of work: one hour there, but 3.2 hours with its losses,
        // so the slower caller completes first.
        let long = score_with(secs(2 * 3600), None);
        assert_eq!(long.winner.as_deref(), Some("mini"), "{}", long.reason);
        let devbox = long.scored("devbox").unwrap().estimate.clone();
        let expected = (2.0f64 * 3603.0 / 3600.0).exp_m1() / 2.0 * 3600.0;
        let completion = devbox.completion_seconds.unwrap().as_secs_f64();
        assert!(
            (completion - expected).abs() < 0.01,
            "{completion} {expected}"
        );
        assert_eq!(
            devbox.run_seconds,
            secs(3600),
            "the run itself is unchanged"
        );
        // The calling member's own runs are never lost to a connection.
        let caller = score_with(secs(60), Some(losses));
        let mini = caller.scored("mini").unwrap();
        assert_eq!(mini.estimate.loss_factor, None);
        assert_eq!(mini.estimate.completion_seconds, Some(secs(60)));
    }

    #[test]
    fn a_warm_faster_member_wins_and_a_saturated_one_never_does() {
        let (runs, syncs) = none();
        let verdict = decide_with(
            vec![
                member("mini", true, 10.0, 1.0, Copy::Caller),
                member("devbox", false, 10.0, 2.0, warm()),
            ],
            2.0,
            &runs,
            &syncs,
        );
        assert_eq!(
            verdict.winner.as_deref(),
            Some("devbox"),
            "{}",
            verdict.reason
        );
        let devbox = verdict.scored("devbox").unwrap();
        assert_eq!(devbox.estimate.completion_seconds, Some(secs(33)));
        assert!(
            devbox.why.starts_with("Starts now, completes in 33 s"),
            "{}",
            devbox.why
        );
        let mut saturated = member("devbox", false, 10.0, 2.0, warm());
        saturated.state.gate = Gate::Saturated {
            cpu_utilization: 0.97,
            outside_utilization: 0.97,
        };
        let verdict = decide_with(
            vec![member("mini", true, 10.0, 1.0, Copy::Caller), saturated],
            2.0,
            &runs,
            &syncs,
        );
        assert_eq!(verdict.winner.as_deref(), Some("mini"));
        assert!(!verdict.scored("devbox").unwrap().estimate.eligible);
    }

    #[test]
    fn a_busy_caller_sends_even_a_new_workspace_away_and_respects_the_hold_limit() {
        let (runs, syncs) = none();
        // The caller frees room only after 100 s; the devbox has no copy yet but
        // could start after a 30 s first sync.
        let mut observations = vec![
            member("mini", true, 1.0, 1.0, Copy::Caller),
            member(
                "devbox",
                false,
                10.0,
                1.0,
                Copy::Remote {
                    warmth: Warmth::ColdWithHistory,
                    busy: false,
                },
            ),
        ];
        learn(
            &mut observations,
            &workspace(),
            &runs,
            &syncs,
            &LossHistory::default(),
        );
        let parameters = defaults();
        let mut wanted = Wanted {
            size: cores(4.0),
            hold_left: secs(600),
            parameters: &parameters,
        };
        let verdict = decide(observations.clone(), Vec::new(), &wanted);
        assert_eq!(
            verdict.winner.as_deref(),
            Some("mini"),
            "{}",
            verdict.reason
        );
        wanted.hold_left = secs(90);
        let verdict = decide(observations.clone(), Vec::new(), &wanted);
        assert_eq!(
            verdict.winner.as_deref(),
            Some("devbox"),
            "{}",
            verdict.reason
        );
        wanted.hold_left = secs(20);
        let verdict = decide(observations, Vec::new(), &wanted);
        assert_eq!(
            verdict.winner.as_deref(),
            None,
            "neither starts within 20 s: {}",
            verdict.reason
        );
    }

    #[test]
    fn the_reserve_counts_through_capacity() {
        let (runs, syncs) = none();
        let mut mini = member("mini", true, 10.0, 1.0, Copy::Caller);
        mini.state.capacity = Capacity::of(
            CoreCounts {
                performance: 10,
                efficiency: 0,
            },
            cores(7.0),
        );
        mini.state.free = cores(3.0);
        mini.state.in_use = Cores::ZERO;
        let verdict = decide_with(
            vec![mini, member("devbox", false, 10.0, 1.0, cold())],
            4.0,
            &runs,
            &syncs,
        );
        assert_eq!(verdict.winner.as_deref(), Some("devbox"));
        assert!(matches!(
            verdict.scored("mini").unwrap().estimate.start,
            Start::Never { .. }
        ));
    }

    #[test]
    fn warmth_breaks_near_ties_and_learned_history_changes_the_decision() {
        let (runs, syncs) = none();
        let verdict = decide_with(
            vec![
                member("mini", true, 0.0, 1.0, Copy::Caller),
                member("cold", false, 10.0, 1.0, cold()),
                member("warm", false, 10.0, 1.0, warm()),
            ],
            2.0,
            &runs,
            &syncs,
        );
        assert_eq!(verdict.winner.as_deref(), Some("warm"));

        // History: the label runs in 5 s on the devbox and 30 s on the mini.
        let sample = |member: &str, run: u64, at: u64| RunSample {
            label: "build".into(),
            family: None,
            member: member.into(),
            at: Timestamp::from_millis(at),
            run: secs(run),
            cpu: None,
            cold: false,
            units: None,
        };
        let trust = Parameters::default().estimates;
        let runs = label_history(
            (0..3)
                .flat_map(|i| [sample("devbox", 5, i), sample("mini", 30, i)])
                .collect(),
            &trust,
        );
        let syncs = SyncHistory::new(
            (0..2)
                .map(|_| SyncSample {
                    member: "devbox".into(),
                    workspace: "/w/app".into(),
                    repository: Some("/w/app/.git".into()),
                    warmth: Warmth::Warm,
                    sync: Duration::from_millis(500),
                })
                .collect(),
            &trust,
        );
        let observations = vec![
            member("mini", true, 10.0, 1.0, Copy::Caller),
            member("devbox", false, 10.0, 1.0, warm()),
        ];
        let without = decide_with(
            observations.clone(),
            2.0,
            &RunHistory::default(),
            &SyncHistory::default(),
        );
        assert_eq!(
            without.winner.as_deref(),
            Some("mini"),
            "{}",
            without.reason
        );
        let with = decide_with(observations, 2.0, &runs, &syncs);
        assert_eq!(with.winner.as_deref(), Some("devbox"), "{}", with.reason);
        let devbox = &with.scored("devbox").unwrap().estimate;
        assert_eq!(devbox.run_from, Source::MemberRuns { samples: 3 });
        assert_eq!(
            devbox.sync_from,
            Some(Source::WorkspaceSyncs { samples: 2 })
        );
        assert_eq!(devbox.completion_seconds, Some(Duration::from_millis(5500)));
    }

    #[test]
    fn a_busy_copy_blocks_and_an_unmeasured_member_assumes_the_typical_speed() {
        let (runs, syncs) = none();
        let mut unmeasured = member("new", false, 10.0, 1.0, warm());
        unmeasured.state.speed = None;
        let verdict = decide_with(
            vec![
                member("mini", true, 10.0, 2.0, Copy::Caller),
                member(
                    "devbox",
                    false,
                    10.0,
                    4.0,
                    Copy::Remote {
                        warmth: Warmth::Warm,
                        busy: true,
                    },
                ),
                unmeasured,
            ],
            1.0,
            &runs,
            &syncs,
        );
        assert!(matches!(
            verdict.scored("devbox").unwrap().estimate.start,
            Start::Blocked { .. }
        ));
        let new = &verdict.scored("new").unwrap().estimate;
        assert!(!new.speed_measured);
        assert_eq!(new.speed, 4.0, "the median of 2 and 4 measured speeds");
    }
    #[test]
    fn parameters_move_the_decision_and_inputs_read_back_exactly() {
        let (runs, syncs) = none();
        // A warm devbox at speed 1.2: 50 s + 3 s against 60 s on the caller.
        let mut observations = vec![
            member("mini", true, 10.0, 1.0, Copy::Caller),
            member("devbox", false, 10.0, 1.2, warm()),
        ];
        learn(
            &mut observations,
            &workspace(),
            &runs,
            &syncs,
            &LossHistory::default(),
        );
        let wanted = |parameters| Wanted {
            size: cores(2.0),
            hold_left: secs(600),
            parameters,
        };
        let defaults = defaults();
        let verdict = decide(observations.clone(), Vec::new(), &wanted(&defaults));
        assert_eq!(
            verdict.winner.as_deref(),
            Some("devbox"),
            "{}",
            verdict.reason
        );
        let wider = Placement {
            keep_local_margin: 0.2,
            ..defaults.clone()
        };
        let verdict = decide(observations.clone(), Vec::new(), &wanted(&wider));
        assert_eq!(
            verdict.winner.as_deref(),
            Some("mini"),
            "{}",
            verdict.reason
        );
        let slow_sync = Placement {
            sync_prior_warm: secs(10),
            ..defaults.clone()
        };
        let verdict = decide(observations.clone(), Vec::new(), &wanted(&slow_sync));
        assert_eq!(
            verdict.winner.as_deref(),
            Some("mini"),
            "{}",
            verdict.reason
        );
    }

    #[test]
    fn recorded_inputs_read_back_score_to_the_identical_verdict() {
        // Everything scoring reads, with awkward values: learned history of
        // sub-millisecond medians, running and queued work with sub-millisecond
        // times, an unmeasured member, contention and an unavailable copy.
        let trust = Parameters::default().estimates;
        let runs = label_history(
            (0..4)
                .flat_map(|i| {
                    [
                        RunSample {
                            label: "build".into(),
                            family: None,
                            member: "devbox".into(),
                            at: Timestamp::from_millis(i),
                            run: Duration::from_nanos(7_123_456_789 + i * 333_333),
                            cpu: None,
                            cold: false,
                            units: None,
                        },
                        RunSample {
                            label: "build".into(),
                            family: None,
                            member: "mini".into(),
                            at: Timestamp::from_millis(i),
                            run: Duration::from_nanos(9_876_543_210 - i * 777_777),
                            cpu: None,
                            cold: true,
                            units: None,
                        },
                    ]
                })
                .collect(),
            &trust,
        );
        let syncs = SyncHistory::new(
            (0..3)
                .map(|i| SyncSample {
                    member: "devbox".into(),
                    workspace: "/w/app".into(),
                    repository: Some("/w/app/.git".into()),
                    warmth: Warmth::Warm,
                    sync: Duration::from_nanos(412_345_678 + i),
                })
                .collect(),
            &trust,
        );
        let mut caller = member("mini", true, 3.0, 0.9137, Copy::Caller);
        caller.utilization = Some(0.6180339887498949);
        caller.holding = vec![
            held(cores(4.5), Some(Duration::from_nanos(12_345_678_901))),
            held(cores(2.5), None),
        ];
        caller.ahead = vec![Ahead {
            size: cores(1.5),
            run: Some(Duration::from_nanos(3_000_000_499)),
        }];
        let mut devbox = member("devbox", false, 8.0, 0.7234, warm());
        devbox.utilization = Some(0.31415926535);
        let mut unmeasured = member(
            "laptop",
            false,
            10.0,
            1.0,
            Copy::Remote {
                warmth: Warmth::ColdWithHistory,
                busy: false,
            },
        );
        unmeasured.state.speed = None;
        let unavailable = member("other", false, 10.0, 1.3, Copy::Unavailable);
        let mut observations = vec![caller, devbox, unmeasured, unavailable];
        learn(
            &mut observations,
            &workspace(),
            &runs,
            &syncs,
            &LossHistory::default(),
        );
        let parameters = defaults();
        let wanted = Wanted {
            size: cores(2.5),
            hold_left: Duration::from_millis(587_321),
            parameters: &parameters,
        };
        let verdict = decide(observations, Vec::new(), &wanted);
        assert!(
            verdict
                .candidates
                .iter()
                .any(|c| c.estimate.wait_seconds > Some(Duration::ZERO)),
            "{verdict:#?}"
        );
        // As a record keeps them: the state without its speed, and the inputs.
        let recorded: Vec<Observation> = verdict
            .candidates
            .iter()
            .map(|c| {
                let inputs: Inputs =
                    serde_json::from_str(&serde_json::to_string(&c.observation.inputs()).unwrap())
                        .unwrap();
                let mut state: MemberState =
                    serde_json::from_str(&serde_json::to_string(&c.observation.state).unwrap())
                        .unwrap();
                state.speed = None;
                Observation::from_inputs(state, inputs)
            })
            .collect();
        let again = decide(recorded, Vec::new(), &wanted);
        assert_eq!(again, verdict);
    }
}
