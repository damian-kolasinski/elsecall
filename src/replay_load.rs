//! Arrival density, independent of command work and rehearsal compression.
use crate::{scheduling::timestamp::Timestamp, workload::trace::Trace};
use std::{collections::BTreeMap, time::Duration};

pub use crate::workload::format::{LoadFactor, MAXIMUM};

/// Freeze source physics before moving arrivals. All intervals and deadline
/// slack move with their root; nested work stays inside the same root tree.
/// Background observations and settings keep their original clock.
pub(crate) fn replay(
    factor: LoadFactor,
    source: &Trace,
    since: Timestamp,
    until: Timestamp,
) -> Trace {
    if factor == LoadFactor::default() {
        return source.clone();
    }
    let shifts: BTreeMap<_, _> = source
        .requests
        .iter()
        .map(|r| {
            let offset = r.fact.requested_at.millis().saturating_sub(since.millis());
            let shift = if r.fact.requested_at >= since && r.fact.requested_at < until {
                offset - factor.offset(offset)
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
        factor
            .offset(until.millis() - since.millis())
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
        if let crate::scheduling::request::Urgency::Deferrable { deadline, .. } = &mut r.urgency {
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
