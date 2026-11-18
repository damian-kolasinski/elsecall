use crate::failure::Result;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Machine {
    pub hostname: Option<String>,
    /// The machine's Bonjour name on the local network, without `.local`; absent
    /// from members whose build predates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_host_name: Option<String>,
    /// Its addresses on its local networks, without loopback, link-local and
    /// tailnet ones; absent from members whose build predates them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lan_addresses: Option<Vec<std::net::IpAddr>>,
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
    /// The kernel's one-minute load average: threads running or waiting for a
    /// CPU. Unlike utilization it keeps rising past a full machine, so it tells
    /// a busy machine from an overloaded one. Absent from earlier members.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_one_minute: Option<f64>,
}

impl CoreCounts {
    /// Logical CPUs of every type.
    pub fn logical(self) -> u32 {
        self.performance.saturating_add(self.efficiency)
    }
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

#[cfg(test)]
impl Machine {
    /// A machine with plausible values, as an inspection reports them.
    pub(crate) fn fixture() -> Self {
        Machine {
            hostname: None,
            local_host_name: None,
            lan_addresses: None,
            os: "macos".into(),
            os_version: None,
            architecture: "aarch64".into(),
            logical_cpus: 14,
            load_average: LoadAverage {
                one_minute: 213.0,
                five_minutes: 202.5,
                fifteen_minutes: 161.9,
            },
            memory: Memory {
                total_bytes: 64,
                used_bytes: 32,
                available_bytes: 32,
            },
            uptime_seconds: 1,
            load: LoadSample {
                cores: CoreCounts {
                    performance: 10,
                    efficiency: 4,
                },
                cpu_utilization: Some(1.0),
                memory_pressure: MemoryPressure::Normal,
                thermal_state: ThermalState::Nominal,
                load_one_minute: Some(213.0),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_keeps_its_load_averages_beside_the_flattened_sample() {
        let machine = Machine::fixture();
        let json = serde_json::to_value(&machine).unwrap();
        assert_eq!(json["load_average"]["one_minute"], 213.0, "{json}");
        assert_eq!(json["load_one_minute"], 213.0);
        let back: Machine = serde_json::from_value(json).unwrap();
        assert_eq!(back.load, machine.load);
        assert_eq!(back.load_average.fifteen_minutes, 161.9);
    }
}
