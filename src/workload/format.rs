//! What a trace file carries besides its requests: how a replay chose
//! placements, the arrival density it ran at, and a rehearsal's worker
//! calibration. Replay, simulation, rehearsal and tuning all read traces, so the
//! format lives with them in workload, below each of them.
use clap::ValueEnum;
use elsecall_failure::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, serde::Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Historical,
    Current,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub mode: Mode,
    pub rules_digest: crate::build_identity::Digest,
}

/// Bound both manual replay density and the automatic calibration search.
pub const MAXIMUM: f64 = 32.0;
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct LoadFactor(f64);
impl Default for LoadFactor {
    fn default() -> Self {
        Self(1.0)
    }
}
impl TryFrom<f64> for LoadFactor {
    type Error = elsecall_failure::Failure;
    fn try_from(f: f64) -> Result<Self> {
        if f.is_finite() && (1.0..=MAXIMUM).contains(&f) {
            Ok(Self(f))
        } else {
            Err(elsecall_failure::Failure::new(
                "invalid_request",
                "--load-factor must be finite and from 1 to 32.",
                "Use an arrival load factor from 1 to 32.",
            ))
        }
    }
}
impl std::str::FromStr for LoadFactor {
    type Err = elsecall_failure::Failure;
    fn from_str(s: &str) -> Result<Self> {
        Self::try_from(s.parse::<f64>().unwrap_or(f64::NAN))
    }
}
impl From<LoadFactor> for f64 {
    fn from(f: LoadFactor) -> f64 {
        f.0
    }
}
impl std::fmt::Display for LoadFactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl LoadFactor {
    pub fn offset(self, milliseconds: u64) -> u64 {
        (milliseconds as f64 / self.0).round() as u64
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Calibration {
    pub iterations: u64,
    pub elapsed_seconds: f64,
    pub cpu_seconds: f64,
    pub iterations_per_second: f64,
    pub logical_cores: u32,
}
impl Calibration {
    pub fn valid(&self) -> bool {
        self.iterations > 0
            && self.elapsed_seconds.is_finite()
            && self.elapsed_seconds > 0.0
            && self.cpu_seconds.is_finite()
            && self.cpu_seconds > 0.0
            && self.iterations_per_second.is_finite()
            && self.iterations_per_second > 0.0
            && self.logical_cores > 0
            && self.logical_cores <= 256
            && (self.iterations as f64 / self.cpu_seconds / self.iterations_per_second - 1.0).abs()
                < 0.000001
    }
}

/// H1 fits machine CPU share times weighted capacity. OS CPU time is measured
/// against all logical CPUs, so fractional busy workers need this conversion.
pub fn logical_demand(effective: f64, cores: crate::inspection::CoreCounts) -> f64 {
    let weighted =
        crate::scheduling::capacity::Capacity::of(cores, crate::scheduling::capacity::Cores::ZERO)
            .weighted_cores
            .as_f64();
    effective * f64::from(cores.performance + cores.efficiency) / weighted
}
