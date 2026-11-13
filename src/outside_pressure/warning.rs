//! An early warning: an outside process that has spun or grown for hours while
//! this member's gate may still be open. It is advice, never a blocked state:
//! the member keeps admitting work.
use crate::{outside_pressure::sampling::Owner, scheduling::timestamp::Timestamp};
use serde::{Deserialize, Serialize};

/// What an outside process has been doing for long enough to warn about.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "concern", rename_all = "snake_case")]
pub enum Concern {
    /// It kept about a core or more busy, continuously.
    Spinning {
        /// Cores busy on average over the trailing window.
        cpu_cores: f64,
    },
    /// Its memory grew steadily.
    Growing {
        memory_bytes: u64,
        growth_bytes_per_hour: u64,
    },
    /// It holds a large share of the machine's memory.
    Oversized {
        memory_bytes: u64,
        /// Of the machine's physical memory, 0 to 1.
        share_of_memory: f64,
    },
}

impl Concern {
    /// The concern's kind alone, which with the process identifies a warning.
    pub fn kind(&self) -> &'static str {
        match self {
            Concern::Spinning { .. } => "spinning",
            Concern::Growing { .. } => "growing",
            Concern::Oversized { .. } => "oversized",
        }
    }
}

/// One outside process and what it has done since when.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Warning {
    #[serde(flatten)]
    pub concern: Concern,
    /// The executable's short name; never arguments, environment or paths.
    pub process: String,
    pub pid: u32,
    /// With the PID, tells the process from a later one that reused its PID.
    pub started: Timestamp,
    pub owner: Owner,
    /// Since when the concern has held, as far back as samples show it.
    pub since: Timestamp,
    /// When it was last seen to hold.
    pub observed_at: Timestamp,
}

/// One warning from when it was raised until it stopped holding.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WarningEpisode {
    #[serde(flatten)]
    pub warning: Warning,
    /// When it last held; `None` while it still does.
    pub until: Option<Timestamp>,
}

impl Warning {
    /// Whether `other` is the same concern about the same process.
    pub fn same(&self, other: &Warning) -> bool {
        self.concern.kind() == other.concern.kind()
            && self.pid == other.pid
            && self.started == other.started
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes_flat_fields() {
        let warning = Warning {
            concern: Concern::Spinning { cpu_cores: 1.0 },
            process: "fseventsd".into(),
            pid: 412,
            started: Timestamp::parse_rfc3339("2026-09-09T08:00:00Z").unwrap(),
            owner: Owner::Root,
            since: Timestamp::parse_rfc3339("2026-11-08T21:00:00Z").unwrap(),
            observed_at: Timestamp::parse_rfc3339("2026-11-09T03:00:00Z").unwrap(),
        };
        let json = serde_json::to_value(&warning).unwrap();
        assert_eq!(json["concern"], "spinning");
        assert_eq!(json["cpu_cores"], 1.0);
        assert_eq!(json["owner"], "root");
        assert_eq!(serde_json::from_value::<Warning>(json).unwrap(), warning);
    }
}
