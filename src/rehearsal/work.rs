//! Freeze CPU work and ideal runtimes independently of fleet orchestration.
use super::{
    invalid,
    worker::{Calibration, logical_demand},
};
use crate::{failure::Result, inspection::CoreCounts, scheduling::capacity::Cores};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub struct Member {
    pub calibration: Calibration,
    pub cores: CoreCounts,
    pub capacity: Cores,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Work {
    pub ideal_ms: Option<f64>,
    pub iterations: u64,
    pub rates: BTreeMap<String, f64>,
    pub uncontended_ms: BTreeMap<String, f64>,
}
impl Work {
    /// Demand contains the plan's structurally eligible members. `size` is the
    /// original declaration, never the candidate's changed declaration.
    pub fn freeze(
        duration_ms: u64,
        demand: &BTreeMap<String, f64>,
        reference_member: &str,
        members: &BTreeMap<String, Member>,
        size: Cores,
        frozen_ideal: Option<Option<f64>>,
    ) -> Result<Self> {
        let reference = members
            .get(reference_member)
            .ok_or_else(|| invalid("Missing reference-member calibration."))?;
        let demand_on_reference = demand
            .get(reference_member)
            .ok_or_else(|| invalid("Reference member violates placement constraints."))?;
        let logical = logical_demand(*demand_on_reference, reference.cores);
        let iterations = (logical * duration_ms as f64 / 1000.0
            * reference.calibration.iterations_per_second)
            .round() as u64;
        let iterations = if logical > 0.0 { iterations.max(1) } else { 0 };
        if iterations > 1_000_000_000_000_000 {
            return Err(invalid("Synthetic work exceeds the work bound."));
        }
        let mut rates = BTreeMap::new();
        let mut uncontended_ms = BTreeMap::new();
        for (name, demand) in demand {
            let member = members
                .get(name)
                .ok_or_else(|| invalid("Missing eligible-member calibration."))?;
            let c = &member.calibration;
            if c.logical_cores != member.cores.performance + member.cores.efficiency {
                return Err(invalid(
                    "Frozen calibration no longer matches member core counts.",
                ));
            }
            let logical = logical_demand(*demand, member.cores).min(f64::from(c.logical_cores));
            let ms = if logical > 0.0 {
                iterations as f64 / (c.iterations_per_second * logical) * 1000.0
            } else {
                duration_ms as f64
            };
            rates.insert(name.clone(), c.iterations_per_second);
            uncontended_ms.insert(name.clone(), ms.max(0.001));
        }
        let ideal = uncontended_ms
            .iter()
            .filter(|(m, _)| size <= members[*m].capacity)
            .map(|(_, v)| *v)
            .reduce(f64::min);
        Ok(Self {
            ideal_ms: frozen_ideal.unwrap_or(ideal),
            iterations,
            rates,
            uncontended_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conserves_work_uses_fastest_capable_member_and_preserves_frozen_ideals() {
        let member = |rate, capacity| Member {
            calibration: Calibration {
                iterations: rate as u64,
                elapsed_seconds: 1.0,
                cpu_seconds: 1.0,
                iterations_per_second: rate,
                logical_cores: 2,
            },
            cores: CoreCounts {
                performance: 2,
                efficiency: 0,
            },
            capacity: Cores::from_millis(capacity),
        };
        let mut members = BTreeMap::from([
            ("a".into(), member(1000.0, 2000)),
            ("b".into(), member(2000.0, 2000)),
        ]);
        let demand = BTreeMap::from([("a".into(), 0.5), ("b".into(), 0.5)]);
        let work = Work::freeze(2000, &demand, "a", &members, Cores::ONE, None).unwrap();
        assert_eq!(work.iterations, 1000);
        assert_eq!(work.ideal_ms, Some(1000.0));
        assert_eq!(work.uncontended_ms["a"], 2000.0);
        members.get_mut("b").unwrap().capacity = Cores::from_millis(500);
        assert_eq!(
            Work::freeze(2000, &demand, "a", &members, Cores::ONE, None)
                .unwrap()
                .ideal_ms,
            Some(2000.0)
        );
        let frozen = Work::freeze(
            2000,
            &demand,
            "a",
            &members,
            Cores::ONE,
            Some(work.ideal_ms),
        )
        .unwrap();
        assert_eq!(frozen.ideal_ms, work.ideal_ms);
        assert_eq!(frozen.iterations, work.iterations);
        assert_eq!(
            Work::freeze(2000, &demand, "a", &members, Cores::from_millis(3000), None)
                .unwrap()
                .ideal_ms,
            None
        );
    }
}
