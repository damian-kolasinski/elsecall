//! Controlled rehearsal provenance, distinct from observational production models.
use crate::{
    build_identity::Build,
    workload::trace::{Request, Trace},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recording {
    #[serde(default)]
    pub calibration: BTreeMap<String, super::worker::Calibration>,
    #[serde(default)]
    pub ideal_ms: BTreeMap<String, Option<f64>>,
    #[serde(default)]
    pub uncontended_ms: BTreeMap<String, BTreeMap<String, f64>>,
    #[serde(default)]
    pub work_iterations: BTreeMap<String, u64>,
    pub session: String,
    pub source_digest: crate::build_identity::Digest,
    pub build: Build,
    pub compress: f64,
    #[serde(default)]
    pub load_factor: crate::replay_load::LoadFactor,
    pub seed: u64,
    pub complete: bool,
    pub load_complete: BTreeMap<String, bool>,
    pub planned: usize,
    pub requests: serde_json::Value,
    pub aggregated_nested: Vec<String>,
    /// Original hold floor, frozen before any proposal overrides.
    pub charge_floor_ms: BTreeMap<String, u64>,
    pub duration_ms: BTreeMap<String, u64>,
    /// Effective uncontended duty supplied to synthetic workers, in weighted-core equivalents.
    pub demand: BTreeMap<String, BTreeMap<String, f64>>,
}
impl Recording {
    pub fn complete_for(&self, trace: &Trace) -> bool {
        self.complete
            && trace.members.iter().all(|m| {
                self.calibration
                    .get(&m.member)
                    .is_some_and(super::worker::Calibration::valid)
                    && self.load_complete.get(&m.member) == Some(&true)
                    && m.errors.is_empty()
                    && m.scheduler_settings.iter().any(|s| s.at <= trace.since)
                    && !m.load.is_empty()
                    && m.load.iter().all(|b| {
                        b.cpu_mean.is_some()
                            && b.outside_mean.is_some()
                            && b.speed_min.is_some()
                            && b.speed_max.is_some()
                    })
            })
            && !trace.incomplete
            && self.planned > 0
            && self.planned == trace.requests.len()
            && super::protocol::valid_session(&self.session)
            && self.compress.is_finite()
            && self.compress >= 1.0
            && trace.requests.iter().all(|r| {
                r.fact.synthetic
                    && super::belongs(&self.session, r.fact.label.as_deref())
                    && !r.fact.unfinished
                    && !r.fact.nested
                    && self.demand.contains_key(&r.fact.run_id)
                    && r.fact
                        .member
                        .as_ref()
                        .is_none_or(|m| self.demand[&r.fact.run_id].contains_key(m))
                    && self.duration_ms.contains_key(&r.fact.run_id)
                    && self.work_iterations.contains_key(&r.fact.run_id)
                    && self.uncontended_ms.contains_key(&r.fact.run_id)
                    && self.ideal_ms.contains_key(&r.fact.run_id)
            })
    }
    pub fn original_kind(&self, request: &Request) -> crate::scheduling::request::Kind {
        self.requests
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| r["job"]["run_id"] == request.fact.run_id)
            .and_then(|r| serde_json::from_value(r["original_kind"].clone()).ok())
            .unwrap_or(request.fact.kind)
    }
    pub fn demand(&self, request: &Request, member: &str) -> Option<f64> {
        self.demand.get(&request.fact.run_id)?.get(member).copied()
    }
}

/// H4a's original additive v1 provenance remains readable, but cannot enable
/// controlled-model shortcuts or supply a certificate without the new evidence.
pub fn deserialize<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Recording>, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Legacy {
        session: String,
        compress: f64,
        seed: u64,
        requests: serde_json::Value,
        aggregated_nested: Vec<String>,
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Provenance {
        Current(Box<Recording>),
        Legacy(Legacy),
    }
    match Option::<Provenance>::deserialize(deserializer)? {
        None => Ok(None),
        Some(Provenance::Current(recording)) => Ok(Some(*recording)),
        Some(Provenance::Legacy(legacy)) => {
            if !super::protocol::valid_session(&legacy.session)
                || !(1.0..=1000.0).contains(&legacy.compress)
            {
                return Err(serde::de::Error::custom(
                    "invalid legacy rehearsal provenance",
                ));
            }
            let _ = (legacy.seed, legacy.requests, legacy.aggregated_nested);
            Ok(None)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduling::timestamp::Timestamp;
    #[test]
    fn original_h4a_v1_artifacts_remain_uncertified_replay_inputs() {
        let start = Timestamp::from_millis(60_000_000);
        let mut artifact = serde_json::json!({"trace_version":1,"incomplete":false,"since":start,"until":start.after(std::time::Duration::from_secs(60)),"master":"a","members":[{"member":"a","errors":[],"load":[],"scheduler_settings":[{"at":start,"capacity":{"weighted_cores":2,"reserved":0,"capacity":2},"parameters":{}}]}],"requests":[],"leftovers":[],"demand":[],"contention":[],"limits":[]});
        artifact["rehearsal"] = serde_json::json!({"session":"a".repeat(24),"compress":6.0,"seed":0,"requests":[],"aggregated_nested":[]});
        let trace: Trace = serde_json::from_value(artifact).unwrap();
        assert!(trace.rehearsal.is_none());
        let result = crate::simulation::simulate(
            trace,
            crate::simulation::Options {
                fidelity: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result["fidelity"]["trusted_for_tuning"], false);
        assert!(
            result["fidelity"]["blockers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b.as_str().unwrap().contains("complete recorded UTC day"))
        );
    }
}
