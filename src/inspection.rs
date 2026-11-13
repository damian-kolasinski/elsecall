use crate::failure::Result;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Machine {
    pub hostname: Option<String>,
    pub os: String,
    pub os_version: Option<String>,
    pub architecture: String,
    pub logical_cpus: usize,
    pub load_average: LoadAverage,
    pub memory: Memory,
    pub uptime_seconds: u64,
    #[serde(flatten)]
    pub load: LoadSample,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadAverage {
    pub one_minute: f64,
    pub five_minutes: f64,
    pub fifteen_minutes: f64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
}

/// Live load of the whole machine, including work Elsecall did not start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoadSample {
    pub cores: CoreCounts,
    /// Fraction of all logical CPUs busy over the latest sampling window, from 0 to 1.
    /// `None` until the inspector has two samples to compare.
    pub cpu_utilization: Option<f64>,
    pub memory_pressure: MemoryPressure,
    pub thermal_state: ThermalState,
}

/// Logical CPUs by core type. Machines without distinct core types report all of
/// them as performance cores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreCounts {
    pub performance: u32,
    pub efficiency: u32,
}

/// The operating system's own memory pressure verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPressure {
    Normal,
    Warning,
    Critical,
}

/// The operating system's thermal pressure level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThermalState {
    Nominal,
    Moderate,
    Heavy,
    Trapping,
    Sleeping,
}

/// Read-only machine facts. Implementations must not inspect processes or secrets.
pub trait MachineInspector: Send + Sync {
    fn inspect(&self) -> Result<Machine>;
    /// Sample live load. CPU utilization covers the time since an earlier sample,
    /// so callers that need a steady signal sample about once a second.
    fn sample_load(&self) -> Result<LoadSample>;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Health {
    pub status: String,
    pub version: String,
    /// The running build's ID; absent from services that predate upgrades.
    #[serde(default)]
    pub build: Option<crate::build_identity::BuildId>,
    pub pid: u32,
    pub uptime_seconds: u64,
}

pub struct Inspection {
    machine: Arc<dyn MachineInspector>,
    started: Instant,
}
impl Inspection {
    pub fn new(machine: Arc<dyn MachineInspector>) -> Self {
        Self {
            machine,
            started: Instant::now(),
        }
    }
    pub fn health(&self) -> Health {
        Health {
            status: "ready".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            build: crate::build_identity::this_build().ok().map(|b| b.build),
            pid: std::process::id(),
            uptime_seconds: self.started.elapsed().as_secs(),
        }
    }
    pub fn machine(&self) -> Result<Machine> {
        self.machine.inspect()
    }
}
