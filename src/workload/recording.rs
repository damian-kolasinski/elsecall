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
    pub calibration: BTreeMap<String, crate::workload::format::Calibration>,
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
    pub load_factor: crate::workload::format::LoadFactor,
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
        self.complete && self.missing_evidence(trace).is_empty()
    }

    /// The evidence `trace` lacks for this recording to count as complete, one
    /// reason per missing piece, apart from the controller's own outcome in
    /// `complete`. Reasons name members and run IDs only, and requests sharing a
    /// reason share one entry, so the list stays short for any trace size.
    pub fn missing_evidence(&self, trace: &Trace) -> Vec<String> {
        let mut missing = Vec::new();
        for m in &trace.members {
            let member = &m.member;
            if !self
                .calibration
                .get(member)
                .is_some_and(crate::workload::format::Calibration::valid)
            {
                missing.push(format!("{member}: no valid worker calibration"));
            }
            if self.load_complete.get(member) != Some(&true) {
                missing.push(format!(
                    "{member}: load observations have a gap over 2 s or cover under 90% of the window"
                ));
            }
            if !m.errors.is_empty() {
                missing.push(format!("{member}: the trace export reported errors"));
            }
            if !m.scheduler_settings.iter().any(|s| s.at <= trace.since) {
                missing.push(format!(
                    "{member}: no scheduler settings in force at the start"
                ));
            }
            if m.load.is_empty() {
                missing.push(format!("{member}: no load buckets"));
            } else if !m.load.iter().all(|b| {
                b.cpu_mean.is_some()
                    && b.outside_mean.is_some()
                    && b.speed_min.is_some()
                    && b.speed_max.is_some()
            }) {
                missing.push(format!(
                    "{member}: a load bucket lacks CPU, outside load or speed observations"
                ));
            }
        }
        if trace.incomplete {
            missing.push("the exported trace is incomplete".to_owned());
        }
        if self.planned == 0 || self.planned != trace.requests.len() {
            missing.push(format!(
                "{} of {} planned requests were exported",
                trace.requests.len(),
                self.planned
            ));
        }
        if !crate::rehearsal::identity::valid_session(&self.session) {
            missing.push("the session ID is invalid".to_owned());
        }
        if !(self.compress.is_finite() && self.compress >= 1.0) {
            missing.push("the compression factor is invalid".to_owned());
        }
        let (mut foreign, mut unevidenced) = (Vec::new(), Vec::new());
        for r in &trace.requests {
            let id = &r.fact.run_id;
            if !(r.fact.synthetic
                && crate::labels::rules::belongs(&self.session, r.fact.label.as_deref())
                && !r.fact.unfinished
                && !r.fact.nested)
            {
                foreign.push(id.as_str());
            } else if !(self.demand.contains_key(id)
                && r.fact
                    .member
                    .as_ref()
                    .is_none_or(|m| self.demand[id].contains_key(m))
                && self.duration_ms.contains_key(id)
                && self.work_iterations.contains_key(id)
                && self.uncontended_ms.contains_key(id)
                && self.ideal_ms.contains_key(id))
            {
                unevidenced.push(id.as_str());
            }
        }
        for (ids, what) in [
            (foreign, "not a finished root request of this rehearsal"),
            (unevidenced, "missing demand, duration or work evidence"),
        ] {
            if !ids.is_empty() {
                missing.push(requests(&ids, what));
            }
        }
        missing
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

/// Run IDs given as examples in one request reason.
const EXAMPLES: usize = 3;

/// One reason for every request it applies to: a count and the first few IDs.
fn requests(ids: &[&str], what: &str) -> String {
    let shown = ids[..ids.len().min(EXAMPLES)].join(", ");
    match ids.len() {
        1 => format!("request {shown}: {what}"),
        n if n <= EXAMPLES => format!("{n} requests ({shown}): {what}"),
        n => format!("{n} requests ({shown} and {} more): {what}", n - EXAMPLES),
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
            if !crate::rehearsal::identity::valid_session(&legacy.session)
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

    /// A one-member, one-request rehearsal trace whose recording is complete.
    fn complete() -> Trace {
        use std::{collections::BTreeMap, time::Duration};
        let start = Timestamp::from_millis(crate::scheduling::fixtures::T0);
        let record = crate::scheduling::fixtures::pinned(0, "work", "a", 1.0);
        let mut trace: Trace = serde_json::from_value(serde_json::json!({"trace_version":1,"incomplete":false,"since":start,"until":start.after(Duration::from_secs(60)),"master":"a","members":[{"member":"a","errors":[],"load":[crate::scheduling::fixtures::bucket(0,0.1,0.0,0,0,1.0)],"scheduler_settings":[{"at":start,"capacity":{"weighted_cores":10,"reserved":0,"capacity":10},"parameters":{}}]}],"requests":[Request::of(&record).unwrap()],"leftovers":[],"demand":[],"contention":[],"limits":[]})).unwrap();
        let session = "a".repeat(24);
        let id = trace.requests[0].fact.run_id.clone();
        trace.requests[0].fact.synthetic = true;
        trace.requests[0].fact.label = Some(format!("rehearsal:{session}:work"));
        trace.rehearsal = Some(Recording {
            calibration: BTreeMap::from([(
                "a".into(),
                crate::workload::format::Calibration {
                    iterations: 1000,
                    elapsed_seconds: 1.0,
                    cpu_seconds: 1.0,
                    iterations_per_second: 1000.0,
                    logical_cores: 1,
                },
            )]),
            ideal_ms: BTreeMap::from([(id.clone(), Some(1000.0))]),
            uncontended_ms: BTreeMap::from([(id.clone(), BTreeMap::from([("a".into(), 1000.0)]))]),
            work_iterations: BTreeMap::from([(id.clone(), 1000)]),
            session,
            source_digest: crate::build_identity::Digest::of(b"source"),
            build: crate::build_identity::this_build().unwrap(),
            load_factor: Default::default(),
            compress: 2.0,
            seed: 0,
            complete: true,
            planned: 1,
            requests: serde_json::Value::Null,
            aggregated_nested: vec![],
            load_complete: BTreeMap::from([("a".into(), true)]),
            demand: BTreeMap::from([(id.clone(), BTreeMap::from([("a".into(), 0.5)]))]),
            duration_ms: BTreeMap::from([(id, 1000)]),
            charge_floor_ms: Default::default(),
        });
        trace
    }

    #[test]
    fn an_incomplete_recording_names_each_missing_piece_of_evidence() {
        type Break = fn(&mut Trace, &mut Recording);
        let trace = complete();
        let recording = trace.rehearsal.clone().unwrap();
        assert!(recording.missing_evidence(&trace).is_empty());
        assert!(recording.complete_for(&trace));
        let id = trace.requests[0].fact.run_id.clone();
        let request = |what: &str| format!("request {id}: {what}");
        let foreign = request("not a finished root request of this rehearsal");
        let unevidenced = request("missing demand, duration or work evidence");
        let rows: Vec<(Break, Vec<String>)> = vec![
            (
                |_, r| r.calibration.clear(),
                vec!["a: no valid worker calibration".into()],
            ),
            (
                |_, r| {
                    r.load_complete.insert("a".into(), false);
                },
                vec![
                    "a: load observations have a gap over 2 s or cover under 90% of the window"
                        .into(),
                ],
            ),
            (
                |t, _| t.members[0].errors.push("trace_unavailable".into()),
                vec!["a: the trace export reported errors".into()],
            ),
            (
                |t, _| {
                    t.members[0].scheduler_settings[0].at =
                        t.since.after(std::time::Duration::from_millis(1))
                },
                vec!["a: no scheduler settings in force at the start".into()],
            ),
            (
                |t, _| t.members[0].load.clear(),
                vec!["a: no load buckets".into()],
            ),
            (
                |t, _| t.members[0].load[0].cpu_mean = None,
                vec!["a: a load bucket lacks CPU, outside load or speed observations".into()],
            ),
            (
                |t, _| t.incomplete = true,
                vec!["the exported trace is incomplete".into()],
            ),
            (
                |_, r| r.planned = 0,
                vec!["1 of 0 planned requests were exported".into()],
            ),
            (
                |_, r| r.planned = 2,
                vec!["1 of 2 planned requests were exported".into()],
            ),
            (
                |_, r| r.compress = 0.5,
                vec!["the compression factor is invalid".into()],
            ),
            (
                |_, r| r.session = "short".into(),
                vec!["the session ID is invalid".into(), foreign.clone()],
            ),
            (
                |t, _| t.requests[0].fact.synthetic = false,
                vec![foreign.clone()],
            ),
            (
                |t, _| t.requests[0].fact.nested = true,
                vec![foreign.clone()],
            ),
            (
                |t, _| t.requests[0].fact.unfinished = true,
                vec![foreign.clone()],
            ),
            (|_, r| r.demand.clear(), vec![unevidenced.clone()]),
            (
                |_, r| r.demand.values_mut().for_each(BTreeMap::clear),
                vec![unevidenced.clone()],
            ),
            (|_, r| r.duration_ms.clear(), vec![unevidenced.clone()]),
            (|_, r| r.work_iterations.clear(), vec![unevidenced.clone()]),
            (|_, r| r.uncontended_ms.clear(), vec![unevidenced.clone()]),
            (|_, r| r.ideal_ms.clear(), vec![unevidenced.clone()]),
        ];
        for (n, (broken, expected)) in rows.into_iter().enumerate() {
            let (mut t, mut r) = (trace.clone(), recording.clone());
            broken(&mut t, &mut r);
            assert_eq!(r.missing_evidence(&t), expected, "row {n}");
            assert!(!r.complete_for(&t), "row {n}");
        }

        // The controller's own outcome is reported by the controller.
        let mut unfinished = recording.clone();
        unfinished.complete = false;
        assert!(unfinished.missing_evidence(&trace).is_empty());
        assert!(!unfinished.complete_for(&trace));

        // Requests sharing a reason share one bounded entry.
        let mut many = trace.clone();
        for n in 0..499 {
            let mut extra = many.requests[0].clone();
            extra.fact.run_id = format!("r-{:012x}", 0x100 + n).parse().unwrap();
            many.requests.push(extra);
        }
        let mut short = recording;
        short.planned = many.requests.len();
        let reasons = short.missing_evidence(&many);
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(reasons[0].starts_with("499 requests (r-000000000100, r-000000000101, r-000000000102 and 496 more): missing demand"), "{reasons:?}");
    }
}
