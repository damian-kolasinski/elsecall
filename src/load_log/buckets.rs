//! Minutes of load rolled into longer periods. Buckets start at whole multiples
//! of their width since the Unix epoch, so every member's buckets line up.
use crate::inspection::{MemoryPressure, ThermalState};
use crate::{
    load_log::minute::{Minute, memory_rank, thermal_rank, worst},
    scheduling::{capacity::Cores, timestamp::Timestamp},
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A period of load. Means are over the seconds observed, weighted by them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bucket {
    pub at: Timestamp,
    /// Observation count (nominally one per second), not elapsed wall seconds.
    /// Timer jitter can put more than 60 observations in a one-minute bucket.
    pub seconds: u64,
    pub cpu_mean: Option<f64>,
    pub cpu_max: Option<f64>,
    pub outside_mean: Option<f64>,
    pub weighted_cores: Cores,
    pub capacity: Cores,
    pub in_use_mean: f64,
    pub in_use_max: Cores,
    pub queued_max: usize,
    /// The most interactive requests queued; minutes from before it was kept
    /// count every queued request. Absent from members on earlier versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactive_queued_max: Option<usize>,
    #[serde(default)]
    pub busy_seconds: u64,
    pub saturated_seconds: u64,
    pub saturated_busy_seconds: u64,
    pub memory_pressure_seconds: u64,
    pub worst_memory_pressure: Option<MemoryPressure>,
    pub worst_thermal_state: Option<ThermalState>,
    pub speed_min: Option<f64>,
    pub speed_max: Option<f64>,
}

/// One bucket of `minutes`, all of which belong to it, starting at `at`.
pub fn fold(at: Timestamp, minutes: &[Minute]) -> Option<Bucket> {
    let seconds: u64 = minutes.iter().map(|m| u64::from(m.seconds)).sum();
    if seconds == 0 {
        return None;
    }
    let weighted_mean = |value: &dyn Fn(&Minute) -> Option<f64>| {
        let (sum, weight) = minutes
            .iter()
            .filter_map(|m| Some((value(m)? * f64::from(m.seconds), f64::from(m.seconds))))
            .fold((0.0, 0.0), |(s, w), (v, n)| (s + v, w + n));
        (weight > 0.0).then(|| sum / weight)
    };
    let max =
        |value: &dyn Fn(&Minute) -> Option<f64>| minutes.iter().filter_map(value).reduce(f64::max);
    let last = minutes.last()?;
    Some(Bucket {
        at,
        seconds,
        cpu_mean: weighted_mean(&|m| m.cpu_mean),
        cpu_max: max(&|m| m.cpu_max),
        outside_mean: weighted_mean(&|m| m.outside_mean),
        weighted_cores: last.weighted_cores,
        capacity: last.capacity,
        in_use_mean: weighted_mean(&|m| Some(m.in_use_mean)).unwrap_or(0.0),
        in_use_max: minutes
            .iter()
            .map(|m| m.in_use_max)
            .max()
            .unwrap_or(Cores::ZERO),
        queued_max: minutes.iter().map(|m| m.queued_max).max().unwrap_or(0),
        interactive_queued_max: minutes
            .iter()
            .map(|m| m.interactive_queued_max.unwrap_or(m.queued_max))
            .max(),
        busy_seconds: minutes.iter().map(|m| u64::from(m.busy_seconds)).sum(),
        saturated_seconds: minutes.iter().map(|m| u64::from(m.saturated_seconds)).sum(),
        saturated_busy_seconds: minutes
            .iter()
            .map(|m| u64::from(m.saturated_busy_seconds))
            .sum(),
        memory_pressure_seconds: minutes
            .iter()
            .map(|m| u64::from(m.memory_pressure_seconds))
            .sum(),
        worst_memory_pressure: minutes
            .iter()
            .fold(None, |w, m| worst(w, m.worst_memory_pressure, memory_rank)),
        worst_thermal_state: minutes
            .iter()
            .fold(None, |w, m| worst(w, m.worst_thermal_state, thermal_rank)),
        speed_min: minutes.iter().filter_map(|m| m.speed).reduce(f64::min),
        speed_max: max(&|m| m.speed),
    })
}

/// `minutes`, oldest first, in buckets of `width` (whole minutes).
pub fn buckets(minutes: &[Minute], width: Duration) -> Vec<Bucket> {
    let width = (width.as_millis() as u64).div_ceil(60_000).max(1) * 60_000;
    let mut out = Vec::new();
    let mut start = 0;
    while start < minutes.len() {
        let at = minutes[start].at.millis() / width * width;
        let end = minutes[start..]
            .iter()
            .position(|m| m.at.millis() / width * width != at)
            .map_or(minutes.len(), |n| start + n);
        out.extend(fold(Timestamp::from_millis(at), &minutes[start..end]));
        start = end;
    }
    out
}

/// A member's recent load, for the fleet view.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoadSummary {
    pub last_hour: Option<Bucket>,
    pub last_day: Option<Bucket>,
}

/// The fleet view's summary of `minutes`, the last day's, at `now`.
pub fn summary(minutes: &[Minute], now: Timestamp) -> LoadSummary {
    let within = |period: Duration| {
        let since = now.millis().saturating_sub(period.as_millis() as u64);
        let from = minutes.partition_point(|m| m.at.millis() < since);
        fold(Timestamp::from_millis(since), &minutes[from..])
    };
    LoadSummary {
        last_hour: within(Duration::from_secs(3600)),
        last_day: within(Duration::from_secs(86_400)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minute(at_min: u64, cpu: f64, in_use: f64, saturated: u32) -> Minute {
        Minute {
            at: Timestamp::from_millis(at_min * 60_000),
            seconds: 60,
            cpu_mean: Some(cpu),
            cpu_max: Some(cpu),
            outside_mean: Some(cpu / 2.0),
            weighted_cores: Cores::from_millis(10_000),
            capacity: Cores::from_millis(9_000),
            in_use_mean: in_use,
            in_use_max: Cores::from_f64(in_use).unwrap(),
            queued_max: 0,
            interactive_queued_max: Some(0),
            busy_seconds: if in_use > 0.0 { 60 } else { 0 },
            saturated_seconds: saturated,
            saturated_busy_seconds: 0,
            memory_pressure_seconds: 0,
            worst_memory_pressure: Some(MemoryPressure::Normal),
            worst_thermal_state: Some(ThermalState::Nominal),
            speed: Some(1.0 - at_min as f64 / 100.0),
        }
    }

    #[test]
    fn minutes_roll_into_aligned_buckets() {
        let minutes = [
            minute(58, 0.2, 0.0, 0),
            minute(59, 0.4, 2.0, 30),
            minute(60, 1.0, 4.0, 60),
            minute(64, 0.0, 0.0, 0),
        ];
        let five = buckets(&minutes, Duration::from_secs(300));
        assert_eq!(
            five.iter()
                .map(|b| b.at.millis() / 60_000)
                .collect::<Vec<_>>(),
            [55, 60]
        );
        assert_eq!(five[0].seconds, 120);
        assert!((five[0].cpu_mean.unwrap() - 0.3).abs() < 1e-9);
        assert_eq!(five[0].cpu_max, Some(0.4));
        assert!((five[0].in_use_mean - 1.0).abs() < 1e-9);
        assert_eq!(five[0].saturated_seconds, 30);
        assert_eq!(five[0].busy_seconds, 60);
        assert_eq!(five[1].in_use_max, Cores::from_millis(4000));
        // A minute from before interactive requests were counted apart counts
        // every queued request.
        let mut older = minutes.clone();
        older[0].queued_max = 2;
        older[0].interactive_queued_max = None;
        assert_eq!(
            buckets(&older, Duration::from_secs(300))[0].interactive_queued_max,
            Some(2)
        );
        assert_eq!(five[1].speed_min, Some(0.36));
        assert_eq!(five[1].speed_max, Some(0.4));
        let summary = summary(&minutes, Timestamp::from_millis(65 * 60_000));
        assert_eq!(summary.last_hour.unwrap().seconds, 240);
        assert!(fold(Timestamp::from_millis(0), &[]).is_none());
    }
}
