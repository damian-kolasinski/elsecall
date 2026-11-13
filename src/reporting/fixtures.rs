//! Fixed records and load buckets the report's tests compute from.
use crate::{
    inspection::ThermalState,
    load_log::buckets::Bucket,
    scheduling::{
        capacity::{Capacity, Cores},
        load::Gate,
        placed::{PlacedRecord, Reached, Side, SyncSummary, Warmth},
        placement::{CopyState, Estimate, Source, Start},
        processes::ProcessIdentity,
        record::{
            Arrival, ArrivalEstimate, Candidate, Decision, HoldBasis, HoldLimitRecord, Outcome,
            Record, State, Verdict,
        },
        request::{Kind, Placement, Urgency},
        run_id::RunId,
        timestamp::Timestamp,
    },
};
use std::time::Duration;

pub const MINUTE: u64 = 60_000;
/// When the fixtures' period starts.
pub const T0: u64 = 1_000 * MINUTE;

fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s)
}

fn ms(s: f64) -> u64 {
    (s * 1000.0).round() as u64
}

/// A request from `calling`, made `n` seconds into the period, that has ended.
pub fn base(n: u64, label: &str, calling: &str) -> Record {
    Record {
        plan_observation: None,
        workload: None,
        run_id: RunId::parse(&format!("r-{n:012x}")).unwrap(),
        kind: Kind::Run,
        synthetic: false,
        internal: false,
        peer_io: None,
        label: Some(label.into()),
        placement: Placement::Pinned,
        urgency: Urgency::Interactive {
            hold_limit_ms: None,
        },
        size: Cores::from_millis(2000),
        size_basis: None,
        calling_member: calling.into(),
        workspace: None,
        workspace_recorded: true,
        member: None,
        placement_note: None,
        nested_in: None,
        shares_capacity_of: None,
        state: State::Ended,
        outcome: None,
        requested_at: Timestamp::from_millis(T0 + n * 1000),
        started_at: None,
        ended_at: Some(Timestamp::from_millis(T0 + n * 1000 + 500)),
        waited_ms: None,
        run_ms: None,
        run_phases: None,
        cpu_ms: None,
        hold_limit: None,
        decision: None,
        warnings: Vec::new(),
        requested_by: ProcessIdentity {
            pid: 1,
            started_us: 1,
        },
        processes: Vec::new(),
        placed: None,
        settings: None,
        reacquired: 0,
        config_sync: None,
        pre_run_history: Some(Default::default()),
        fallbacks: Vec::new(),
    }
}

pub fn candidate(member: &str, chosen: bool, estimate: Option<Estimate>) -> Candidate {
    Candidate {
        member: member.into(),
        chosen,
        why: String::new(),
        capacity: Capacity {
            weighted_cores: Cores::from_millis(10_000),
            reserved: Cores::ZERO,
            capacity: Cores::from_millis(10_000),
        },
        in_use: Cores::ZERO,
        free: Cores::from_millis(10_000),
        queued: Default::default(),
        load: None,
        cpu_utilization_window: None,
        gate: Gate::Open,
        expected_run_seconds: estimate.as_ref().map(|e| e.run_seconds),
        expected_completion: None,
        estimate,
        inputs: None,
    }
}

pub fn estimate(sync: Option<f64>, run: f64, run_from: Source) -> Estimate {
    let sync_seconds = secs(sync.unwrap_or(0.0));
    Estimate {
        speed: 1.0,
        speed_measured: true,
        copy: if sync.is_some() {
            CopyState::Warm
        } else {
            CopyState::Caller
        },
        copy_busy: false,
        outside_utilization: None,
        start: Start::Now,
        wait_seconds: Some(Duration::ZERO),
        sync_seconds,
        sync_from: sync.map(|_| Source::Prior),
        run_seconds: secs(run),
        run_from,
        cold_factor: None,
        loss_factor: None,
        slowdown: 1.0,
        completion_seconds: Some(sync_seconds + secs(run)),
        eligible: true,
    }
}

pub fn decision(
    verdict: Verdict,
    candidates: Vec<Candidate>,
    arrival: Option<(&str, f64)>,
) -> Decision {
    Decision {
        decided_at: Timestamp::from_millis(T0),
        verdict,
        reason: String::new(),
        candidates,
        unreachable: Vec::new(),
        parameters: None,
        scoring: None,
        arrival: arrival.map(|(member, wait)| Arrival {
            scored_at: Timestamp::from_millis(T0),
            winner: Some(member.into()),
            candidates: vec![ArrivalEstimate {
                member: member.into(),
                wait_seconds: Some(secs(wait)),
                sync_seconds: Duration::ZERO,
                run_seconds: Duration::ZERO,
                completion_seconds: None,
                eligible: true,
                state: None,
                inputs: None,
            }],
            size: None,
            hold_left: None,
        }),
    }
}

/// A placeable run from `mini`.
pub struct Run {
    pub n: u64,
    pub label: &'static str,
    pub member: &'static str,
    pub predicted_run: f64,
    pub run_from: Source,
    pub actual_run: f64,
    pub predicted_wait: f64,
    pub waited: f64,
    /// Elsewhere: predicted 3 s (the prior), synced in 1 s.
    pub warmth: Warmth,
}

impl Default for Run {
    fn default() -> Self {
        Run {
            n: 1,
            label: "build",
            member: "mini",
            predicted_run: 10.0,
            run_from: Source::MemberRuns { samples: 3 },
            actual_run: 10.0,
            predicted_wait: 2.0,
            waited: 2.0,
            warmth: Warmth::Warm,
        }
    }
}

/// A placeable run from `mini` that ran on `run.member`.
pub fn placed(run: Run) -> Record {
    let elsewhere = run.member != "mini";
    let sync = if elsewhere { 1.0 } else { 0.0 };
    let mut record = base(run.n, run.label, "mini");
    record.placement = Placement::Placeable;
    record.member = Some(run.member.into());
    record.outcome = Some(Outcome::Exited { exit_code: 0 });
    record.waited_ms = Some(ms(run.waited));
    record.run_ms = Some(ms(run.actual_run + sync));
    let other = if elsewhere { "mini" } else { "devbox" };
    record.decision = Some(decision(
        Verdict::Started,
        vec![
            candidate(
                run.member,
                true,
                Some(estimate(
                    elsewhere.then_some(3.0),
                    run.predicted_run,
                    run.run_from.clone(),
                )),
            ),
            candidate(
                other,
                false,
                Some(estimate((!elsewhere).then_some(3.0), 60.0, Source::Prior)),
            ),
        ],
        Some((run.member, run.predicted_wait)),
    ));
    if elsewhere {
        record.placed = Some(PlacedRecord {
            side: Side::Caller,
            calling_member: "mini".into(),
            member: run.member.into(),
            caller_workspace: "/w/app".into(),
            repository: None,
            copy: None,
            location: None,
            warmth: Some(run.warmth),
            reached: Reached::Finished,
            sync: Some(SyncSummary {
                duration_ms: Some(1000),
                ..Default::default()
            }),
            exit_code: Some(0),
            signal: None,
            last_contact_at: None,
            leftovers: None,
            leftovers_wait_ms: None,
            interrupted: None,
        });
    }
    record
}

/// A pinned run on `member` that took `run` seconds, with the label's median
/// of 4 s behind its hold limit.
pub fn pinned(n: u64, label: &str, member: &str, run: f64) -> Record {
    let mut record = base(n, label, member);
    record.member = Some(member.into());
    record.outcome = Some(Outcome::Exited { exit_code: 0 });
    record.waited_ms = Some(0);
    record.run_ms = Some(ms(run));
    record.hold_limit = Some(HoldLimitRecord {
        seconds: secs(596.0),
        basis: HoldBasis::Default,
        expected_run_seconds: Some(secs(4.0)),
    });
    let mut chosen = candidate(member, true, None);
    chosen.expected_run_seconds = Some(secs(4.0));
    record.decision = Some(decision(Verdict::Started, vec![chosen], None));
    record
}

/// A minute of load `at_min` minutes into the period.
pub fn bucket(
    at_min: u64,
    cpu: f64,
    in_use: f64,
    queued: usize,
    saturated: u64,
    speed: f64,
) -> Bucket {
    Bucket {
        at: Timestamp::from_millis(T0 + at_min * MINUTE),
        seconds: 60,
        cpu_mean: Some(cpu),
        cpu_max: Some(cpu),
        outside_mean: Some((cpu - in_use / 10.0).max(0.0)),
        weighted_cores: Cores::from_millis(10_000),
        capacity: Cores::from_millis(10_000),
        in_use_mean: in_use,
        in_use_max: Cores::from_f64(in_use).unwrap(),
        queued_max: queued,
        interactive_queued_max: Some(queued),
        busy_seconds: if in_use > 0.0 { 60 } else { 0 },
        saturated_seconds: saturated,
        saturated_busy_seconds: if in_use > 0.0 { saturated } else { 0 },
        memory_pressure_seconds: 0,
        worst_memory_pressure: None,
        worst_thermal_state: Some(ThermalState::Nominal),
        speed_min: Some(speed),
        speed_max: Some(speed),
    }
}

/// A blocked episode by memory pressure from `since` to `until` (`None` while it
/// lasts), led by the root daemons `outside`, each with its resident size and
/// cores.
pub fn episode(
    since: Timestamp,
    until: Option<Timestamp>,
    outside: &[(&str, u64, f64)],
) -> crate::outside_pressure::store::Episode {
    use crate::outside_pressure::{
        blocked::{Blocked, OutsideProcess, Reason},
        sampling::{Memory, Owner},
    };
    crate::outside_pressure::store::Episode {
        blocked: Blocked {
            reason: Reason::MemoryPressure {
                level: crate::inspection::MemoryPressure::Warning,
            },
            since,
            attributed_at: since.after(Duration::from_secs(300)),
            outside: outside
                .iter()
                .map(|(name, resident_bytes, cpu_cores)| OutsideProcess {
                    name: (*name).into(),
                    pid: 412,
                    owner: Owner::Root,
                    memory: Memory {
                        resident_bytes: *resident_bytes,
                        footprint_bytes: None,
                    },
                    cpu_cores: *cpu_cores,
                    uptime_seconds: 62 * 86_400,
                })
                .collect(),
        },
        until,
    }
}
