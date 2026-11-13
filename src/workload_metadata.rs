//! Optional declarations about operations inside one admitted command. These are
//! observations, never authorization, resource policy, or process outcomes.
use crate::{failure::Result, scheduling::request::invalid};
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
pub const MAXIMUM_UNITS: u32 = 1_000_000;
pub const MAXIMUM_ELAPSED_MS: u64 = 7 * 24 * 60 * 60 * 1000;
pub const MAXIMUM_JSON_BYTES: usize = 2048;

/// An opaque producer-generated SHA-256 digest, not a display label or command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EstimateKey(String);
impl TryFrom<String> for EstimateKey {
    type Error = &'static str;
    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        if value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(Self(value))
        } else {
            Err("Estimate keys must be 64 lowercase hexadecimal characters.")
        }
    }
}
impl From<EstimateKey> for String {
    fn from(key: EstimateKey) -> Self {
        key.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct PlannedUnits(u32);
impl PlannedUnits {
    pub fn get(self) -> u32 {
        self.0
    }
}
impl TryFrom<u32> for PlannedUnits {
    type Error = &'static str;
    fn try_from(value: u32) -> std::result::Result<Self, Self::Error> {
        if (1..=MAXIMUM_UNITS).contains(&value) {
            Ok(Self(value))
        } else {
            Err("Planned units must be between 1 and 1000000.")
        }
    }
}
impl From<PlannedUnits> for u32 {
    fn from(value: PlannedUnits) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopPolicy {
    StopOnFailure,
    ContinueAll,
}

/// Repository scope and family come from the existing request, never new keys.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Workload {
    Opaque {},
    Sequential {
        planned_units: PlannedUnits,
        stop_policy: StopPolicy,
        estimate_key: EstimateKey,
        variant_key: EstimateKey,
    },
}

impl Workload {
    /// The units of a sequential plan; `None` for opaque work.
    pub fn planned_units(&self) -> Option<u32> {
        match self {
            Self::Opaque {} => None,
            Self::Sequential { planned_units, .. } => Some(planned_units.get()),
        }
    }
}

/// Terminal means the declared operation was fully observed, including failure;
/// it does not claim that all tests passed or even ran inside that operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub attempted_units: u32,
    pub terminal_units: u32,
    pub failed_units: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Failure,
    RequestedStop,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CensorReason {
    Interrupted,
    Timeout,
    Cancelled,
    TransportLost,
    HolderExited,
    ServiceRestart,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Observation {
    AllUnitsObserved {
        coverage: Coverage,
    },
    PrefixObserved {
        coverage: Coverage,
        reason: StopReason,
    },
    Censored {
        coverage: Option<Coverage>,
        reason: CensorReason,
    },
    Unknown {},
}
impl Observation {
    pub fn validate(&self, workload: Option<&Workload>) -> Result<()> {
        let coverage = match self {
            Self::AllUnitsObserved { coverage } | Self::PrefixObserved { coverage, .. } => {
                Some(coverage)
            }
            Self::Censored { coverage, .. } => coverage.as_ref(),
            Self::Unknown {} => None,
        };
        let Some(c) = coverage else {
            return Ok(());
        };
        let Some(Workload::Sequential {
            planned_units,
            stop_policy,
            ..
        }) = workload
        else {
            return Err(invalid(
                "Unit coverage requires an admitted sequential workload.",
            ));
        };
        let planned = planned_units.get();
        if c.failed_units > c.terminal_units
            || c.terminal_units > c.attempted_units
            || c.attempted_units > planned
            || c.elapsed_ms.is_some_and(|ms| ms > MAXIMUM_ELAPSED_MS)
        {
            return Err(invalid(
                "Coverage must satisfy failed <= terminal <= attempted <= planned and bounded elapsed time.",
            ));
        }
        match self {
            Self::AllUnitsObserved { .. } if c.terminal_units != planned => Err(invalid(
                "All-unit coverage must equal the admitted planned count.",
            )),
            Self::PrefixObserved { .. } if c.terminal_units == planned => {
                Err(invalid("An observed prefix must be shorter than the plan."))
            }
            Self::PrefixObserved {
                reason: StopReason::Failure,
                ..
            } if *stop_policy != StopPolicy::StopOnFailure || c.failed_units == 0 => Err(invalid(
                "A failure-stopped prefix requires a failed unit and stop_on_failure.",
            )),
            _ => Ok(()),
        }
    }
}

pub fn parse_workload(json: &str) -> Result<Workload> {
    if json.len() > MAXIMUM_JSON_BYTES {
        return Err(invalid("Workload metadata is too large."));
    }
    serde_json::from_str(json).map_err(|_| invalid("Invalid workload metadata."))
}
pub fn parse_observation(json: &str) -> Result<Observation> {
    if json.len() > MAXIMUM_JSON_BYTES {
        return Err(invalid("Workload observation is too large."));
    }
    serde_json::from_str(json).map_err(|_| invalid("Invalid workload observation."))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn workload() -> Workload {
        Workload::Sequential {
            planned_units: 3.try_into().unwrap(),
            stop_policy: StopPolicy::StopOnFailure,
            estimate_key: "a".repeat(64).try_into().unwrap(),
            variant_key: "b".repeat(64).try_into().unwrap(),
        }
    }
    #[test]
    fn bounds_and_coverage_are_independent_of_process_success() {
        assert!(PlannedUnits::try_from(0).is_err());
        assert!(PlannedUnits::try_from(MAXIMUM_UNITS + 1).is_err());
        assert!(EstimateKey::try_from("command --secret".to_owned()).is_err());
        let coverage = Coverage {
            attempted_units: 3,
            terminal_units: 3,
            failed_units: 2,
            elapsed_ms: Some(100),
        };
        assert!(
            Observation::AllUnitsObserved { coverage }
                .validate(Some(&workload()))
                .is_ok()
        );
        for coverage in [
            Coverage {
                attempted_units: 4,
                ..coverage
            },
            Coverage {
                terminal_units: 4,
                ..coverage
            },
            Coverage {
                failed_units: 4,
                ..coverage
            },
            Coverage {
                elapsed_ms: Some(MAXIMUM_ELAPSED_MS + 1),
                ..coverage
            },
            Coverage {
                terminal_units: 2,
                ..coverage
            },
        ] {
            assert!(
                Observation::AllUnitsObserved { coverage }
                    .validate(Some(&workload()))
                    .is_err()
            );
        }
        assert!(
            Observation::AllUnitsObserved { coverage }
                .validate(None)
                .is_err()
        );
        assert!(
            parse_workload(&format!(
                "{}{}",
                " ".repeat(MAXIMUM_JSON_BYTES),
                r#"{"kind":"opaque"}"#
            ))
            .is_err()
        );
    }
}
