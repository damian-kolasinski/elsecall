//! The period a report covers, and the widths it compares members' load in.
use crate::{
    failure::{Failure, Result},
    scheduling::{records::RETENTION, timestamp::Timestamp},
};
use std::time::Duration;

const MINUTE_MS: u64 = 60_000;
/// The longest period a report covers: as long as members keep records and load.
pub const MAXIMUM_PERIOD: Duration = RETENTION;
/// At most this many fine buckets per member, so one member's answer fits a
/// member response.
pub const FINE_BUCKETS: u64 = 1000;
/// Series points per member when the caller does not choose a width.
pub const SERIES_POINTS: u64 = 24;

/// A report's failure on invalid input.
pub fn invalid(message: impl Into<String>) -> Failure {
    Failure::new(
        "invalid_request",
        message,
        "Run elsecall report --help for valid options.",
    )
}

/// From `since` to before `until`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub since: Timestamp,
    pub until: Timestamp,
}

/// Whole minutes, at least one, rounded up.
fn minutes(ms: u64) -> u64 {
    ms.div_ceil(MINUTE_MS).max(1) * MINUTE_MS
}

impl Window {
    /// A non-empty period of at most [`MAXIMUM_PERIOD`] (and a minute's slack,
    /// for clocks), ending no later than a minute past `now`.
    pub fn new(since: Timestamp, until: Timestamp, now: Timestamp) -> Result<Self> {
        let slack = MINUTE_MS;
        if since >= until
            || until.millis() > now.millis() + slack
            || until.millis() - since.millis() > MAXIMUM_PERIOD.as_millis() as u64 + slack
        {
            return Err(invalid(
                "The period must be in the past and span at most 30 days, as long as records and load history are kept.",
            ));
        }
        Ok(Self { since, until })
    }

    pub fn span(&self) -> Duration {
        self.until.since(self.since)
    }

    pub fn contains(&self, at: Timestamp) -> bool {
        at >= self.since && at < self.until
    }

    /// The width members' load is compared in: whole minutes, as narrow as
    /// possible within [`FINE_BUCKETS`] over the period.
    pub fn fine_width(&self) -> Duration {
        let span = self.span().as_millis() as u64;
        Duration::from_millis(minutes(span.div_ceil(FINE_BUCKETS)))
    }

    /// The width of each series point: `requested`, else a 24th of the period,
    /// rounded up to whole fine buckets.
    pub fn series_width(&self, requested: Option<Duration>) -> Duration {
        let fine = self.fine_width().as_millis() as u64;
        let wanted = requested.map_or_else(
            || (self.span().as_millis() as u64).div_ceil(SERIES_POINTS),
            |d| d.as_millis() as u64,
        );
        Duration::from_millis(wanted.div_ceil(fine).max(1) * fine)
    }

    /// Whether a bucket starting at `at` and `width` long overlaps the period.
    pub fn overlaps(&self, at: Timestamp, width: Duration) -> bool {
        at < self.until && at.after(width) > self.since
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400_000;

    fn window(since: u64, until: u64) -> Window {
        Window::new(
            Timestamp::from_millis(since),
            Timestamp::from_millis(until),
            Timestamp::from_millis(until),
        )
        .unwrap()
    }

    #[test]
    fn periods_are_in_the_past_and_within_retention() {
        let now = Timestamp::from_millis(40 * DAY);
        let at = Timestamp::from_millis;
        assert!(Window::new(at(10 * DAY), now, now).is_ok());
        for (since, until) in [(40 * DAY, 40 * DAY), (9 * DAY, 40 * DAY), (DAY, 41 * DAY)] {
            assert_eq!(
                Window::new(at(since), at(until), now).unwrap_err().code,
                "invalid_request"
            );
        }
    }

    #[test]
    fn fine_buckets_stay_within_a_thousand_and_series_align_to_them() {
        let hour = window(0, 3_600_000);
        assert_eq!(hour.fine_width(), Duration::from_secs(60));
        assert_eq!(hour.series_width(None), Duration::from_secs(180));
        let week = window(0, 7 * DAY);
        // 604,800 s over 1,000 buckets is 604.8 s: 11 whole minutes.
        assert_eq!(week.fine_width(), Duration::from_secs(660));
        // A 24th of a week is 7 h, which is 38.2 fine buckets: 39 of them.
        assert_eq!(week.series_width(None), Duration::from_secs(39 * 660));
        assert_eq!(
            week.series_width(Some(Duration::from_secs(60))),
            Duration::from_secs(660),
            "never narrower than a fine bucket"
        );
        assert!(week.overlaps(Timestamp::from_millis(0), Duration::from_secs(60)));
        assert!(!week.overlaps(Timestamp::from_millis(7 * DAY), Duration::from_secs(60)));
    }
}
