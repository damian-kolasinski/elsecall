//! What history says about labels: how long their runs take on each member and
//! how many cores they keep busy. Pure: the records come from the caller.
//!
//! **Trust.** An estimate is formed only from completed runs requested within the
//! recency of `[estimates]`, at most the latest `window` of each kind, and only
//! once there are at least `minimum_runs` of them (by default 14 days, 20 and 3;
//! see `parameters`). Fewer samples, or older ones, say too little about a label
//! whose work changes as its code does.
//!
//! **Run time** is estimated in [`run_time`], from the label's own runs, then its
//! family's, converting runs between members by the speed ratios learned in
//! [`ratios`].
//!
//! Runs into a cold copy are learned apart from warm ones, since a cold build does
//! more work. A run's time is its command's: for a placed run, the workspace sync
//! is taken out.
//!
//! **Size** is the upper quartile of recent runs' average busy cores (CPU time over
//! run time, for runs of at least [`MINIMUM_RUN_FOR_SIZE`]), rounded up to half a
//! core. The upper quartile errs towards admitting less at once, which is the safe
//! side of overcommitting.
pub mod plan_fit;
pub mod ratios;
pub mod run_time;

pub use run_time::{Basis, Conversions, RunEstimate, RunEstimates, RunHistory};

use crate::scheduling::{
    capacity::Cores,
    parameters::Estimates,
    placed::Side,
    record::{Outcome, Record},
    request::Kind,
    timestamp::Timestamp,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Shorter runs are dominated by start-up, so their busy cores say little.
pub const MINIMUM_RUN_FOR_SIZE: Duration = Duration::from_secs(1);
/// Records read for one label, or one family: enough for every member's window.
pub fn records_read(estimates: &Estimates) -> u32 {
    (estimates.window as u32 * 10).max(200)
}
/// Records read across every label, for learned speed ratios.
pub const RATIO_RECORDS_READ: u32 = 2000;
const SIZE_STEP: u32 = 500;

/// One completed run.
#[derive(Clone, Debug, PartialEq)]
pub struct RunSample {
    pub label: String,
    /// The pattern of the label rule it matched, which names its family.
    pub family: Option<String>,
    pub member: String,
    pub at: Timestamp,
    /// The command's own time, without any workspace sync.
    pub run: Duration,
    pub cpu: Option<Duration>,
    /// Whether it ran in a copy that was cold when it was placed.
    pub cold: bool,
    /// The units of its declared sequential plan its time covers, when its
    /// wrapper observed them: every planned unit, or the prefix it stopped
    /// after. `None` for opaque work and for plans not fully observed.
    pub units: Option<u32>,
}

/// The run samples in `records` requested within `recency` before `now`, newest
/// first. Leases, unlabelled and unfinished runs and the member side of placed
/// runs are left out.
pub fn samples(records: &[Record], now: Timestamp, recency: Duration) -> Vec<RunSample> {
    samples_of(Kind::Run, records, now, recency)
}

/// [`samples`] of requests of `kind`: a lease's time is how long its holder
/// held it, which says how long a running lease of the same label is likely
/// to, not how long a run takes.
pub fn samples_of(
    kind: Kind,
    records: &[Record],
    now: Timestamp,
    recency: Duration,
) -> Vec<RunSample> {
    let since = Timestamp::from_millis(now.millis().saturating_sub(recency.as_millis() as u64));
    let mut samples: Vec<RunSample> = records
        .iter()
        .filter(|r| r.kind == kind && r.requested_at >= since)
        .filter(|r| !r.synthetic && !crate::rehearsal::synthetic(r.label.as_deref()))
        .filter(|r| r.outcome.as_ref().is_some_and(|o| o.completed()))
        .filter(|r| r.placed.as_ref().is_none_or(|p| p.side == Side::Caller))
        .filter_map(|record| {
            let member = record.member.clone()?;
            let total = Duration::from_millis(record.run_ms?);
            let sync = record
                .placed
                .as_ref()
                .and_then(|p| p.sync.as_ref())
                .and_then(|s| s.duration_ms)
                .map_or(Duration::ZERO, Duration::from_millis);
            Some(RunSample {
                label: record.label.clone()?,
                family: record
                    .settings
                    .as_ref()
                    .and_then(|s| s.rule.as_ref())
                    .map(|rule| rule.pattern.clone()),
                member,
                at: record.requested_at,
                run: total.saturating_sub(sync),
                cpu: record.cpu_ms.map(Duration::from_millis),
                cold: record.placed.as_ref().is_some_and(|p| p.cold()),
                units: observed_units(record),
            })
        })
        .collect();
    samples.sort_by_key(|s| std::cmp::Reverse(s.at));
    samples
}

/// The units of `record`'s declared plan that its run time covers: all of a
/// plan its wrapper saw through, or the prefix it stopped after. A run that did
/// not exit, or whose wrapper reported no coverage, covers no known number.
fn observed_units(record: &Record) -> Option<u32> {
    use crate::workload_metadata::Observation;
    if !matches!(record.outcome, Some(Outcome::Exited { .. })) {
        return None;
    }
    match record.plan_observation.as_ref()? {
        Observation::AllUnitsObserved { coverage }
        | Observation::PrefixObserved { coverage, .. } => {
            Some(coverage.attempted_units).filter(|units| *units > 0)
        }
        Observation::Censored { .. } | Observation::Unknown {} => None,
    }
}

/// A size learned from history.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LearnedSize {
    pub size: Cores,
    pub samples: usize,
}

/// The middle value: the upper of the two middle ones for an even count.
fn median(mut values: Vec<f64>) -> Option<f64> {
    values.sort_by(f64::total_cmp);
    values.get(values.len() / 2).copied()
}

/// A label's learned size from its `samples` (newest first), when trusted.
pub fn size(samples: &[RunSample], estimates: &Estimates) -> Option<LearnedSize> {
    let mut busy: Vec<f64> = samples
        .iter()
        .filter(|s| s.run >= MINIMUM_RUN_FOR_SIZE)
        .filter_map(|s| Some(s.cpu?.as_secs_f64() / s.run.as_secs_f64()))
        .take(estimates.window)
        .collect();
    if busy.len() < estimates.minimum_runs {
        return None;
    }
    busy.sort_by(f64::total_cmp);
    let upper_quartile = busy[(busy.len() * 3).div_ceil(4) - 1];
    let millis = (upper_quartile * 1000.0).ceil().min(1e9) as u32;
    let size = Cores::from_millis(millis.div_ceil(SIZE_STEP).max(1) * SIZE_STEP);
    Some(LearnedSize {
        size,
        samples: busy.len(),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample(
        member: &str,
        at: u64,
        run: f64,
        cpu: Option<f64>,
        cold: bool,
    ) -> RunSample {
        labelled("build", None, member, at, run, cold, cpu)
    }
    pub(crate) fn labelled(
        label: &str,
        family: Option<&str>,
        member: &str,
        at: u64,
        run: f64,
        cold: bool,
        cpu: Option<f64>,
    ) -> RunSample {
        RunSample {
            label: label.into(),
            family: family.map(Into::into),
            member: member.into(),
            at: Timestamp::from_millis(at),
            run: Duration::from_secs_f64(run),
            cpu: cpu.map(Duration::from_secs_f64),
            cold,
            units: None,
        }
    }
    pub(crate) fn defaults() -> Estimates {
        crate::scheduling::parameters::Parameters::default().estimates
    }

    #[test]
    fn size_is_the_upper_quartile_of_busy_cores_rounded_up_to_half_a_core() {
        let samples = |busy: &[f64]| -> Vec<RunSample> {
            busy.iter()
                .enumerate()
                .map(|(i, b)| sample("slow", i as u64, 10.0, Some(b * 10.0), false))
                .collect()
        };
        assert_eq!(size(&samples(&[2.0, 2.0]), &defaults()), None);
        assert_eq!(
            size(&samples(&[1.2, 3.1, 2.0, 2.2]), &defaults()),
            Some(LearnedSize {
                size: Cores::from_millis(2500),
                samples: 4
            })
        );
        assert_eq!(
            size(&samples(&[0.1, 0.1, 0.1]), &defaults()).unwrap().size,
            Cores::from_millis(500),
            "never below half a core"
        );
        let short: Vec<RunSample> = (0..5)
            .map(|i| sample("slow", i, 0.5, Some(4.0), false))
            .collect();
        assert_eq!(
            size(&short, &defaults()),
            None,
            "short runs say little about size"
        );
    }

    #[test]
    fn samples_take_out_the_sync_and_skip_old_or_unfinished_runs() {
        use crate::scheduling::{
            placed::{PlacedRecord, Reached, SyncSummary},
            processes::ProcessIdentity,
            record::{Outcome, State},
            request::{Placement, Urgency},
            run_id::RunId,
            settings::{AppliedRule, Settings, Source},
        };
        let day = 24 * 60 * 60 * 1000;
        let now = Timestamp::from_millis(100 * day);
        let record = |at: u64, outcome: Option<Outcome>, placed: Option<PlacedRecord>| Record {
            plan_observation: None,
            workload: None,
            run_id: RunId::generate(),
            kind: Kind::Run,
            synthetic: false,
            internal: false,
            peer_io: None,
            label: Some("build".into()),
            placement: Placement::Placeable,
            urgency: Urgency::Interactive {
                hold_limit_ms: None,
            },
            size: Cores::ONE,
            size_basis: None,
            calling_member: "mini".into(),
            workspace: None,
            workspace_recorded: true,
            member: Some("devbox".into()),
            placement_note: None,
            nested_in: None,
            shares_capacity_of: None,
            state: State::Ended,
            outcome,
            requested_at: Timestamp::from_millis(at),
            started_at: None,
            ended_at: None,
            waited_ms: None,
            run_ms: Some(30_000),
            run_phases: None,
            cpu_ms: Some(60_000),
            hold_limit: None,
            decision: None,
            warnings: Vec::new(),
            requested_by: ProcessIdentity {
                pid: 1,
                started_us: 1,
            },
            processes: Vec::new(),
            placed,
            settings: Some(Settings {
                rule: Some(AppliedRule {
                    number: 2,
                    pattern: "build *".into(),
                }),
                size: Source::Default,
                urgency: Source::Default,
                hold_limit: Some(Source::Default),
                deadline: None,
                at_deadline: None,
                placement: Source::Rule,
            }),
            reacquired: 0,
            config_sync: None,
            pre_run_history: Some(Default::default()),
            fallbacks: Vec::new(),
        };
        let placed = |side| PlacedRecord {
            side,
            calling_member: "mini".into(),
            member: "devbox".into(),
            caller_workspace: "/w".into(),
            repository: None,
            copy: None,
            location: None,
            warmth: None,
            reached: Reached::Finished,
            sync: Some(SyncSummary {
                duration_ms: Some(4_000),
                ..SyncSummary::default()
            }),
            exit_code: Some(0),
            signal: None,
            last_contact_at: None,
            leftovers: None,
            leftovers_wait_ms: None,
            interrupted: None,
        };
        let exited = Some(Outcome::Exited { exit_code: 1 });
        let mut unlabelled = record(99 * day, exited.clone(), None);
        unlabelled.label = None;
        let mut lease = record(99 * day, Some(Outcome::Released), None);
        lease.kind = Kind::Lease;
        lease.run_ms = Some(90_000);
        let records = [
            lease,
            record(99 * day, exited.clone(), Some(placed(Side::Caller))),
            record(99 * day, exited.clone(), Some(placed(Side::Member))),
            record(80 * day, exited.clone(), None),
            record(99 * day, None, None),
            record(
                99 * day,
                Some(Outcome::Failed {
                    code: "connection_lost".into(),
                }),
                None,
            ),
            unlabelled,
        ];
        let found = samples(&records, now, defaults().recency);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].run, Duration::from_secs(26));
        assert_eq!(found[0].cpu, Some(Duration::from_secs(60)));
        assert_eq!(found[0].label, "build");
        assert_eq!(found[0].family.as_deref(), Some("build *"));
        assert!(!found[0].cold);
        // A running lease is estimated from leases: how long holders held them.
        let leases = samples_of(Kind::Lease, &records, now, defaults().recency);
        assert_eq!(leases.len(), 1, "{leases:?}");
        assert_eq!(leases[0].run, Duration::from_secs(90));

        // A run's units are those its observed plan covered, and only when it
        // exited.
        let observed = |outcome: Outcome, observation: &str| {
            let mut run = record(99 * day, Some(outcome), None);
            run.plan_observation =
                Some(crate::workload_metadata::parse_observation(observation).unwrap());
            run
        };
        let exited = Outcome::Exited { exit_code: 0 };
        let all = r#"{"status":"all_units_observed","coverage":{"attempted_units":3,"terminal_units":3,"failed_units":0}}"#;
        let prefix = r#"{"status":"prefix_observed","reason":"failure","coverage":{"attempted_units":2,"terminal_units":2,"failed_units":1}}"#;
        let censored = r#"{"status":"censored","reason":"interrupted","coverage":{"attempted_units":2,"terminal_units":1,"failed_units":0}}"#;
        let units = |record: Record| samples(&[record], now, defaults().recency)[0].units;
        assert_eq!(units(observed(exited.clone(), all)), Some(3));
        assert_eq!(units(observed(exited.clone(), prefix)), Some(2));
        assert_eq!(units(observed(exited.clone(), censored)), None);
        assert_eq!(
            units(observed(exited.clone(), r#"{"status":"unknown"}"#)),
            None
        );
        assert_eq!(units(observed(Outcome::HolderExited, all)), None);
        assert_eq!(units(record(99 * day, Some(exited), None)), None);
    }
}
