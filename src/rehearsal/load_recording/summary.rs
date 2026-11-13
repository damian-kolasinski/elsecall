//! Time-weighted rehearsal observations, clipped to the recorded window. A real
//! one-second observation may straddle a UTC-minute boundary; it belongs to both
//! buckets. Repeated observations never create extra observed time.
use crate::{
    load_log::{
        buckets::Bucket,
        minute::{Second, memory_rank, thermal_rank, worst},
    },
    scheduling::{capacity::Cores, load::Gate, timestamp::Timestamp},
};
use std::collections::BTreeMap;
struct Fold {
    bucket: Bucket,
    millis: u64,
    cpu: f64,
    outside: f64,
    grants: f64,
    busy: u64,
    saturated: u64,
    saturated_busy: u64,
    memory: u64,
}
impl Fold {
    fn new(at: u64, s: &Second) -> Self {
        Self {
            bucket: Bucket {
                at: Timestamp::from_millis(at),
                seconds: 0,
                cpu_mean: None,
                cpu_max: None,
                outside_mean: None,
                weighted_cores: s.weighted_cores,
                capacity: s.capacity,
                in_use_mean: 0.0,
                in_use_max: Cores::ZERO,
                queued_max: 0,
                interactive_queued_max: Some(0),
                busy_seconds: 0,
                saturated_seconds: 0,
                saturated_busy_seconds: 0,
                memory_pressure_seconds: 0,
                worst_memory_pressure: None,
                worst_thermal_state: None,
                speed_min: None,
                speed_max: None,
            },
            millis: 0,
            cpu: 0.0,
            outside: 0.0,
            grants: 0.0,
            busy: 0,
            saturated: 0,
            saturated_busy: 0,
            memory: 0,
        }
    }
    fn add(&mut self, s: &Second, millis: u64) {
        let cpu = s
            .cpu_utilization
            .expect("only complete observations enter a fold");
        let speed = s.speed.expect("only complete observations enter a fold");
        let weight = millis as f64;
        self.millis += millis;
        self.cpu += cpu * weight;
        self.outside += (cpu - s.in_use.as_f64() / s.weighted_cores.as_f64()).max(0.0) * weight;
        self.grants += s.in_use.as_f64() * weight;
        let b = &mut self.bucket;
        b.cpu_max = Some(b.cpu_max.map_or(cpu, |value| value.max(cpu)));
        b.weighted_cores = s.weighted_cores;
        b.capacity = s.capacity;
        b.in_use_max = b.in_use_max.max(s.in_use);
        b.queued_max = b.queued_max.max(s.queued);
        b.interactive_queued_max = Some(
            b.interactive_queued_max
                .unwrap_or(0)
                .max(s.queued_interactive),
        );
        b.speed_min = Some(b.speed_min.map_or(speed, |value| value.min(speed)));
        b.speed_max = Some(b.speed_max.map_or(speed, |value| value.max(speed)));
        b.worst_memory_pressure = worst(b.worst_memory_pressure, s.memory_pressure, memory_rank);
        b.worst_thermal_state = worst(b.worst_thermal_state, s.thermal_state, thermal_rank);
        if s.in_use > Cores::ZERO {
            self.busy += millis;
        }
        match s.gate {
            Gate::Saturated { .. } => {
                self.saturated += millis;
                if s.in_use > Cores::ZERO {
                    self.saturated_busy += millis;
                }
            }
            Gate::MemoryPressure { .. } => self.memory += millis,
            Gate::Open | Gate::Unknown => (),
        }
    }
    fn finish(mut self, since: u64, until: u64) -> Bucket {
        let b = &mut self.bucket;
        let full = since <= b.at.millis() && until >= b.at.millis() + 60_000;
        b.seconds = if full {
            self.millis / 1000
        } else {
            self.millis.div_ceil(1000)
        };
        b.cpu_mean = Some(self.cpu / self.millis as f64);
        b.outside_mean = Some(self.outside / self.millis as f64);
        b.in_use_mean = self.grants / self.millis as f64;
        b.busy_seconds = self.busy.div_ceil(1000).min(b.seconds);
        b.saturated_seconds = self.saturated.div_ceil(1000).min(b.seconds);
        b.saturated_busy_seconds = self.saturated_busy.div_ceil(1000).min(b.seconds);
        b.memory_pressure_seconds = self.memory.div_ceil(1000).min(b.seconds);
        self.bucket
    }
}
pub(super) fn buckets(
    samples: &[&Second],
    since: Timestamp,
    until: Timestamp,
) -> (Vec<Bucket>, u64) {
    let mut folds = BTreeMap::new();
    let mut observed = 0;
    for (index, sample) in samples.iter().enumerate() {
        if sample.cpu_utilization.is_none() || sample.speed.is_none() {
            continue;
        }
        let next = samples
            .get(index + 1)
            .map_or(until.millis(), |s| s.at.millis());
        let end = next.min(until.millis()).min(sample.at.millis() + 1000);
        let mut at = sample.at.millis().max(since.millis());
        while at < end {
            let minute = at / 60_000 * 60_000;
            let to = end.min(minute + 60_000);
            folds
                .entry(minute)
                .or_insert_with(|| Fold::new(minute, sample))
                .add(sample, to - at);
            observed += to - at;
            at = to;
        }
    }
    (
        folds
            .into_values()
            .map(|f| f.finish(since.millis(), until.millis()))
            .collect(),
        observed,
    )
}
