//! What outside load lets admission do: the verdict it applies to new work,
//! which the load log records every minute.
use crate::inspection::MemoryPressure;
use serde::{Deserialize, Serialize};

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
