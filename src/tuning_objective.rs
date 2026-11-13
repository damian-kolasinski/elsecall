//! Acceptance arithmetic shared by measured comparisons and offline proposals.
use crate::{
    build_identity::{Build, Digest},
    rehearsal::worker::Calibration,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Noise {
    pub noise_version: u32,
    pub source_digest: Digest,
    pub build: Build,
    pub seed: u64,
    pub compress: f64,
    #[serde(default)]
    pub load_factor: crate::replay_load::LoadFactor,
    pub mean_overhead: f64,
    pub p90_overhead: f64,
    /// Freeze controlled work and ideal denominators for subsequent pairs.
    pub calibration: BTreeMap<String, Calibration>,
    pub ideals: BTreeMap<String, Option<f64>>,
    pub baseline_settings: BTreeMap<String, crate::scheduling::records::SchedulerSettings>,
}
impl Noise {
    pub fn valid(&self) -> bool {
        self.noise_version == 1
            && self.mean_overhead.is_finite()
            && self.mean_overhead >= 0.0
            && self.p90_overhead.is_finite()
            && self.p90_overhead >= 0.0
            && self.compress.is_finite()
            && (1.0..=1000.0).contains(&self.compress)
            && !self.ideals.is_empty()
            && self.ideals.len() <= 10_000
            && self.ideals.iter().all(|(id, v)| {
                crate::scheduling::run_id::RunId::parse(id).is_some()
                    && v.is_none_or(|v| v.is_finite() && v > 0.0)
            })
            && self.baseline_settings.len() == self.calibration.len()
            && self
                .calibration
                .keys()
                .all(|m| self.baseline_settings.contains_key(m))
            && !self.calibration.is_empty()
            && self.calibration.len() <= 64
            && self
                .calibration
                .keys()
                .all(|m| m.parse::<crate::roster::MemberName>().is_ok())
            && self.calibration.values().all(Calibration::valid)
    }
    pub fn read(path: &std::path::Path) -> crate::failure::Result<Self> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .and_then(|f| f.take(32 * 1024 * 1024 + 1).read_to_end(&mut bytes))
            .map_err(|e| crate::failure::Failure::io("Read A/A noise", e))?;
        if bytes.len() > 32 * 1024 * 1024 {
            return Err(crate::rehearsal::invalid("Noise file exceeds 32 MiB."));
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| crate::rehearsal::invalid("Invalid noise JSON."))?;
        let value = value.get("data").unwrap_or(&value);
        let noise: Self = serde_json::from_value(value.get("noise_floor").unwrap_or(value).clone())
            .map_err(|_| {
                crate::rehearsal::invalid("Expected a completed A/A noise-floor artifact.")
            })?;
        if !noise.valid() {
            return Err(crate::rehearsal::invalid(
                "Invalid A/A noise-floor evidence.",
            ));
        }
        Ok(noise)
    }
}

/// Inputs are the scheduling_objective objects, not legacy charged-time metrics.
/// Unknown evidence yields a null verdict. A known hard violation yields false.
pub fn compare(a: &Value, b: &Value, noise: Option<&Noise>) -> Value {
    let delta = |path: &str| {
        a.pointer(path)
            .and_then(Value::as_f64)
            .zip(b.pointer(path).and_then(Value::as_f64))
            .map(|(a, b)| b - a)
    };
    let mean = delta("/overhead/mean");
    let p90 = delta("/overhead/p90");
    let failure_pass = a["failures"]["count"]
        .as_u64()
        .zip(b["failures"]["count"].as_u64())
        .map(|(a, b)| b <= a);
    let mut limits = b["hard_limits"].clone();
    if !limits.is_object() {
        limits = json!({"reserve":null,"fleet_busy":null,"contention_slowdown":null});
    }
    limits["failures"] = json!(failure_pass);
    let limits = limits.as_object().expect("limits");
    let holds = if limits.values().any(|v| v == false) {
        Some(false)
    } else if limits.values().all(|v| v == true) {
        Some(true)
    } else {
        None
    };
    let threshold = noise.map(|n| (2.0 * n.mean_overhead).max(0.05));
    let accepted = match (mean, p90, threshold, holds) {
        (Some(mean), Some(p90), Some(threshold), Some(true)) => {
            Some(-mean + 1e-12 >= threshold && p90 <= 1e-12)
        }
        (_, _, _, Some(false)) => Some(false),
        _ => None,
    };
    json!({"accepted":accepted,"mean_improvement":mean.map(|v|-v),"required_mean_improvement":threshold,"p90_not_worse":p90.map(|v|v<=1e-12),"hard_limits":limits,"all_hard_limits_hold":holds,
        "deltas":{"mean_overhead":mean,"p90_overhead":p90,"reserve_violation_seconds":delta("/reserve_violation_seconds"),"fleet_busy_rate":delta("/fleet_busy/rate"),"p90_contention_slowdown":delta("/contention_slowdown/distribution/p90"),"failures":delta("/failures/count")},
        "noise":noise,"rule":"mean improvement >= max(0.05, 2 * A/A mean noise), p90 not worse, every hard limit holds; missing evidence has no verdict"})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn metric(mean: f64, p90: f64) -> Value {
        json!({"overhead":{"mean":mean,"p90":p90},"hard_limits":{"reserve":true,"fleet_busy":true,"contention_slowdown":true,"failures":true},"failures":{"count":1}})
    }
    #[test]
    fn acceptance_uses_percentage_points_twice_noise_p90_and_every_limit() {
        let noise = Noise {
            noise_version: 1,
            source_digest: Digest::of(b"fixture"),
            build: crate::build_identity::this_build().unwrap(),
            seed: 0,
            compress: 1.0,
            load_factor: Default::default(),
            mean_overhead: 0.04,
            p90_overhead: 0.01,
            calibration: BTreeMap::new(),
            ideals: BTreeMap::new(),
            baseline_settings: BTreeMap::new(),
        };
        let a = metric(0.3, 0.5);
        assert_eq!(
            compare(&a, &metric(0.22, 0.5), Some(&noise))["accepted"],
            true
        );
        assert_eq!(
            compare(&a, &metric(0.23, 0.5), Some(&noise))["accepted"],
            false
        );
        assert_eq!(
            compare(&a, &metric(0.1, 0.51), Some(&noise))["accepted"],
            false
        );
        assert!(compare(&a, &metric(0.1, 0.5), None)["accepted"].is_null());
        for limit in ["reserve", "fleet_busy", "contention_slowdown"] {
            let mut b = metric(0.1, 0.5);
            b["hard_limits"][limit] = json!(false);
            assert_eq!(compare(&a, &b, Some(&noise))["accepted"], false);
            b["hard_limits"][limit] = Value::Null;
            assert!(compare(&a, &b, Some(&noise))["accepted"].is_null());
        }
        let mut b = metric(0.1, 0.5);
        b["failures"]["count"] = json!(2);
        assert_eq!(compare(&a, &b, Some(&noise))["accepted"], false);
    }
}
