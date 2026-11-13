//! The `fleet_busy` failure: why a request did not start, what holds the member,
//! and when trying again is likely to succeed.
use crate::{
    failure::Failure,
    scheduling::{
        capacity::Cores,
        estimates::{self, Holding},
        load::Gate,
        request::Kind,
        run_id::RunId,
        timestamp::Timestamp,
    },
};
use serde::Serialize;
use std::time::Duration;

/// Work holding capacity when the request gave up.
pub struct Work {
    pub run_id: RunId,
    pub kind: Kind,
    pub label: Option<String>,
    pub size: Cores,
    pub started_at: Option<Timestamp>,
    pub expected_run: Option<Duration>,
}

/// What the member looked like when the hold limit passed.
pub struct Situation {
    pub now: Timestamp,
    pub hold_limit: Duration,
    pub size: Cores,
    pub free: Cores,
    pub capacity: Cores,
    pub waiting: usize,
    pub gate: Gate,
    pub holding: Vec<Work>,
}

#[derive(Serialize)]
struct Running {
    run_id: RunId,
    kind: Kind,
    label: Option<String>,
    size: Cores,
    running_seconds: u64,
    expected_remaining_seconds: Option<u64>,
}
#[derive(Serialize)]
struct Details {
    retry_after_seconds: u64,
    running: Vec<Running>,
}

/// Work running on some member, as a fleet-wide `fleet_busy` lists it.
#[derive(Serialize)]
pub struct RunningOn {
    pub member: String,
    pub run_id: RunId,
    pub kind: Kind,
    pub label: Option<String>,
    pub size: Cores,
    pub running_seconds: u64,
    pub expected_remaining_seconds: Option<u64>,
}
impl RunningOn {
    pub fn of(member: &str, work: Work, now: Timestamp) -> Self {
        let elapsed = now.since(work.started_at.unwrap_or(now));
        Self {
            member: member.to_owned(),
            run_id: work.run_id,
            kind: work.kind,
            label: work.label,
            size: work.size,
            running_seconds: elapsed.as_secs(),
            expected_remaining_seconds: work
                .expected_run
                .map(|run| run.saturating_sub(elapsed).as_secs()),
        }
    }
}

/// Why one member could not start a placeable request, and when it might.
#[derive(Serialize)]
pub struct MemberSituation {
    pub member: String,
    pub why: String,
    /// When the member is estimated to be able to start it; `None` if never.
    #[serde(skip)]
    pub retry: Option<Duration>,
}

#[derive(Serialize)]
struct FleetDetails {
    retry_after_seconds: u64,
    members: Vec<MemberSituation>,
    running: Vec<RunningOn>,
}

/// `fleet_busy` for a placeable request no member could start within its hold
/// limit: why each member could not, what runs where, and the earliest any
/// member is estimated to be able to start it.
pub fn fleet_busy_across(
    hold_limit: Duration,
    members: Vec<MemberSituation>,
    running: Vec<RunningOn>,
) -> Failure {
    let retry = members
        .iter()
        .filter_map(|m| m.retry)
        .min()
        .unwrap_or(estimates::UNKNOWN_RETRY)
        .max(estimates::MINIMUM_RETRY);
    let summary = members
        .iter()
        .map(|m| format!("{}: {}", m.member, m.why))
        .collect::<Vec<_>>()
        .join(" ");
    Failure::new(
        "fleet_busy",
        format!(
            "No member could start it within the {} s hold limit. {summary}",
            hold_limit.as_secs()
        ),
        format!(
            "Retry in about {} s, request a smaller --size, or name a member with --member.",
            retry.as_secs()
        ),
    )
    .with_details(FleetDetails {
        retry_after_seconds: retry.as_secs(),
        members,
        running,
    })
}

pub fn fleet_busy(situation: Situation) -> Failure {
    let mut running = Vec::new();
    let mut holding = Vec::new();
    for work in situation.holding {
        let elapsed = situation
            .now
            .since(work.started_at.unwrap_or(situation.now));
        holding.push(Holding {
            size: work.size,
            elapsed,
            expected_run: work.expected_run,
        });
        running.push(Running {
            run_id: work.run_id,
            kind: work.kind,
            label: work.label,
            size: work.size,
            running_seconds: elapsed.as_secs(),
            expected_remaining_seconds: work
                .expected_run
                .map(|run| run.saturating_sub(elapsed).as_secs()),
        });
    }
    let retry = estimates::retry_after(situation.free, situation.size, &holding);
    let why = match situation.gate {
        Gate::Saturated {
            outside_utilization,
            ..
        } => format!(
            "the member is saturated by outside load ({:.0}% CPU)",
            outside_utilization * 100.0
        ),
        Gate::MemoryPressure { .. } => "the member is under memory pressure".into(),
        Gate::Open | Gate::Unknown => format!(
            "{} of {} cores are free and {} other requests wait",
            situation.free, situation.capacity, situation.waiting
        ),
    };
    Failure::new(
        "fleet_busy",
        format!(
            "No capacity within the {} s hold limit: {why}.",
            situation.hold_limit.as_secs()
        ),
        format!(
            "Retry in about {} s, request a smaller --size, or run it as --deferrable with a --deadline.",
            retry.as_secs()
        ),
    )
    .with_details(Details {
        retry_after_seconds: retry.as_secs(),
        running,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fleet_wide_busy_lists_every_member_and_retries_at_the_earliest() {
        let failure = fleet_busy_across(
            Duration::from_secs(30),
            vec![
                MemberSituation {
                    member: "mini".into(),
                    why: "Cannot start within the 30 s left of the hold limit: wait 80 s.".into(),
                    retry: Some(Duration::from_secs(80)),
                },
                MemberSituation {
                    member: "devbox".into(),
                    why: "Saturated by outside load (97% CPU).".into(),
                    retry: Some(estimates::BLOCKED_RETRY),
                },
            ],
            vec![RunningOn {
                member: "mini".into(),
                run_id: RunId::parse("r-000000000001").unwrap(),
                kind: Kind::Run,
                label: Some("build".into()),
                size: Cores::ONE,
                running_seconds: 20,
                expected_remaining_seconds: Some(80),
            }],
        );
        assert_eq!(failure.code, "fleet_busy");
        assert!(
            failure.message.contains("mini: Cannot start"),
            "{}",
            failure.message
        );
        assert!(failure.message.contains("devbox: Saturated"));
        assert!(failure.hint.starts_with("Retry in about 30 s"));
        let details = failure.details.unwrap();
        assert_eq!(details["retry_after_seconds"], 30);
        assert_eq!(details["running"][0]["member"], "mini");
        assert_eq!(details["members"][1]["member"], "devbox");
    }

    #[test]
    fn reports_running_work_and_a_retry_estimate() {
        let now = Timestamp::from_millis(100_000);
        let failure = fleet_busy(Situation {
            now,
            hold_limit: Duration::from_secs(30),
            size: Cores::ONE,
            free: Cores::ZERO,
            capacity: Cores::ONE,
            waiting: 2,
            gate: Gate::Open,
            holding: vec![Work {
                run_id: RunId::parse("r-000000000001").unwrap(),
                kind: Kind::Lease,
                label: Some("build".into()),
                size: Cores::ONE,
                started_at: Some(Timestamp::from_millis(60_000)),
                expected_run: Some(Duration::from_secs(100)),
            }],
        });
        assert_eq!(failure.code, "fleet_busy");
        assert_eq!(
            failure.message,
            "No capacity within the 30 s hold limit: 0 of 1 cores are free and 2 other requests wait."
        );
        assert!(failure.hint.starts_with("Retry in about 60 s"));
        let details = failure.details.unwrap();
        assert_eq!(details["retry_after_seconds"], 60);
        assert_eq!(details["running"][0]["label"], "build");
        assert_eq!(details["running"][0]["running_seconds"], 40);
        assert_eq!(details["running"][0]["expected_remaining_seconds"], 60);
    }
}
