//! How often each member lost contact with the runs placed on it, from the
//! calling member's records. Pure.
//!
//! A placed run whose connection is lost is lost whole: the member stops the
//! command and nothing it produced comes back, so the caller runs it again from
//! the start. How often that happens is a rate per hour of placed work: the runs
//! that ended `connection_lost` over the hours placed runs spent on the member
//! (their `run_ms`, sync included, and the time so far of those still going),
//! over the latest `loss_window`. Placement weighs a member's expected run by it
//! (see `placement`). The calling member itself never loses contact with its own
//! runs, and runs its own service restart ended say nothing about the member:
//! they count neither as losses nor as placed work.
use crate::scheduling::{
    placed::{Interruption, Side},
    record::{Outcome, Record},
    timestamp::{Timestamp, millis},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};

/// The failure code of a placed run whose connection was lost.
pub const CONNECTION_LOST: &str = "connection_lost";

/// Lost runs on one member, and the time placed runs spent there.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Losses {
    /// Placed runs that ended with their connection lost.
    pub lost: u32,
    /// The time every placed run there took, lost ones included.
    #[serde(rename = "run_ms", with = "millis")]
    pub run: Duration,
}

impl Losses {
    /// Lost runs per second of placed work, counting at least `minimum` of it,
    /// so that one early loss does not read as a member that loses everything.
    pub fn per_second(&self, minimum: Duration) -> f64 {
        let exposure = self.run.max(minimum).as_secs_f64();
        if self.lost == 0 || exposure == 0.0 {
            0.0
        } else {
            f64::from(self.lost) / exposure
        }
    }

    /// How long work expected to take `work` takes on average when each loss
    /// at rate `per_second` starts it over: `(e^(λ·work) − 1) / λ`, the
    /// expected time of a task that must run `work` uninterrupted. It is `work`
    /// for a member that loses nothing.
    pub fn exposed(work: Duration, per_second: f64) -> Duration {
        let seconds = work.as_secs_f64();
        if per_second <= 0.0 || seconds == 0.0 {
            return work;
        }
        // A bound far past any hold limit keeps the arithmetic finite.
        let exposed = (per_second * seconds).exp_m1() / per_second;
        Duration::from_secs_f64(exposed.min(MAXIMUM_EXPOSED.as_secs_f64())).max(work)
    }
}

/// The most expected time a run is given, for a member that loses nearly all.
pub const MAXIMUM_EXPOSED: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Every member's losses over the window.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LossHistory {
    by_member: BTreeMap<String, Losses>,
}

impl LossHistory {
    /// The losses of runs this member placed elsewhere and requested within
    /// `window` before `now`, from `records`. Runs still going add their time
    /// so far; synthetic work counts never.
    pub fn new(records: &[Record], now: Timestamp, window: Duration) -> Self {
        let since = Timestamp::from_millis(now.millis().saturating_sub(window.as_millis() as u64));
        let mut by_member: BTreeMap<String, Losses> = BTreeMap::new();
        for record in records
            .iter()
            .filter(|r| !window.is_zero() && r.requested_at >= since)
            .filter(|r| !r.synthetic && !crate::rehearsal::synthetic(r.label.as_deref()))
        {
            let Some(placed) = record.placed.as_ref().filter(|p| p.side == Side::Caller) else {
                continue;
            };
            if placed.interrupted == Some(Interruption::ServiceRestart) {
                continue;
            }
            let (run, lost) = match (&record.outcome, record.run_ms, record.started_at) {
                (Some(outcome), Some(run_ms), _) => (
                    Duration::from_millis(run_ms),
                    matches!(outcome, Outcome::Failed { code } if code == CONNECTION_LOST),
                ),
                (None, _, Some(started)) => (now.since(started), false),
                _ => continue,
            };
            let losses = by_member.entry(placed.member.clone()).or_default();
            losses.run += run;
            losses.lost += u32::from(lost);
        }
        Self { by_member }
    }

    /// What `member` lost, when placed runs went there in the window.
    pub fn of(&self, member: &str) -> Option<Losses> {
        self.by_member.get(member).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hours(n: f64) -> Duration {
        Duration::from_secs_f64(n * 3600.0)
    }

    /// A placed run of `minutes` on `member`, requested `ago` minutes before
    /// `NOW`, from this member's side.
    fn placed(member: &str, ago: u64, minutes: u64, outcome: Option<&str>) -> Record {
        let mut record: Record = serde_json::from_value(serde_json::json!({
            "run_id": "r-000000000001", "kind": "run", "label": "test one",
            "placement": "placeable", "urgency": {"interactive": {"hold_limit_ms": null}},
            "size": 1.0, "calling_member": "caller", "workspace_recorded": true,
            "member": member, "state": "ended", "requested_at": "2026-11-09T00:00:00Z",
            "requested_by": {"pid": 1, "started_us": 1}, "processes": [], "reacquired": 0,
            "warnings": [],
            "placed": {"side": "caller", "calling_member": "caller", "member": member,
                       "caller_workspace": "/repo", "reached": "started"},
        }))
        .unwrap();
        record.requested_at = Timestamp::from_millis(NOW - ago * 60_000);
        record.started_at = Some(record.requested_at);
        match outcome {
            Some("lost") => {
                record.outcome = Some(Outcome::Failed {
                    code: CONNECTION_LOST.into(),
                });
                record.run_ms = Some(minutes * 60_000);
            }
            Some(_) => {
                record.outcome = Some(Outcome::Exited { exit_code: 0 });
                record.run_ms = Some(minutes * 60_000);
            }
            None => (),
        }
        record
    }
    const NOW: u64 = 1_794_268_800_000;

    #[test]
    fn losses_count_placed_runs_this_side_saw_lost_over_all_placed_work() {
        let mut restarted = placed("peer", 30, 20, Some("lost"));
        restarted.placed.as_mut().unwrap().interrupted = Some(Interruption::ServiceRestart);
        let mut hosted = placed("peer", 30, 20, Some("lost"));
        hosted.placed.as_mut().unwrap().side = Side::Member;
        let mut synthetic = placed("peer", 30, 20, Some("lost"));
        synthetic.synthetic = true;
        let mut rehearsal = placed("peer", 30, 20, Some("lost"));
        rehearsal.label = Some("rehearsal: test one".into());
        let mut local = placed("caller", 30, 20, Some("exited"));
        local.placed = None;
        let records = [
            placed("peer", 60, 10, Some("lost")),
            placed("peer", 120, 50, Some("exited")),
            // Still running, for 30 minutes so far.
            placed("peer", 30, 0, None),
            // Older than the window.
            placed("peer", 25 * 60, 60, Some("lost")),
            placed("other", 60, 5, Some("exited")),
            restarted,
            hosted,
            synthetic,
            rehearsal,
            local,
        ];
        let history = LossHistory::new(&records, Timestamp::from_millis(NOW), hours(24.0));
        assert_eq!(
            history.of("peer"),
            Some(Losses {
                lost: 1,
                run: Duration::from_secs(90 * 60)
            })
        );
        assert_eq!(
            history.of("other"),
            Some(Losses {
                lost: 0,
                run: Duration::from_secs(5 * 60)
            })
        );
        assert_eq!(history.of("caller"), None);
        let none = LossHistory::new(&records, Timestamp::from_millis(NOW), Duration::ZERO);
        assert_eq!(none.of("peer"), None, "a window of 0 counts nothing");
    }

    #[test]
    fn a_member_that_never_loses_takes_the_work_as_it_is() {
        let none = Losses {
            lost: 0,
            run: hours(10.0),
        };
        assert_eq!(none.per_second(hours(1.0)), 0.0);
        assert_eq!(Losses::exposed(hours(1.0), 0.0), hours(1.0));
    }

    #[test]
    fn losses_lengthen_long_work_far_more_than_short_work() {
        // One loss an hour: a 2-minute run is barely longer, an hour-long one
        // takes e − 1 hours.
        let rate = Losses {
            lost: 10,
            run: hours(10.0),
        }
        .per_second(hours(1.0));
        let short = Losses::exposed(Duration::from_secs(120), rate).as_secs_f64();
        assert!((short - 122.0).abs() < 0.1, "{short}");
        let long = Losses::exposed(hours(1.0), rate).as_secs_f64();
        assert!(
            (long - (std::f64::consts::E - 1.0) * 3600.0).abs() < 0.01,
            "{long}"
        );
    }

    #[test]
    fn little_exposure_counts_as_the_minimum() {
        let early = Losses {
            lost: 1,
            run: Duration::from_secs(60),
        };
        assert!((early.per_second(hours(1.0)) - 1.0 / 3600.0).abs() < 1e-12);
        assert!((early.per_second(Duration::ZERO) - 1.0 / 60.0).abs() < 1e-12);
    }

    #[test]
    fn a_member_that_loses_nearly_everything_stays_finite() {
        assert_eq!(Losses::exposed(hours(100.0), 1.0), MAXIMUM_EXPOSED);
    }
}
