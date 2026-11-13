//! Scripted machine observations for services that tests start on one shared
//! machine, where several members must look different and change on cue: their
//! load comes from a file the test rewrites, and their speed is fixed. Selected
//! only by `ELSECALL_MACHINE_LOAD=file:PATH` and `ELSECALL_MACHINE_SPEED`, like the
//! tailnet discovery overrides.
use crate::{
    failure::Result,
    inspection::{CoreCounts, LoadSample, Machine, MachineInspector, MemoryPressure, ThermalState},
    scheduling::speed::SpeedMeter,
};
use serde::Deserialize;
use std::path::PathBuf;

/// The load file named by the environment, if any.
pub fn load_file() -> Option<PathBuf> {
    std::env::var("ELSECALL_MACHINE_LOAD")
        .ok()?
        .strip_prefix("file:")
        .map(PathBuf::from)
}

/// The fixed speed named by the environment, if any.
pub fn speed() -> Option<FixedSpeed> {
    std::env::var("ELSECALL_MACHINE_SPEED")
        .ok()?
        .parse()
        .ok()
        .filter(|s: &f64| s.is_finite() && *s > 0.0)
        .map(FixedSpeed)
}

/// What the load file says; anything left out is idle.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Script {
    cpu_utilization: Option<f64>,
    memory_pressure: Option<MemoryPressure>,
    performance_cores: Option<u32>,
    efficiency_cores: Option<u32>,
}

/// A machine whose load and core counts come from a file, read on every sample.
pub struct ScriptedLoad<M> {
    pub machine: M,
    pub file: PathBuf,
}

impl<M: MachineInspector> ScriptedLoad<M> {
    fn scripted(&self, mut sample: LoadSample) -> LoadSample {
        let script: Script = std::fs::read(&self.file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        sample.cpu_utilization = Some(script.cpu_utilization.unwrap_or(0.05));
        sample.memory_pressure = script.memory_pressure.unwrap_or(MemoryPressure::Normal);
        sample.thermal_state = ThermalState::Nominal;
        if let Some(performance) = script.performance_cores {
            sample.cores = CoreCounts {
                performance,
                efficiency: script.efficiency_cores.unwrap_or(0),
            };
        }
        sample
    }
}

impl<M: MachineInspector> MachineInspector for ScriptedLoad<M> {
    fn inspect(&self) -> Result<Machine> {
        let mut machine = self.machine.inspect()?;
        machine.load = self.scripted(machine.load);
        Ok(machine)
    }
    fn sample_load(&self) -> Result<LoadSample> {
        self.machine.sample_load().map(|s| self.scripted(s))
    }
}

/// A speed meter that always measures the same speed.
#[derive(Clone, Copy, Debug)]
pub struct FixedSpeed(pub f64);
impl SpeedMeter for FixedSpeed {
    fn measure(&self) -> Result<f64> {
        Ok(self.0)
    }
}
