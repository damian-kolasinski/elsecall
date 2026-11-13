//! A machine whose load is reported as idle, for services that tests start on a
//! shared machine: its real load comes from unrelated work, which would otherwise
//! decide whether their tests' requests start. Selected only by
//! `ELSECALL_MACHINE_LOAD=idle`, like the tailnet discovery overrides.
use crate::{
    failure::Result,
    inspection::{LoadSample, Machine, MachineInspector, MemoryPressure, ThermalState},
};

pub struct IdleLoad<M>(pub M);

/// Whether the environment asks for an idle machine.
pub fn requested() -> bool {
    std::env::var("ELSECALL_MACHINE_LOAD").is_ok_and(|value| value == "idle")
}

fn idle(mut sample: LoadSample) -> LoadSample {
    sample.cpu_utilization = Some(0.05);
    sample.memory_pressure = MemoryPressure::Normal;
    sample.thermal_state = ThermalState::Nominal;
    sample
}

impl<M: MachineInspector> MachineInspector for IdleLoad<M> {
    fn inspect(&self) -> Result<Machine> {
        let mut machine = self.0.inspect()?;
        machine.load = idle(machine.load);
        Ok(machine)
    }
    fn sample_load(&self) -> Result<LoadSample> {
        self.0.sample_load().map(idle)
    }
}
