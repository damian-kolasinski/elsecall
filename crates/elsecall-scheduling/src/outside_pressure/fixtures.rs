//! Fixed outside-pressure episodes the report's tests compute from.
use elsecall_contract::scheduling_model::timestamp::Timestamp;
use std::time::Duration;

/// A blocked episode by memory pressure from `since` to `until` (`None` while it
/// lasts), led by the root daemons `outside`, each with its resident size and
/// cores.
pub fn episode(
    since: Timestamp,
    until: Option<Timestamp>,
    outside: &[(&str, u64, f64)],
) -> crate::outside_pressure::store::Episode {
    use crate::outside_pressure::{
        blocked::{Blocked, OutsideProcess, Reason},
        sampling::{Memory, Owner},
    };
    crate::outside_pressure::store::Episode {
        blocked: Blocked {
            reason: Reason::MemoryPressure {
                level: elsecall_contract::inspection::MemoryPressure::Warning,
            },
            since,
            attributed_at: since.after(Duration::from_secs(300)),
            outside: outside
                .iter()
                .map(|(name, resident_bytes, cpu_cores)| OutsideProcess {
                    name: (*name).into(),
                    pid: 412,
                    owner: Owner::Root,
                    memory: Memory {
                        resident_bytes: *resident_bytes,
                        footprint_bytes: None,
                    },
                    cpu_cores: *cpu_cores,
                    uptime_seconds: 62 * 86_400,
                })
                .collect(),
        },
        until,
    }
}
