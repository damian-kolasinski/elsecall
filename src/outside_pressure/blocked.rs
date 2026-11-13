//! What a member reports while outside pressure holds its admission gate closed.
use crate::{
    inspection::MemoryPressure,
    outside_pressure::sampling::{Memory, Owner},
    scheduling::{load::Gate, timestamp::Timestamp},
};
use serde::{Deserialize, Deserializer, Serialize};

/// Why the gate is closed, when something outside Elsecall closes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Reason {
    /// The kernel reports memory pressure at a level the gate blocks.
    MemoryPressure { level: MemoryPressure },
    /// Load beyond Elsecall's own grants saturates the machine's CPUs.
    OutsideSaturation {
        /// The window's utilization not explained by Elsecall's grants, 0 to 1.
        outside_utilization: f64,
    },
}

impl Reason {
    /// The reason a closed `gate` gives; `None` while it admits.
    pub fn of(gate: Gate) -> Option<Reason> {
        match gate {
            Gate::MemoryPressure { level } => Some(Reason::MemoryPressure { level }),
            Gate::Saturated {
                outside_utilization,
                ..
            } => Some(Reason::OutsideSaturation {
                outside_utilization,
            }),
            Gate::Open | Gate::Unknown => None,
        }
    }
}

/// A member whose gate has stayed closed by outside pressure long enough to
/// name the outside processes behind it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Blocked {
    #[serde(flatten)]
    pub reason: Reason,
    /// When the gate closed; brief reopenings do not move it.
    pub since: Timestamp,
    /// When the outside processes were last sampled.
    pub attributed_at: Timestamp,
    /// The processes outside Elsecall's work that lead on the reason's measure,
    /// leading first: memory for memory pressure, CPU for saturation.
    pub outside: Vec<OutsideProcess>,
}

impl Blocked {
    /// The process the operator should look at first.
    pub fn leading(&self) -> Option<&OutsideProcess> {
        self.outside.first()
    }
}

/// One process outside Elsecall's work. Its name only: never its arguments,
/// environment or path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutsideProcess {
    pub name: String,
    pub pid: u32,
    pub owner: Owner,
    /// `resident_bytes`, and `footprint_bytes` where it could be read.
    #[serde(flatten)]
    pub memory: Memory,
    /// Cores it kept busy on average since the previous sample: 1.0 is one
    /// whole core.
    pub cpu_cores: f64,
    pub uptime_seconds: u64,
}

/// Reads a blocked state another build reported, dropping one this build
/// cannot decode, such as a reason added by a later build, instead of failing
/// the whole status.
pub fn lenient<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Blocked>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| serde_json::from_value(value).ok()))
}

/// Reads a list another build reported, dropping each entry this build cannot
/// decode.
pub fn lenient_list<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let values = Option::<Vec<serde_json::Value>>::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .flatten()
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes_flat_fields_and_tolerates_later_reasons() {
        let blocked = Blocked {
            reason: Reason::MemoryPressure {
                level: MemoryPressure::Warning,
            },
            since: Timestamp::parse_rfc3339("2026-11-10T04:48:00Z").unwrap(),
            attributed_at: Timestamp::parse_rfc3339("2026-11-10T04:53:00Z").unwrap(),
            outside: vec![OutsideProcess {
                name: "fseventsd".into(),
                pid: 412,
                owner: Owner::Root,
                memory: Memory {
                    resident_bytes: 27 << 30,
                    footprint_bytes: None,
                },
                cpu_cores: 1.0,
                uptime_seconds: 5_356_800,
            }],
        };
        let json = serde_json::to_value(&blocked).unwrap();
        assert_eq!(json["reason"], "memory_pressure");
        assert_eq!(json["level"], "warning");
        assert_eq!(json["outside"][0]["resident_bytes"], 27u64 << 30);
        assert!(json["outside"][0].get("footprint_bytes").is_none());
        assert_eq!(json["outside"][0]["owner"], "root");
        assert_eq!(serde_json::from_value::<Blocked>(json).unwrap(), blocked);

        #[derive(Deserialize)]
        struct Status {
            #[serde(default, deserialize_with = "lenient")]
            blocked: Option<Blocked>,
        }
        let later: Status = serde_json::from_str(
            r#"{"blocked":{"reason":"disk_full","since":"2026-11-10T04:48:00Z","attributed_at":"2026-11-10T04:53:00Z","outside":[]}}"#,
        )
        .unwrap();
        assert_eq!(later.blocked, None);
        let absent: Status = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.blocked, None);
    }
}
