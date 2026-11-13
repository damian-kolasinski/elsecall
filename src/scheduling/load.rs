//! Whether outside load lets a member admit new work.
use crate::{
    inspection::{LoadSample, MemoryPressure},
    scheduling::{capacity::Cores, parameters::Admission},
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Samples averaged for saturation; at one sample a second, about three seconds.
pub const WINDOW: usize = 3;

/// Recent samples of one member's live load.
#[derive(Debug, Default)]
pub struct LoadHistory {
    samples: VecDeque<LoadSample>,
    unavailable: bool,
}

/// The verdict admission applies to new, non-nested work.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Gate {
    Open,
    /// The latest sample could not be read; admission falls back to capacity alone.
    Unknown,
    Saturated {
        /// Measured utilization of the whole machine.
        cpu_utilization: f64,
        /// The part attributed to work Elsecall did not start.
        outside_utilization: f64,
    },
    MemoryPressure {
        level: MemoryPressure,
    },
}
impl Gate {
    pub fn admits(self) -> bool {
        matches!(self, Gate::Open | Gate::Unknown)
    }
}

impl LoadHistory {
    pub fn record(&mut self, sample: LoadSample) {
        if self.samples.len() == WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
        self.unavailable = false;
    }
    pub fn record_unavailable(&mut self) {
        self.unavailable = true;
    }
    pub fn latest(&self) -> Option<&LoadSample> {
        self.samples.back().filter(|_| !self.unavailable)
    }
    /// Average utilization over the window, ignoring samples without one.
    pub fn cpu_utilization(&self) -> Option<f64> {
        let known: Vec<f64> = self
            .samples
            .iter()
            .filter_map(|s| s.cpu_utilization)
            .collect();
        (!known.is_empty()).then(|| known.iter().sum::<f64>() / known.len() as f64)
    }
    /// The verdict, given the fraction of the machine's weighted cores that
    /// Elsecall's own grants hold. Utilization cannot be attributed to processes
    /// without inspecting them, so Elsecall's work is assumed to use what it was
    /// granted and the rest counts as outside load.
    pub fn gate(&self, own_share: f64, parameters: &Admission) -> Gate {
        match self.latest() {
            None => Gate::Unknown,
            Some(latest) => gate(
                latest.memory_pressure,
                self.cpu_utilization(),
                own_share,
                parameters,
            ),
        }
    }
}

/// The share of a member's weighted cores that Elsecall's grants hold.
pub fn own_share(in_use: Cores, weighted_cores: Cores) -> f64 {
    if weighted_cores.millis() == 0 {
        0.0
    } else {
        in_use.as_f64() / weighted_cores.as_f64()
    }
}

/// The gate of a member whose state was recorded: its latest sample's memory
/// pressure, the window's `utilization` and the grants in use, under
/// `parameters`. This is the verdict the member's own service reached when it
/// used `parameters`; without a latest sample it is unknown.
pub fn gate_of(
    load: Option<&LoadSample>,
    utilization: Option<f64>,
    in_use: Cores,
    weighted_cores: Cores,
    parameters: &Admission,
) -> Gate {
    match load {
        None => Gate::Unknown,
        Some(latest) => gate(
            latest.memory_pressure,
            utilization,
            own_share(in_use, weighted_cores),
            parameters,
        ),
    }
}

/// The verdict for a member whose latest sample shows `memory_pressure`, whose
/// window averages `cpu_utilization`, and whose grants hold `own_share` of it.
pub fn gate(
    memory_pressure: MemoryPressure,
    cpu_utilization: Option<f64>,
    own_share: f64,
    parameters: &Admission,
) -> Gate {
    if parameters.memory_pressure.blocks(memory_pressure) {
        return Gate::MemoryPressure {
            level: memory_pressure,
        };
    }
    match cpu_utilization {
        Some(cpu_utilization)
            if cpu_utilization - own_share >= parameters.saturated_utilization =>
        {
            Gate::Saturated {
                cpu_utilization,
                outside_utilization: cpu_utilization - own_share,
            }
        }
        _ => Gate::Open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        inspection::{CoreCounts, ThermalState},
        scheduling::parameters::{BlockingPressure, Parameters},
    };

    fn sample(cpu: Option<f64>, memory: MemoryPressure) -> LoadSample {
        LoadSample {
            cores: CoreCounts {
                performance: 10,
                efficiency: 4,
            },
            cpu_utilization: cpu,
            memory_pressure: memory,
            thermal_state: ThermalState::Nominal,
        }
    }

    #[test]
    fn saturation_uses_the_window_average_and_memory_pressure_blocks_at_once() {
        let defaults = Parameters::default().admission;
        let mut history = LoadHistory::default();
        assert_eq!(history.gate(0.0, &defaults), Gate::Unknown);
        history.record(sample(None, MemoryPressure::Normal));
        assert_eq!(
            history.gate(0.0, &defaults),
            Gate::Open,
            "first sample has no utilization"
        );
        history.record(sample(Some(1.0), MemoryPressure::Normal));
        assert!(matches!(
            history.gate(0.0, &defaults),
            Gate::Saturated { .. }
        ));
        history.record(sample(Some(0.7), MemoryPressure::Normal));
        assert_eq!(
            history.gate(0.0, &defaults),
            Gate::Open,
            "average 0.85 is below 0.9"
        );
        for _ in 0..WINDOW {
            history.record(sample(Some(0.95), MemoryPressure::Normal));
        }
        assert!(!history.gate(0.0, &defaults).admits());
        assert_eq!(
            history.gate(0.1, &defaults),
            Gate::Open,
            "own work explains the rest"
        );
        history.record(sample(Some(0.0), MemoryPressure::Warning));
        assert_eq!(
            history.gate(0.0, &defaults),
            Gate::MemoryPressure {
                level: MemoryPressure::Warning
            }
        );
        let tolerant = Admission {
            saturated_utilization: 0.99,
            memory_pressure: BlockingPressure::Critical,
        };
        assert_eq!(
            history.gate(0.0, &tolerant),
            Gate::Open,
            "warning pressure passes when only critical blocks"
        );
        for _ in 0..WINDOW {
            history.record(sample(Some(0.95), MemoryPressure::Normal));
        }
        assert!(matches!(
            history.gate(0.0, &defaults),
            Gate::Saturated { .. }
        ));
        assert_eq!(history.gate(0.0, &tolerant), Gate::Open);
        history.record_unavailable();
        assert_eq!(history.gate(0.0, &defaults), Gate::Unknown);
        assert!(history.gate(0.0, &defaults).admits());
    }
}
