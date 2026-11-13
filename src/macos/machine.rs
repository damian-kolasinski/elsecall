use crate::{
    failure::{Failure, Result},
    inspection::{
        CoreCounts, LoadAverage, LoadSample, Machine, MachineInspector, Memory, MemoryPressure,
        ThermalState,
    },
    macos::{sysctl, thermal},
};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

/// CPU utilization is the change in CPU time between samples, so the adapter keeps
/// the previous sample. A window shorter than this returns the previous value.
const MINIMUM_WINDOW: Duration = Duration::from_millis(500);

pub struct MacMachine {
    cpu: Mutex<CpuWindow>,
}
struct CpuWindow {
    system: sysinfo::System,
    sampled: Option<Instant>,
    utilization: Option<f64>,
}
impl Default for MacMachine {
    fn default() -> Self {
        Self {
            cpu: Mutex::new(CpuWindow {
                system: sysinfo::System::new(),
                sampled: None,
                utilization: None,
            }),
        }
    }
}

impl MacMachine {
    fn cpu_utilization(&self) -> Option<f64> {
        let mut window = self.cpu.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        match window.sampled {
            Some(at) if now.duration_since(at) < MINIMUM_WINDOW => (),
            first_or_due => {
                window.system.refresh_cpu_usage();
                if first_or_due.is_some() {
                    let percent = f64::from(window.system.global_cpu_usage());
                    window.utilization = Some((percent / 100.0).clamp(0.0, 1.0));
                }
                window.sampled = Some(now);
            }
        }
        window.utilization
    }
}

fn logical_cpus() -> Result<u32> {
    let count = std::thread::available_parallelism()
        .map_err(|e| Failure::io("Read CPU count", e))?
        .get();
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

fn cores() -> Result<CoreCounts> {
    // Apple silicon lists performance levels from fastest; level 1 is efficiency.
    let levels = sysctl::integer(c"hw.nperflevels").unwrap_or(1);
    let level = |name| sysctl::integer(name).and_then(|n| u32::try_from(n).ok());
    match (levels, level(c"hw.perflevel0.logicalcpu")) {
        (2, Some(performance)) => Ok(CoreCounts {
            performance,
            efficiency: level(c"hw.perflevel1.logicalcpu").unwrap_or(0),
        }),
        _ => Ok(CoreCounts {
            performance: logical_cpus()?,
            efficiency: 0,
        }),
    }
}

fn memory_pressure() -> MemoryPressure {
    // Values from kern.memorystatus_vm_pressure_level: 1 normal, 2 warn, 4 critical.
    match sysctl::integer(c"kern.memorystatus_vm_pressure_level") {
        Some(2) => MemoryPressure::Warning,
        Some(4) => MemoryPressure::Critical,
        _ => MemoryPressure::Normal,
    }
}

impl MachineInspector for MacMachine {
    fn inspect(&self) -> Result<Machine> {
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        let load = sysinfo::System::load_average();
        Ok(Machine {
            hostname: sysinfo::System::host_name(),
            os: "macos".into(),
            os_version: sysinfo::System::os_version(),
            architecture: std::env::consts::ARCH.into(),
            logical_cpus: logical_cpus()? as usize,
            load_average: LoadAverage {
                one_minute: load.one,
                five_minutes: load.five,
                fifteen_minutes: load.fifteen,
            },
            memory: Memory {
                total_bytes: system.total_memory(),
                used_bytes: system.used_memory(),
                available_bytes: system.available_memory(),
            },
            uptime_seconds: sysinfo::System::uptime(),
            load: self.sample_load()?,
        })
    }

    fn sample_load(&self) -> Result<LoadSample> {
        Ok(LoadSample {
            cores: cores()?,
            cpu_utilization: self.cpu_utilization(),
            memory_pressure: memory_pressure(),
            thermal_state: thermal::state().unwrap_or(ThermalState::Nominal),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utilization_needs_two_samples_and_core_types_add_up() {
        let machine = MacMachine::default();
        let first = machine.sample_load().unwrap();
        assert_eq!(first.cpu_utilization, None);
        assert_eq!(
            first.cores.performance + first.cores.efficiency,
            logical_cpus().unwrap()
        );
        std::thread::sleep(MINIMUM_WINDOW);
        let second = machine.sample_load().unwrap();
        assert!(
            second
                .cpu_utilization
                .is_some_and(|u| (0.0..=1.0).contains(&u))
        );
    }
}
