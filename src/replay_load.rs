//! Arrival density, independent of command work and rehearsal compression.
use crate::{failure::Result, scheduling::timestamp::Timestamp, workload::trace::Trace};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};

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
    type Error = crate::failure::Failure;
    fn try_from(f: f64) -> Result<Self> {
        if f.is_finite() && (1.0..=MAXIMUM).contains(&f) {
            Ok(Self(f))
        } else {
            Err(crate::failure::Failure::new(
                "invalid_request",
                "--load-factor must be finite and from 1 to 32.",
                "Use an arrival load factor from 1 to 32.",
            ))
        }
    }
}
impl std::str::FromStr for LoadFactor {
    type Err = crate::failure::Failure;
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
    /// Freeze source physics before moving arrivals. All intervals and deadline
    /// slack move with their root; nested work stays inside the same root tree.
    /// Background observations and settings keep their original clock.
    pub(crate) fn replay(self, source: &Trace, since: Timestamp, until: Timestamp) -> Trace {
        if self == Self::default() {
            return source.clone();
        }
        let shifts: BTreeMap<_, _> = source
            .requests
            .iter()
            .map(|r| {
                let offset = r.fact.requested_at.millis().saturating_sub(since.millis());
                let shift = if r.fact.requested_at >= since && r.fact.requested_at < until {
                    offset - self.offset(offset)
                } else {
                    0
                };
                (r.fact.run_id.as_str(), shift)
            })
            .collect();
        let by_id: BTreeMap<_, _> = source
            .requests
            .iter()
            .map(|r| (r.fact.run_id.as_str(), r))
            .collect();
        let mut replay = source.clone();
        replay.until = since.after(Duration::from_millis(
            self.offset(until.millis() - since.millis())
                .saturating_add(1),
        ));
        // Later arrivals must not become part of the shortened cohort or drain.
        replay.requests.retain(|r| r.fact.requested_at < until);
        for r in &mut replay.requests {
            r.execution_reference = Some(crate::simulation::freeze_reference(source, r));
            let mut root = &by_id[r.fact.run_id.as_str()];
            while let Some(parent) = root.nested_in.as_deref().and_then(|id| by_id.get(id)) {
                root = parent;
            }
            let shift = shifts[root.fact.run_id.as_str()];
            let move_time = |t: Timestamp| Timestamp::from_millis(t.millis().saturating_sub(shift));
            r.fact.requested_at = move_time(r.fact.requested_at);
            if let Some(resources) = &mut r.fact.resources {
                resources.started_at = resources.started_at.map(move_time);
                resources.ended_at = resources.ended_at.map(move_time);
            }
            if let crate::scheduling::request::Urgency::Deferrable { deadline, .. } = &mut r.urgency
            {
                *deadline = move_time(*deadline);
            }
            if let Some(d) = &mut r.decision {
                d.decided_at = move_time(d.decided_at);
            }
            if let Some(attempts) = &mut r.pre_run_attempts {
                for a in attempts {
                    a.started_at = a.started_at.map(move_time);
                }
            }
        }
        replay
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn density_is_bounded_and_only_scales_arrival_offsets() {
        for f in [0.0, 0.5, 32.01, f64::INFINITY, f64::NAN] {
            assert!(LoadFactor::try_from(f).is_err());
        }
        assert_eq!(LoadFactor::try_from(4.0).unwrap().offset(12000), 3000);
        assert!(serde_json::from_str::<LoadFactor>("0").is_err());
    }
}
