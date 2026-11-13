//! Machine and core-speed observation adapters over virtual fleet state.
use super::{model, world::World};
use crate::{
    failure::Result,
    inspection::{
        self, CoreCounts, LoadSample, Machine, MachineInspector, MemoryPressure, ThermalState,
    },
    scheduling::speed::SpeedMeter,
};
use std::sync::{Arc, Mutex};

pub(super) struct Observation {
    pub(super) world: Arc<Mutex<World>>,
    pub(super) member: String,
    pub(super) cores: CoreCounts,
}
impl MachineInspector for Observation {
    fn inspect(&self) -> Result<Machine> {
        Ok(Machine {
            hostname: Some(self.member.clone()),
            os: "virtual".into(),
            os_version: None,
            architecture: "virtual".into(),
            logical_cpus: (self.cores.performance + self.cores.efficiency) as usize,
            load_average: inspection::LoadAverage {
                one_minute: 0.0,
                five_minutes: 0.0,
                fifteen_minutes: 0.0,
            },
            memory: inspection::Memory {
                total_bytes: 0,
                used_bytes: 0,
                available_bytes: 0,
            },
            uptime_seconds: 0,
            load: self.sample_load()?,
        })
    }
    fn sample_load(&self) -> Result<LoadSample> {
        let w = self.world.lock().unwrap();
        let b = model::bucket(&w.trace, &self.member, w.clock.now().millis());
        Ok(LoadSample {
            cores: self.cores,
            cpu_utilization: w.cpu(&self.member),
            memory_pressure: if b.is_some_and(|b| b.memory_pressure_seconds > 0) {
                b.and_then(|b| b.worst_memory_pressure)
                    .unwrap_or(MemoryPressure::Normal)
            } else {
                MemoryPressure::Normal
            },
            thermal_state: b
                .and_then(|b| b.worst_thermal_state)
                .unwrap_or(ThermalState::Nominal),
        })
    }
}
pub(super) struct Speed(pub f64);
impl SpeedMeter for Speed {
    fn measure(&self) -> Result<f64> {
        Ok(self.0)
    }
}
