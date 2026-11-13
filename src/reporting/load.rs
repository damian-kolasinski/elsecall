//! Metrics of members' load history: utilization over the period and over time,
//! work that ran while outside load saturated a member, measured speed, and how
//! evenly busy the members were.
use crate::{
    inspection::{MemoryPressure, ThermalState},
    load_log::{
        buckets::Bucket,
        minute::{memory_rank, thermal_rank, worst},
    },
    reporting::statistics::{mean, nearest_rank, sorted},
    scheduling::timestamp::Timestamp,
};
use serde::Serialize;
use std::{collections::BTreeMap, time::Duration};

/// A member's load over the whole period.
#[derive(Debug, Serialize, PartialEq)]
pub struct LoadTotals {
    pub sampled_seconds: u64,
    pub cpu_mean: Option<f64>,
    /// Nearest-rank 95th percentile of the fine buckets' mean CPU.
    pub cpu_p95: Option<f64>,
    pub outside_mean: Option<f64>,
    /// Mean share of weighted cores Elsecall's grants held.
    pub elsecall_share: f64,
    pub queued_max: usize,
    /// Seconds Elsecall work held cores.
    pub busy_seconds: u64,
    /// Seconds outside load closed the gate, and their share of the sampled ones.
    pub saturated_seconds: u64,
    pub saturated_share: f64,
    /// Seconds Elsecall work held cores while outside load closed the gate, and
    /// their share of `busy_seconds`.
    pub saturated_busy_seconds: u64,
    pub saturated_busy_share: Option<f64>,
    pub memory_pressure_seconds: u64,
    pub worst_memory_pressure: Option<MemoryPressure>,
    pub worst_thermal_state: Option<ThermalState>,
}

fn weighted_mean(buckets: &[Bucket], value: impl Fn(&Bucket) -> Option<f64>) -> Option<f64> {
    let (sum, weight) = buckets
        .iter()
        .filter_map(|b| Some((value(b)? * b.seconds as f64, b.seconds as f64)))
        .fold((0.0, 0.0), |(s, w), (v, n)| (s + v, w + n));
    (weight > 0.0).then(|| sum / weight)
}

fn elsecall_share(bucket: &Bucket) -> f64 {
    let weighted = bucket.weighted_cores.as_f64();
    if weighted > 0.0 {
        bucket.in_use_mean / weighted
    } else {
        0.0
    }
}

pub fn totals(buckets: &[Bucket]) -> Option<LoadTotals> {
    let sampled: u64 = buckets.iter().map(|b| b.seconds).sum();
    if sampled == 0 {
        return None;
    }
    let sum = |value: fn(&Bucket) -> u64| buckets.iter().map(value).sum::<u64>();
    let busy = sum(|b| b.busy_seconds);
    let saturated = sum(|b| b.saturated_seconds);
    let saturated_busy = sum(|b| b.saturated_busy_seconds);
    Some(LoadTotals {
        sampled_seconds: sampled,
        cpu_mean: weighted_mean(buckets, |b| b.cpu_mean),
        cpu_p95: nearest_rank(
            &sorted(buckets.iter().filter_map(|b| b.cpu_mean).collect()),
            0.95,
        ),
        outside_mean: weighted_mean(buckets, |b| b.outside_mean),
        elsecall_share: weighted_mean(buckets, |b| Some(elsecall_share(b))).unwrap_or(0.0),
        queued_max: buckets.iter().map(|b| b.queued_max).max().unwrap_or(0),
        busy_seconds: busy,
        saturated_seconds: saturated,
        saturated_share: saturated as f64 / sampled as f64,
        saturated_busy_seconds: saturated_busy,
        saturated_busy_share: (busy > 0).then(|| saturated_busy as f64 / busy as f64),
        memory_pressure_seconds: sum(|b| b.memory_pressure_seconds),
        worst_memory_pressure: buckets
            .iter()
            .fold(None, |w, b| worst(w, b.worst_memory_pressure, memory_rank)),
        worst_thermal_state: buckets
            .iter()
            .fold(None, |w, b| worst(w, b.worst_thermal_state, thermal_rank)),
    })
}

/// How a member's measured speed moved over the period.
#[derive(Debug, Serialize, PartialEq)]
pub struct SpeedDrift {
    /// The first and last fine buckets' speed, each the midpoint of its range.
    pub first: f64,
    pub last: f64,
    pub min: f64,
    pub max: f64,
    /// (max − min) / max.
    pub drift: f64,
}

pub fn speed_drift(buckets: &[Bucket]) -> Option<SpeedDrift> {
    let measured: Vec<&Bucket> = buckets
        .iter()
        .filter(|b| b.speed_min.is_some() && b.speed_max.is_some())
        .collect();
    let min = measured
        .iter()
        .filter_map(|b| b.speed_min)
        .reduce(f64::min)?;
    let max = measured
        .iter()
        .filter_map(|b| b.speed_max)
        .reduce(f64::max)?;
    Some(SpeedDrift {
        first: midpoint(measured.first()?)?,
        last: midpoint(measured.last()?)?,
        min,
        max,
        drift: if max > 0.0 { (max - min) / max } else { 0.0 },
    })
}

fn midpoint(bucket: &Bucket) -> Option<f64> {
    Some((bucket.speed_min? + bucket.speed_max?) / 2.0)
}

/// The member's speed over the period: the mean of its buckets' midpoints.
pub fn mean_speed(buckets: &[Bucket]) -> Option<f64> {
    mean(&buckets.iter().filter_map(midpoint).collect::<Vec<_>>())
}

/// The most interactive requests queued in `bucket`; buckets from members on
/// earlier versions count every queued request.
fn interactive_queued(bucket: &Bucket) -> usize {
    bucket.interactive_queued_max.unwrap_or(bucket.queued_max)
}

/// A member's load in one series point.
#[derive(Debug, Serialize, PartialEq)]
pub struct SeriesPoint {
    pub at: Timestamp,
    pub sampled_seconds: u64,
    pub cpu_mean: Option<f64>,
    pub outside_mean: Option<f64>,
    pub elsecall_share: f64,
    pub queued_max: usize,
    pub busy_seconds: u64,
    pub saturated_seconds: u64,
    pub speed_min: Option<f64>,
    pub speed_max: Option<f64>,
}

/// Fine `buckets` grouped into points of `width`, a multiple of theirs, aligned
/// to the epoch as the buckets are.
pub fn series(buckets: &[Bucket], width: Duration) -> Vec<SeriesPoint> {
    let width = (width.as_millis() as u64).max(1);
    let mut groups: BTreeMap<u64, Vec<Bucket>> = BTreeMap::new();
    for bucket in buckets {
        groups
            .entry(bucket.at.millis() / width * width)
            .or_default()
            .push(bucket.clone());
    }
    groups
        .into_iter()
        .filter_map(|(at, group)| {
            let totals = totals(&group)?;
            Some(SeriesPoint {
                at: Timestamp::from_millis(at),
                sampled_seconds: totals.sampled_seconds,
                cpu_mean: totals.cpu_mean,
                outside_mean: totals.outside_mean,
                elsecall_share: totals.elsecall_share,
                queued_max: totals.queued_max,
                busy_seconds: totals.busy_seconds,
                saturated_seconds: totals.saturated_seconds,
                speed_min: group.iter().filter_map(|b| b.speed_min).reduce(f64::min),
                speed_max: group.iter().filter_map(|b| b.speed_max).reduce(f64::max),
            })
        })
        .collect()
}

/// CPU at or above this on one member…
pub const BUSY_CPU: f64 = 0.8;
/// …while another is at or below this is an imbalanced period.
pub const IDLE_CPU: f64 = 0.3;

/// How evenly busy the members were, compared fine bucket by fine bucket over
/// the periods in which at least two members sampled.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Balance {
    pub periods: usize,
    /// Mean over periods of the gap between the busiest and idlest member's CPU.
    pub cpu_spread_mean: Option<f64>,
    /// Share of periods with one member at [`BUSY_CPU`] or more and another at
    /// [`IDLE_CPU`] or less.
    pub imbalanced_share: Option<f64>,
    /// The member with the highest mean measured speed.
    pub fastest_member: Option<String>,
    /// Sampled seconds of periods in which the fastest member held no Elsecall
    /// work and its gate was open, while another member had interactive
    /// requests queued. Deferrable work, such as housekeeping, waits by design.
    pub fastest_idle_while_queued_seconds: u64,
}

/// The balance of `loads`, each member's fine buckets, all of one width.
pub fn balance(loads: &[(&str, &[Bucket])]) -> Balance {
    let fastest = loads
        .iter()
        .filter_map(|(member, buckets)| Some((*member, mean_speed(buckets)?)))
        .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(a.0)))
        .map(|(member, _)| member.to_owned());
    let mut periods: BTreeMap<u64, Vec<(&str, &Bucket)>> = BTreeMap::new();
    for (member, buckets) in loads {
        for bucket in *buckets {
            periods
                .entry(bucket.at.millis())
                .or_default()
                .push((member, bucket));
        }
    }
    let mut balance = Balance {
        fastest_member: fastest.clone(),
        ..Balance::default()
    };
    let mut spreads = Vec::new();
    let mut imbalanced = 0;
    for period in periods.values().filter(|p| p.len() >= 2) {
        let cpus: Vec<f64> = period.iter().filter_map(|(_, b)| b.cpu_mean).collect();
        if cpus.len() >= 2 {
            let high = cpus.iter().copied().fold(f64::MIN, f64::max);
            let low = cpus.iter().copied().fold(f64::MAX, f64::min);
            spreads.push(high - low);
            imbalanced += usize::from(high >= BUSY_CPU && low <= IDLE_CPU);
        }
        let Some(fastest) = &fastest else { continue };
        let Some((_, own)) = period.iter().find(|(m, _)| *m == fastest.as_str()) else {
            continue;
        };
        let idle =
            own.busy_seconds == 0 && own.saturated_seconds == 0 && own.memory_pressure_seconds == 0;
        let others_queued = period
            .iter()
            .any(|(m, b)| *m != fastest.as_str() && interactive_queued(b) > 0);
        if idle && others_queued {
            balance.fastest_idle_while_queued_seconds += own.seconds;
        }
    }
    balance.periods = spreads.len();
    balance.cpu_spread_mean = mean(&spreads);
    balance.imbalanced_share =
        (!spreads.is_empty()).then(|| imbalanced as f64 / spreads.len() as f64);
    balance
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::fixtures::{MINUTE, T0, bucket};

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// The devbox (speed 1.2) idles while the mini (speed 1.0) is busy and
    /// queues; then the devbox works while saturated, then both are idle.
    fn devbox() -> Vec<Bucket> {
        vec![
            bucket(0, 0.1, 0.0, 0, 0, 1.2),
            bucket(1, 0.1, 0.0, 0, 0, 1.2),
            bucket(2, 0.95, 4.0, 0, 30, 1.0),
            bucket(3, 0.2, 0.0, 0, 0, 1.2),
        ]
    }

    fn mini() -> Vec<Bucket> {
        vec![
            bucket(0, 0.9, 8.0, 2, 0, 1.0),
            bucket(1, 0.9, 8.0, 1, 0, 1.0),
            bucket(2, 0.5, 2.0, 0, 0, 1.0),
            bucket(3, 0.2, 0.0, 0, 0, 1.0),
        ]
    }

    #[test]
    fn totals_weight_by_sampled_seconds() {
        let mut buckets = devbox();
        buckets[3].seconds = 30;
        let totals = totals(&buckets).unwrap();
        assert_eq!(totals.sampled_seconds, 210);
        // (0.1×60 + 0.1×60 + 0.95×60 + 0.2×30) / 210
        assert!(close(totals.cpu_mean.unwrap(), 75.0 / 210.0));
        assert_eq!(totals.cpu_p95, Some(0.95));
        assert!(close(totals.elsecall_share, 0.4 * 60.0 / 210.0));
        assert_eq!(totals.busy_seconds, 60);
        assert_eq!(totals.saturated_seconds, 30);
        assert!(close(totals.saturated_share, 30.0 / 210.0));
        assert_eq!(totals.saturated_busy_seconds, 30);
        assert_eq!(totals.saturated_busy_share, Some(0.5));
        assert_eq!(totals.worst_thermal_state, Some(ThermalState::Nominal));
        assert_eq!(totals.queued_max, 0);
        assert_eq!(super::totals(&[]), None);
    }

    #[test]
    fn speed_drift_is_the_range_over_the_highest() {
        let drift = speed_drift(&devbox()).unwrap();
        assert_eq!((drift.first, drift.last), (1.2, 1.2));
        assert_eq!((drift.min, drift.max), (1.0, 1.2));
        assert!(close(drift.drift, 0.2 / 1.2));
        assert!(close(mean_speed(&devbox()).unwrap(), 4.6 / 4.0));
        let mut unmeasured = devbox();
        for b in &mut unmeasured {
            b.speed_min = None;
            b.speed_max = None;
        }
        assert_eq!(speed_drift(&unmeasured), None);
    }

    #[test]
    fn series_points_group_fine_buckets() {
        let points = series(&mini(), Duration::from_secs(120));
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].at.millis(), T0);
        assert_eq!(points[1].at.millis(), T0 + 2 * MINUTE);
        assert_eq!(points[0].queued_max, 2);
        assert!(close(points[0].elsecall_share, 0.8));
        assert!(close(points[1].cpu_mean.unwrap(), 0.35));
        assert_eq!(points[1].busy_seconds, 60);
        assert_eq!(points[0].speed_max, Some(1.0));
    }

    #[test]
    fn balance_compares_members_period_by_period() {
        let (devbox, mini) = (devbox(), mini());
        let balance = balance(&[("devbox", &devbox), ("mini", &mini)]);
        assert_eq!(balance.periods, 4);
        // Spreads 0.8, 0.8, 0.45, 0.
        assert!(close(balance.cpu_spread_mean.unwrap(), 2.05 / 4.0));
        assert_eq!(balance.imbalanced_share, Some(0.5));
        assert_eq!(balance.fastest_member.as_deref(), Some("devbox"));
        assert_eq!(balance.fastest_idle_while_queued_seconds, 120);

        // Only deferrable work queued on the mini: the devbox idles by design.
        let deferrable: Vec<Bucket> = mini
            .iter()
            .map(|b| Bucket {
                interactive_queued_max: Some(0),
                ..b.clone()
            })
            .collect();
        let quiet = super::balance(&[("devbox", &devbox), ("mini", &deferrable)]);
        assert_eq!(quiet.fastest_idle_while_queued_seconds, 0);
        let older: Vec<Bucket> = mini
            .iter()
            .map(|b| Bucket {
                interactive_queued_max: None,
                ..b.clone()
            })
            .collect();
        let older = super::balance(&[("devbox", &devbox), ("mini", &older)]);
        assert_eq!(
            older.fastest_idle_while_queued_seconds, 120,
            "every request counts"
        );

        let alone = super::balance(&[("mini", &mini)]);
        assert_eq!(alone.periods, 0);
        assert_eq!(alone.cpu_spread_mean, None);
        assert_eq!(alone.fastest_idle_while_queued_seconds, 0);
    }
}
