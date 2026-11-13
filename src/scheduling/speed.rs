//! How fast one core of this member runs work, measured rather than assumed.
//!
//! A size means the same number of cores on every member, so what differs between
//! members is how much work each core does. A meter measures that with a fixed
//! workload timed by the measuring thread's own CPU clock: time other processes
//! take from the core does not count, so a loaded member still measures its core,
//! not its load. Load is placement's separate input. A trial can still land on an
//! efficiency core or share a core's caches, which only ever makes it slower, so
//! the member's speed is the best of its recent measurements.
use crate::failure::Result;
use std::{collections::VecDeque, time::Duration};

/// Measures one core's speed. The unit is the meter's own; only ratios between
/// members measured by the same meter matter.
pub trait SpeedMeter: Send + Sync {
    /// Blocks while the workload runs, typically a fraction of a second.
    fn measure(&self) -> Result<f64>;
}

/// How often a member measures itself again.
pub const MEASURE_EVERY: Duration = Duration::from_secs(10 * 60);
/// Measurements the best is taken from: an hour's worth.
pub const KEPT: usize = 6;

/// The range of speeds a member may report: far beyond any real core either way.
pub const PLAUSIBLE: std::ops::RangeInclusive<f64> = 0.001..=1000.0;

/// Deserializes a speed another member reported, keeping it only when plausible,
/// so no estimate divides by zero or overflows a duration.
pub fn plausible<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<f64>, D::Error> {
    let speed = <Option<f64> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(speed.filter(|s| PLAUSIBLE.contains(s)))
}

/// A member's recent measurements.
#[derive(Debug, Default)]
pub struct SpeedHistory {
    measurements: VecDeque<f64>,
}

impl SpeedHistory {
    /// Keeps a measurement; nonsensical values are dropped.
    pub fn record(&mut self, speed: f64) {
        if !PLAUSIBLE.contains(&speed) {
            return;
        }
        if self.measurements.len() == KEPT {
            self.measurements.pop_front();
        }
        self.measurements.push_back(speed);
    }

    /// The member's speed: the best recent measurement, or `None` before the first.
    pub fn current(&self) -> Option<f64> {
        self.measurements.iter().copied().reduce(f64::max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implausible_reported_speeds_are_dropped() {
        #[derive(serde::Deserialize)]
        struct Reported {
            #[serde(default, deserialize_with = "plausible")]
            speed: Option<f64>,
        }
        let speed = |json: &str| serde_json::from_str::<Reported>(json).unwrap().speed;
        assert_eq!(speed(r#"{"speed":1.5}"#), Some(1.5));
        assert_eq!(speed(r#"{"speed":0}"#), None);
        assert_eq!(speed(r#"{"speed":-2}"#), None);
        assert_eq!(speed(r#"{"speed":1e-300}"#), None);
        assert_eq!(speed(r#"{"speed":1e300}"#), None);
        assert_eq!(speed(r#"{"speed":null}"#), None);
        assert_eq!(speed("{}"), None);
    }

    #[test]
    fn speed_is_the_best_of_the_recent_measurements() {
        let mut history = SpeedHistory::default();
        assert_eq!(history.current(), None);
        history.record(1.0);
        history.record(0.4);
        history.record(f64::NAN);
        history.record(-1.0);
        assert_eq!(history.current(), Some(1.0));
        for _ in 0..KEPT {
            history.record(0.8);
        }
        assert_eq!(history.current(), Some(0.8), "old measurements age out");
    }
}
