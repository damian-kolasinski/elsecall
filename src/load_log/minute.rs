//! One minute of a member's load, folded from the scheduler's per-second samples.
use crate::{
    inspection::{MemoryPressure, ThermalState},
    scheduling::{capacity::Cores, load::Gate, timestamp::Timestamp},
};
use serde::{Deserialize, Serialize};

const MINUTE_MS: u64 = 60_000;

/// What the scheduler saw in one second.
#[derive(Clone, Debug, PartialEq)]
pub struct Second {
    pub at: Timestamp,
    /// Whole-machine CPU utilization of the latest sample, 0 to 1.
    pub cpu_utilization: Option<f64>,
    pub memory_pressure: Option<MemoryPressure>,
    pub thermal_state: Option<ThermalState>,
    pub weighted_cores: Cores,
    pub capacity: Cores,
    /// Capacity Elsecall's grants hold.
    pub in_use: Cores,
    /// Requests waiting in this member's own queue.
    pub queued: usize,
    /// Of them, interactive requests.
    pub queued_interactive: usize,
    pub gate: Gate,
    pub speed: Option<f64>,
}

/// One minute of load. Shares are of the whole machine; cores are weighted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Minute {
    /// The minute's start.
    pub at: Timestamp,
    /// Observation count, nominally one per second; timer jitter can exceed 60.
    pub seconds: u32,
    pub cpu_mean: Option<f64>,
    pub cpu_max: Option<f64>,
    /// CPU utilization beyond the share Elsecall's grants hold: outside load.
    pub outside_mean: Option<f64>,
    pub weighted_cores: Cores,
    pub capacity: Cores,
    /// Cores Elsecall's grants held, on average and at most.
    pub in_use_mean: f64,
    pub in_use_max: Cores,
    pub queued_max: usize,
    /// The most interactive requests queued; absent in minutes from before it
    /// was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactive_queued_max: Option<usize>,
    /// Seconds Elsecall work held cores.
    #[serde(default)]
    pub busy_seconds: u32,
    /// Seconds the gate was closed by outside load.
    pub saturated_seconds: u32,
    /// Seconds Elsecall work held cores while the gate was closed by outside load.
    pub saturated_busy_seconds: u32,
    /// Seconds the gate was closed by memory pressure.
    pub memory_pressure_seconds: u32,
    pub worst_memory_pressure: Option<MemoryPressure>,
    pub worst_thermal_state: Option<ThermalState>,
    pub speed: Option<f64>,
}

pub fn memory_rank(level: MemoryPressure) -> u8 {
    match level {
        MemoryPressure::Normal => 0,
        MemoryPressure::Warning => 1,
        MemoryPressure::Critical => 2,
    }
}
pub fn thermal_rank(state: ThermalState) -> u8 {
    match state {
        ThermalState::Nominal => 0,
        ThermalState::Moderate => 1,
        ThermalState::Heavy => 2,
        ThermalState::Trapping => 3,
        ThermalState::Sleeping => 4,
    }
}

/// The worse of two optional levels by `rank`.
pub fn worst<T: Copy>(a: Option<T>, b: Option<T>, rank: fn(T) -> u8) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if rank(b) > rank(a) { b } else { a }),
        (a, b) => a.or(b),
    }
}

/// Folds seconds into minutes; a minute is done once a second of a later minute
/// arrives.
#[derive(Debug, Default)]
pub struct Accumulator {
    current: Option<Fold>,
}

#[derive(Debug)]
struct Fold {
    at: u64,
    seconds: u32,
    cpu_sum: f64,
    cpu_count: u32,
    cpu_max: Option<f64>,
    outside_sum: f64,
    in_use_sum: f64,
    last: Second,
    in_use_max: Cores,
    queued_max: usize,
    interactive_queued_max: usize,
    busy: u32,
    saturated: u32,
    saturated_busy: u32,
    memory: u32,
    worst_memory: Option<MemoryPressure>,
    worst_thermal: Option<ThermalState>,
}

impl Fold {
    fn new(second: Second) -> Self {
        let mut fold = Fold {
            at: second.at.millis() / MINUTE_MS * MINUTE_MS,
            seconds: 0,
            cpu_sum: 0.0,
            cpu_count: 0,
            cpu_max: None,
            outside_sum: 0.0,
            in_use_sum: 0.0,
            in_use_max: Cores::ZERO,
            queued_max: 0,
            interactive_queued_max: 0,
            busy: 0,
            saturated: 0,
            saturated_busy: 0,
            memory: 0,
            worst_memory: None,
            worst_thermal: None,
            last: second.clone(),
        };
        fold.add(second);
        fold
    }

    fn add(&mut self, second: Second) {
        self.seconds += 1;
        if let Some(cpu) = second.cpu_utilization {
            let weighted = second.weighted_cores.as_f64();
            let own = if weighted > 0.0 {
                second.in_use.as_f64() / weighted
            } else {
                0.0
            };
            self.cpu_sum += cpu;
            self.outside_sum += (cpu - own).max(0.0);
            self.cpu_count += 1;
            self.cpu_max = Some(self.cpu_max.map_or(cpu, |m| m.max(cpu)));
        }
        self.in_use_sum += second.in_use.as_f64();
        self.in_use_max = self.in_use_max.max(second.in_use);
        self.queued_max = self.queued_max.max(second.queued);
        self.interactive_queued_max = self.interactive_queued_max.max(second.queued_interactive);
        if second.in_use > Cores::ZERO {
            self.busy += 1;
        }
        match second.gate {
            Gate::Saturated { .. } => {
                self.saturated += 1;
                if second.in_use > Cores::ZERO {
                    self.saturated_busy += 1;
                }
            }
            Gate::MemoryPressure { .. } => self.memory += 1,
            Gate::Open | Gate::Unknown => (),
        }
        self.worst_memory = worst(self.worst_memory, second.memory_pressure, memory_rank);
        self.worst_thermal = worst(self.worst_thermal, second.thermal_state, thermal_rank);
        self.last = second;
    }

    fn minute(&self) -> Minute {
        let count = f64::from(self.cpu_count);
        Minute {
            at: Timestamp::from_millis(self.at),
            seconds: self.seconds,
            cpu_mean: (self.cpu_count > 0).then(|| self.cpu_sum / count),
            cpu_max: self.cpu_max,
            outside_mean: (self.cpu_count > 0).then(|| self.outside_sum / count),
            weighted_cores: self.last.weighted_cores,
            capacity: self.last.capacity,
            in_use_mean: self.in_use_sum / f64::from(self.seconds),
            in_use_max: self.in_use_max,
            queued_max: self.queued_max,
            interactive_queued_max: Some(self.interactive_queued_max),
            busy_seconds: self.busy,
            saturated_seconds: self.saturated,
            saturated_busy_seconds: self.saturated_busy,
            memory_pressure_seconds: self.memory,
            worst_memory_pressure: self.worst_memory,
            worst_thermal_state: self.worst_thermal,
            speed: self.last.speed,
        }
    }
}

impl Accumulator {
    /// The minute being folded, so far.
    pub fn current(&self) -> Option<Minute> {
        self.current.as_ref().map(Fold::minute)
    }

    /// Adds a second; returns the previous minute once `second` starts a new one.
    pub fn add(&mut self, second: Second) -> Option<Minute> {
        let minute = second.at.millis() / MINUTE_MS * MINUTE_MS;
        match &mut self.current {
            Some(fold) if fold.at == minute => {
                fold.add(second);
                None
            }
            // While the clock is behind a minute already folded, seconds are
            // dropped, so no stored minute is written over.
            Some(fold) if fold.at > minute => None,
            _ => self
                .current
                .replace(Fold::new(second))
                .map(|done| done.minute()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn second(at_s: u64, cpu: f64, in_use: f64, gate: Gate) -> Second {
        Second {
            at: Timestamp::from_millis(at_s * 1000),
            cpu_utilization: Some(cpu),
            memory_pressure: Some(MemoryPressure::Normal),
            thermal_state: Some(ThermalState::Nominal),
            weighted_cores: Cores::from_millis(10_000),
            capacity: Cores::from_millis(8_000),
            in_use: Cores::from_f64(in_use).unwrap(),
            queued: 0,
            queued_interactive: 0,
            gate,
            speed: Some(1.0),
        }
    }

    #[test]
    fn seconds_fold_into_their_minute() {
        let saturated = Gate::Saturated {
            cpu_utilization: 0.95,
            outside_utilization: 0.95,
        };
        let mut accumulator = Accumulator::default();
        assert_eq!(accumulator.add(second(120, 0.5, 0.0, Gate::Open)), None);
        assert_eq!(accumulator.add(second(121, 0.95, 0.0, saturated)), None);
        let mut busy = second(150, 1.0, 2.0, saturated);
        busy.queued = 3;
        busy.queued_interactive = 1;
        busy.memory_pressure = Some(MemoryPressure::Warning);
        busy.thermal_state = Some(ThermalState::Heavy);
        assert_eq!(accumulator.add(busy), None);
        assert_eq!(accumulator.current().unwrap().seconds, 3, "so far");
        let done = accumulator
            .add(second(180, 0.1, 0.0, Gate::Open))
            .expect("the next minute ends the last");
        assert_eq!(done.at, Timestamp::from_millis(120_000));
        assert_eq!(done.seconds, 3);
        assert_eq!(done.cpu_max, Some(1.0));
        assert!((done.cpu_mean.unwrap() - 2.45 / 3.0).abs() < 1e-9);
        // Outside load: the last second's 1.0 is 0.8 beyond its 2 of 10 cores.
        assert!((done.outside_mean.unwrap() - 2.25 / 3.0).abs() < 1e-9);
        assert!((done.in_use_mean - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(done.in_use_max, Cores::from_millis(2000));
        assert_eq!(done.queued_max, 3);
        assert_eq!(
            done.interactive_queued_max,
            Some(1),
            "deferrable work aside"
        );
        assert_eq!(done.busy_seconds, 1);
        assert_eq!(done.saturated_seconds, 2);
        assert_eq!(done.saturated_busy_seconds, 1);
        assert_eq!(done.worst_memory_pressure, Some(MemoryPressure::Warning));
        assert_eq!(done.worst_thermal_state, Some(ThermalState::Heavy));
        assert_eq!(
            accumulator.add(second(100, 0.1, 0.0, Gate::Open)),
            None,
            "a clock that went back writes nothing"
        );
        assert_eq!(
            accumulator.current().unwrap().seconds,
            1,
            "and leaves the minute in progress alone"
        );
        assert_eq!(accumulator.add(second(150, 0.1, 0.0, Gate::Open)), None);
        assert_eq!(
            accumulator
                .add(second(240, 0.1, 0.0, Gate::Open))
                .unwrap()
                .at,
            Timestamp::from_millis(180_000)
        );
    }
}
