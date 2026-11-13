//! Time estimates: default hold limits and when a busy member may have room.
use crate::scheduling::capacity::Cores;
use std::time::Duration;

/// A run should fit typical agent tool-call limits of about ten minutes.
pub const TOOL_CALL_BUDGET: Duration = Duration::from_secs(10 * 60);
pub const HOLD_LIMIT_FLOOR: Duration = Duration::from_secs(30);
/// Completed runs of a label used for its expected run time.
pub const HISTORY: usize = 20;
/// Suggested retry delay when running work gives no better estimate.
pub const UNKNOWN_RETRY: Duration = Duration::from_secs(60);
/// Suggested retry delay when capacity is free but load or the queue blocks.
pub const BLOCKED_RETRY: Duration = Duration::from_secs(30);
pub const MINIMUM_RETRY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HoldLimit {
    pub limit: Duration,
    pub basis: HoldBasis,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HoldBasis {
    /// The caller set the limit.
    Requested,
    /// Ten minutes minus the expected run time, at least 30 seconds.
    Default { expected_run: Option<Duration> },
}

pub fn hold_limit(requested: Option<Duration>, expected_run: Option<Duration>) -> HoldLimit {
    match requested {
        Some(limit) => HoldLimit {
            limit,
            basis: HoldBasis::Requested,
        },
        None => HoldLimit {
            limit: TOOL_CALL_BUDGET
                .saturating_sub(expected_run.unwrap_or_default())
                .max(HOLD_LIMIT_FLOOR),
            basis: HoldBasis::Default { expected_run },
        },
    }
}

/// The middle of recent run times, resistant to an occasional outlier.
pub fn expected_run(mut durations: Vec<Duration>) -> Option<Duration> {
    durations.sort();
    durations.get(durations.len() / 2).copied()
}

/// Work holding capacity, as retry estimates see it.
pub struct Holding {
    pub size: Cores,
    pub elapsed: Duration,
    pub expected_run: Option<Duration>,
}

/// When enough capacity for `needed` is likely free, assuming running work ends
/// at its expected run time. Falls back to fixed delays when that is unknown.
pub fn retry_after(free: Cores, needed: Cores, holding: &[Holding]) -> Duration {
    if needed <= free {
        return BLOCKED_RETRY;
    }
    let mut ends: Vec<(Duration, Cores)> = holding
        .iter()
        .filter_map(|h| Some((h.expected_run?.saturating_sub(h.elapsed), h.size)))
        .collect();
    ends.sort();
    let mut available = free;
    for (remaining, size) in ends {
        available = available.saturating_add(size);
        if available >= needed {
            return remaining.max(MINIMUM_RETRY);
        }
    }
    UNKNOWN_RETRY
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }
    fn cores(n: u32) -> Cores {
        Cores::from_millis(n * 1000)
    }

    #[test]
    fn default_hold_limit_leaves_room_for_the_run_with_a_floor() {
        assert_eq!(hold_limit(None, None).limit, secs(600));
        assert_eq!(hold_limit(None, Some(secs(240))).limit, secs(360));
        assert_eq!(hold_limit(None, Some(secs(590))).limit, secs(30));
        assert_eq!(hold_limit(None, Some(secs(3600))).limit, secs(30));
        let requested = hold_limit(Some(secs(2)), Some(secs(240)));
        assert_eq!(requested.limit, secs(2));
        assert_eq!(requested.basis, HoldBasis::Requested);
    }

    #[test]
    fn expected_run_is_the_median() {
        assert_eq!(expected_run(vec![]), None);
        assert_eq!(
            expected_run(vec![secs(40), secs(900), secs(41)]),
            Some(secs(41))
        );
    }

    #[test]
    fn retry_waits_for_enough_running_work_to_end() {
        let holding = [
            Holding {
                size: cores(2),
                elapsed: secs(10),
                expected_run: Some(secs(100)),
            },
            Holding {
                size: cores(1),
                elapsed: secs(10),
                expected_run: Some(secs(30)),
            },
            Holding {
                size: cores(4),
                elapsed: secs(0),
                expected_run: None,
            },
        ];
        assert_eq!(retry_after(cores(0), cores(1), &holding), secs(20));
        assert_eq!(retry_after(cores(0), cores(3), &holding), secs(90));
        assert_eq!(retry_after(cores(0), cores(5), &holding), UNKNOWN_RETRY);
        assert_eq!(retry_after(cores(2), cores(1), &holding), BLOCKED_RETRY);
    }
}
