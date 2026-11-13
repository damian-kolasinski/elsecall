//! Exact admitted grant intervals, independent of sampled host CPU.
use super::{model, timeline::Reserve};
use crate::{scheduling::timestamp::Timestamp, workload::trace::Trace};
use std::collections::BTreeSet;

pub fn observed(trace: &Trace, reserve: &mut Reserve) {
    for member in &trace.members {
        let mut boundaries = BTreeSet::from([trace.since.millis(), trace.until.millis()]);
        let mut intervals = Vec::new();
        let mut missing = trace.incomplete || !member.errors.is_empty();
        for r in trace
            .requests
            .iter()
            .filter(|r| !r.fact.nested && r.fact.member.as_ref() == Some(&member.member))
        {
            let Some((wait, end)) = r.fact.waited_ms.zip(model::terminal(r)) else {
                missing = true;
                continue;
            };
            let start = r.fact.requested_at.millis().saturating_add(wait);
            let end = end.min(trace.until.millis());
            let start = start.max(trace.since.millis());
            if start < end {
                boundaries.extend([start, end]);
                intervals.push((start, end, u64::from(r.fact.size.millis())));
            }
        }
        for s in &member.scheduler_settings {
            if s.at > trace.since && s.at < trace.until {
                boundaries.insert(s.at.millis());
            }
        }
        let boundaries: Vec<_> = boundaries.into_iter().collect();
        for pair in boundaries.windows(2) {
            let (start, end) = (pair[0], pair[1]);
            let seconds = (end - start) as f64 / 1000.0;
            let settings = member
                .scheduler_settings
                .iter()
                .rev()
                .find(|s| s.at <= Timestamp::from_millis(start));
            let Some(settings) = settings.filter(|_| !missing) else {
                reserve.admission_unknown_member_seconds += seconds;
                continue;
            };
            let demand: u64 = intervals
                .iter()
                .filter(|(s, e, _)| *s <= start && *e > start)
                .map(|(_, _, d)| *d)
                .sum();
            // Freeze the approved reserve floor, even if later overlays reduce it.
            let floor = member
                .scheduler_settings
                .iter()
                .map(|s| s.capacity.reserved)
                .max()
                .expect("known settings");
            let usable = settings
                .capacity
                .capacity
                .min(settings.capacity.weighted_cores.saturating_sub(floor));
            let excess_millis = demand.saturating_sub(u64::from(usable.millis()));
            let excess = excess_millis as f64 / 1000.0;
            if excess_millis > 0 {
                reserve.admission_violating_member_seconds += seconds;
            }
            reserve.declared_excess_core_seconds += excess * seconds;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduling::capacity::Cores;
    #[test]
    fn integrates_exact_overload_boundaries_and_missing_settings() {
        let mut trace = crate::simulation::engine::tests::trace();
        trace.members.truncate(1);
        trace.requests.truncate(2);
        let start = trace.since;
        trace.until = start.after(std::time::Duration::from_secs(5));
        for (i, r) in trace.requests.iter_mut().enumerate() {
            r.fact.member = Some("a".into());
            r.fact.size = Cores::from_millis(750);
            r.fact.requested_at = start.after(std::time::Duration::from_secs(i as u64));
            r.fact.waited_ms = Some(0);
            r.fact.resources.as_mut().unwrap().ended_at =
                Some(start.after(std::time::Duration::from_secs(3 + i as u64)));
        }
        // Both 0.75-core grants overlap from t=1 to t=3; usable capacity is 1.
        let mut reserve = Reserve::default();
        observed(&trace, &mut reserve);
        assert_eq!(reserve.admission_violating_member_seconds, 2.0);
        assert_eq!(reserve.declared_excess_core_seconds, 1.0);
        assert_eq!(reserve.admission_unknown_member_seconds, 0.0);
        // Keep admission's integer arithmetic: 0.1 + 0.2 exactly fits 0.3.
        trace.requests[0].fact.size = Cores::from_millis(100);
        trace.requests[1].fact.size = Cores::from_millis(200);
        trace.members[0].scheduler_settings[0]
            .capacity
            .weighted_cores = Cores::from_millis(1300);
        trace.members[0].scheduler_settings[0].capacity.capacity = Cores::from_millis(300);
        let mut reserve = Reserve::default();
        observed(&trace, &mut reserve);
        assert_eq!(reserve.admission_violating_member_seconds, 0.0);
        assert_eq!(reserve.declared_excess_core_seconds, 0.0);
        trace.members[0].scheduler_settings.clear();
        let mut reserve = Reserve::default();
        observed(&trace, &mut reserve);
        assert_eq!(reserve.admission_unknown_member_seconds, 5.0);
        assert_eq!(reserve.admission_violating_member_seconds, 0.0);
    }
}
