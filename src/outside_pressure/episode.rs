//! When a closed gate becomes a blocked member, and when it stops being one.
//!
//! The gate closes at once on memory pressure and on three seconds of
//! saturation, and it can close briefly many times an hour: a link step, a
//! simulator boot, an indexing burst. A closure becomes a blocked state only once
//! it has lasted [`SUSTAINED`] without the gate staying open for [`REOPENED`],
//! and a blocked state ends only once the gate has stayed open for [`CLEARED`],
//! so neither a spike nor a moment's relief flips the status.
use crate::{
    outside_pressure::{
        attribution::Attribution,
        blocked::{Blocked, Reason},
    },
    scheduling::{load::Gate, timestamp::Timestamp},
};
use std::time::Duration;

/// How long the gate must stay closed before the member counts as blocked.
/// Interactive requests wait 90 s by default before they give up, and build
/// spikes that close the gate pass within a minute or two; five minutes is more
/// than three hold limits, so a blocked member has already failed or queued
/// real work, and it is still minutes, not hours, after the onset.
pub const SUSTAINED: Duration = Duration::from_secs(5 * 60);
/// How long the gate must stay open before a closure that is not yet a blocked
/// state ends. Saturation is averaged over three seconds, so the gate can
/// flicker open for a second or two inside a sustained closure; ten seconds open
/// admits waiting work, so a gate that keeps opening that long is not held shut.
pub const REOPENED: Duration = Duration::from_secs(10);
/// How long the gate must stay open before a blocked state ends.
pub const CLEARED: Duration = Duration::from_secs(60);
/// How often processes are sampled while the gate stays closed: the baseline for
/// CPU rates before [`SUSTAINED`], and fresh attribution after it.
pub const RESAMPLE: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Watch {
    #[default]
    Open,
    /// Closed, not yet for long enough or not yet attributed outside.
    Closed {
        since: Timestamp,
        last_closed: Timestamp,
        reason: Reason,
        sampled: Option<Timestamp>,
    },
    Blocked {
        blocked: Blocked,
        last_closed: Timestamp,
        /// When processes were last sampled, successfully or not.
        sampled: Timestamp,
    },
}

/// What the detector must do after an observation.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Nothing,
    /// Sample processes as a baseline for CPU rates.
    Sample,
    /// Sample processes and attribute the closure with [`Watch::attributed`].
    Attribute,
    /// The blocked state ended at `until`.
    Ended {
        blocked: Blocked,
        until: Timestamp,
    },
}

/// What attribution changed.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    Unchanged,
    Started(Blocked),
    Updated(Blocked),
    /// Elsecall's own work now explains the closure, so the outside episode ended.
    Ended {
        blocked: Blocked,
        until: Timestamp,
    },
}

impl Watch {
    pub fn blocked(&self) -> Option<&Blocked> {
        match self {
            Watch::Blocked { blocked, .. } => Some(blocked),
            Watch::Open | Watch::Closed { .. } => None,
        }
    }

    /// The watch after a service restart, given the episode the previous run
    /// left open: when the gate is still closed soon after that episode was last
    /// attributed, the closure keeps its onset, so a restart, such as a fleet
    /// upgrade, does not make a nine-hour block look five minutes old.
    pub fn resumed(previous: &Blocked, at: Timestamp, gate: Gate) -> Option<Watch> {
        let reason = Reason::of(gate)?;
        Self::resumable(previous, at).then_some(Watch::Closed {
            since: previous.since,
            last_closed: at,
            reason,
            sampled: Some(at),
        })
    }

    /// Whether an interrupted episode can still resume at `at`.
    pub fn resumable(previous: &Blocked, at: Timestamp) -> bool {
        at.since(previous.attributed_at) <= SUSTAINED
    }

    /// Takes in the gate at `at`.
    pub fn observe(&mut self, at: Timestamp, gate: Gate) -> Step {
        let due = |sampled: Option<Timestamp>| sampled.is_none_or(|s| at.since(s) >= RESAMPLE);
        match (Reason::of(gate), &mut *self) {
            (Some(reason), Watch::Open) => {
                *self = Watch::Closed {
                    since: at,
                    last_closed: at,
                    reason,
                    sampled: Some(at),
                };
                Step::Sample
            }
            (
                Some(now),
                Watch::Closed {
                    since,
                    last_closed,
                    reason,
                    sampled,
                },
            ) => {
                *last_closed = at;
                *reason = now;
                if !due(*sampled) {
                    Step::Nothing
                } else {
                    *sampled = Some(at);
                    if at.since(*since) >= SUSTAINED {
                        Step::Attribute
                    } else {
                        Step::Sample
                    }
                }
            }
            (
                Some(now),
                Watch::Blocked {
                    blocked,
                    last_closed,
                    sampled,
                },
            ) => {
                *last_closed = at;
                blocked.reason = now;
                if due(Some(*sampled)) {
                    *sampled = at;
                    Step::Attribute
                } else {
                    Step::Nothing
                }
            }
            (None, Watch::Open) => Step::Nothing,
            (None, Watch::Closed { last_closed, .. }) => {
                if at.since(*last_closed) >= REOPENED {
                    *self = Watch::Open;
                }
                Step::Nothing
            }
            (
                None,
                Watch::Blocked {
                    blocked,
                    last_closed,
                    ..
                },
            ) => {
                if at.since(*last_closed) < CLEARED {
                    return Step::Nothing;
                }
                let step = Step::Ended {
                    blocked: blocked.clone(),
                    until: *last_closed,
                };
                *self = Watch::Open;
                step
            }
        }
    }

    /// Takes in an attribution sampled at `at` after [`Step::Attribute`].
    pub fn attributed(&mut self, at: Timestamp, attribution: Attribution) -> Change {
        match (attribution, &mut *self) {
            (Attribution::Outside(outside), Watch::Closed { since, reason, .. }) => {
                let blocked = Blocked {
                    reason: reason.clone(),
                    since: *since,
                    attributed_at: at,
                    outside,
                };
                *self = Watch::Blocked {
                    blocked: blocked.clone(),
                    last_closed: at,
                    sampled: at,
                };
                Change::Started(blocked)
            }
            (Attribution::Outside(outside), Watch::Blocked { blocked, .. }) => {
                blocked.attributed_at = at;
                blocked.outside = outside;
                Change::Updated(blocked.clone())
            }
            (
                Attribution::OwnWork,
                Watch::Blocked {
                    blocked,
                    last_closed,
                    ..
                },
            ) => {
                let ended = Change::Ended {
                    blocked: blocked.clone(),
                    until: at,
                };
                *self = Watch::Closed {
                    since: at,
                    last_closed: *last_closed,
                    reason: blocked.reason.clone(),
                    sampled: Some(at),
                };
                ended
            }
            (_, Watch::Open) | (Attribution::OwnWork, Watch::Closed { .. }) => Change::Unchanged,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspection::MemoryPressure;

    const PRESSURE: Gate = Gate::MemoryPressure {
        level: MemoryPressure::Warning,
    };
    fn at(seconds: u64) -> Timestamp {
        Timestamp::from_millis(1_000_000_000 + seconds * 1000)
    }

    #[test]
    fn samples_a_baseline_then_attributes_once_sustained_and_clears_after_a_minute_open() {
        let mut watch = Watch::default();
        assert_eq!(watch.observe(at(0), PRESSURE), Step::Sample);
        assert_eq!(watch.observe(at(1), PRESSURE), Step::Nothing);
        assert_eq!(watch.observe(at(60), PRESSURE), Step::Sample);
        // A reopening shorter than ten seconds does not restart the closure.
        for s in 61..70 {
            assert_eq!(watch.observe(at(s), Gate::Open), Step::Nothing);
        }
        for s in (100..300).step_by(10) {
            watch.observe(at(s), PRESSURE);
        }
        assert_eq!(watch.observe(at(300), PRESSURE), Step::Attribute);
        assert_eq!(
            watch.attributed(at(300), Attribution::OwnWork),
            Change::Unchanged,
            "own work keeps it a plain closure"
        );
        assert_eq!(watch.observe(at(330), PRESSURE), Step::Nothing);
        assert_eq!(watch.observe(at(360), PRESSURE), Step::Attribute);
        let Change::Started(blocked) = watch.attributed(at(360), Attribution::Outside(vec![]))
        else {
            panic!("outside attribution blocks");
        };
        assert_eq!(blocked.since, at(0));
        assert_eq!(watch.blocked(), Some(&blocked));
        assert_eq!(watch.observe(at(400), PRESSURE), Step::Nothing);
        assert_eq!(watch.observe(at(420), PRESSURE), Step::Attribute);
        assert!(matches!(
            watch.attributed(at(420), Attribution::Outside(vec![])),
            Change::Updated(b) if b.attributed_at == at(420)
        ));
        assert_eq!(watch.observe(at(421), Gate::Unknown), Step::Nothing);
        assert_eq!(watch.observe(at(479), Gate::Open), Step::Nothing);
        let Step::Ended { until, .. } = watch.observe(at(480), Gate::Open) else {
            panic!("a minute open ends it");
        };
        assert_eq!(until, at(420));
        assert_eq!(watch, Watch::Open);
    }

    #[test]
    fn a_spike_shorter_than_the_threshold_never_attributes() {
        let mut watch = Watch::default();
        for s in 0..240 {
            assert_ne!(watch.observe(at(s), PRESSURE), Step::Attribute);
        }
        for s in 240..300 {
            watch.observe(at(s), Gate::Open);
        }
        assert_eq!(watch, Watch::Open);
        assert_eq!(
            watch.observe(at(300), PRESSURE),
            Step::Sample,
            "a new closure"
        );
    }

    #[test]
    fn a_gate_that_keeps_reopening_is_not_held_shut() {
        let mut watch = Watch::default();
        for cycle in 0..10 {
            let start = cycle * 65;
            for s in start..start + 10 {
                assert_ne!(watch.observe(at(s), PRESSURE), Step::Attribute);
            }
            for s in start + 10..start + 65 {
                watch.observe(at(s), Gate::Open);
            }
        }
        assert_eq!(watch, Watch::Open);
    }

    #[test]
    fn a_blocked_state_samples_once_a_minute_even_when_sampling_fails() {
        let mut watch = Watch::default();
        watch.observe(at(0), PRESSURE);
        assert_eq!(watch.observe(at(300), PRESSURE), Step::Attribute);
        watch.attributed(at(300), Attribution::Outside(vec![]));
        assert_eq!(watch.observe(at(360), PRESSURE), Step::Attribute);
        // The sample failed, so nothing was attributed.
        for s in 361..420 {
            assert_eq!(watch.observe(at(s), PRESSURE), Step::Nothing);
        }
        assert_eq!(watch.observe(at(420), PRESSURE), Step::Attribute);
    }

    #[test]
    fn a_restart_keeps_the_onset_of_a_block_that_still_lasts() {
        let mut watch = Watch::default();
        watch.observe(at(0), PRESSURE);
        watch.observe(at(300), PRESSURE);
        let Change::Started(previous) = watch.attributed(at(300), Attribution::Outside(vec![]))
        else {
            panic!("blocked");
        };
        assert_eq!(Watch::resumed(&previous, at(330), Gate::Open), None);
        assert_eq!(
            Watch::resumed(&previous, at(300 + 301), PRESSURE),
            None,
            "too long after its last attribution"
        );
        let mut resumed = Watch::resumed(&previous, at(330), PRESSURE).unwrap();
        assert_eq!(resumed.observe(at(331), PRESSURE), Step::Nothing);
        assert_eq!(resumed.observe(at(390), PRESSURE), Step::Attribute);
        let Change::Started(again) = resumed.attributed(at(390), Attribution::Outside(vec![]))
        else {
            panic!("blocked again");
        };
        assert_eq!(again.since, at(0));
    }

    #[test]
    fn own_work_taking_over_ends_the_outside_episode() {
        let mut watch = Watch::default();
        watch.observe(at(0), PRESSURE);
        assert_eq!(watch.observe(at(300), PRESSURE), Step::Attribute);
        watch.attributed(at(300), Attribution::Outside(vec![]));
        assert_eq!(watch.observe(at(360), PRESSURE), Step::Attribute);
        assert!(matches!(
            watch.attributed(at(360), Attribution::OwnWork),
            Change::Ended { until, .. } if until == at(360)
        ));
        assert_eq!(watch.blocked(), None);
    }
}
