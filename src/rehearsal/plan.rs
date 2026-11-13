//! Freeze trace inputs before creating scratch trees or submitting any work.
use super::{invalid, protocol::Launch};
use crate::{failure::Result, scheduling::request::Kind, workload::trace::Trace};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize)]
pub struct Arrival {
    pub at_ms: u64,
    pub member: String,
    pub original_run_id: String,
    pub reference_member: String,
    pub original_hold_ms: Option<u64>,
    pub original_kind: Kind,
    pub original_label: Option<String>,
    pub original_family: Option<String>,
    pub request: Launch,
}
pub struct Plan {
    pub arrivals: Vec<Arrival>,
    pub aggregated_nested: Vec<String>,
    pub workspaces: BTreeMap<String, Vec<String>>,
}
impl Plan {
    pub fn new(trace: &Trace, compress: f64) -> Result<Self> {
        Self::with_load(trace, compress, Default::default())
    }
    pub fn with_load(
        trace: &Trace,
        compress: f64,
        load_factor: crate::replay_load::LoadFactor,
    ) -> Result<Self> {
        crate::simulation::validate_trace(trace)?;
        if !compress.is_finite() || !(1.0..=1000.0).contains(&compress) {
            return Err(invalid(
                "--compress must be a finite number from 1 to 1000.",
            ));
        }
        let mut arrivals = Vec::new();
        let mut workspaces: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut requests: Vec<_> = trace
            .requests
            .iter()
            .filter(|r| {
                !r.fact.nested
                    && r.fact.requested_at >= trace.since
                    && r.fact.requested_at < trace.until
            })
            .collect();
        requests.sort_by_key(|r| (r.fact.requested_at, r.fact.run_id.as_str()));
        if requests.is_empty() || requests.len() > 10_000 {
            return Err(invalid("A rehearsal requires 1 to 10000 arrivals."));
        }
        // Root process-tree demand already includes nested work. Never submit
        // another independent synthetic grant for the same resource exposure.
        if let Some((id, reason)) = nested_problems(trace).first() {
            return Err(invalid(format!("{id}: {reason}")));
        }
        let aggregated_nested = trace
            .requests
            .iter()
            .filter(|r| r.fact.nested && r.fact.requested_at >= trace.since)
            .map(|r| r.fact.run_id.clone())
            .collect();
        if trace.requests.iter().any(|r| {
            r.fact.requested_at < trace.since && r.terminal_at().is_none_or(|end| end > trace.since)
        }) {
            return Err(invalid(
                "H4a requires no active carry-in; select a window before the first request.",
            ));
        }
        let inputs = super::inputs::Inputs::new(trace);
        for (index, r) in requests.into_iter().enumerate() {
            let mut arrival = inputs.arrival(r, index, compress)?;
            let at_ms = (load_factor.offset(r.fact.requested_at.millis() - trace.since.millis())
                as f64
                / compress)
                .round() as u64;
            let shift = arrival.at_ms - at_ms;
            arrival.at_ms = at_ms;
            if let crate::scheduling::request::Urgency::Deferrable { deadline, .. } =
                &mut arrival.request.urgency
            {
                *deadline = crate::scheduling::timestamp::Timestamp::from_millis(
                    deadline.millis().saturating_sub(shift),
                );
            }
            workspaces
                .entry(r.fact.calling_member.clone())
                .or_default()
                .insert(arrival.request.workspace.clone());
            arrivals.push(arrival);
        }
        Ok(Self {
            arrivals,
            aggregated_nested,
            workspaces: workspaces
                .into_iter()
                .map(|(m, w)| (m, w.into_iter().collect()))
                .collect(),
        })
    }
}

/// Every unsupported nested interval, with the child's record identity.
pub(crate) fn nested_problems(trace: &Trace) -> Vec<(String, &'static str)> {
    let by_id: BTreeMap<_, _> = trace
        .requests
        .iter()
        .map(|r| (r.fact.run_id.as_str(), r))
        .collect();
    let mut problems = Vec::new();
    for nested in trace.requests.iter().filter(|r| {
        r.fact.nested && r.fact.requested_at >= trace.since && r.fact.requested_at < trace.until
    }) {
        let mut ancestor = Some(nested);
        while let Some(r) = ancestor.filter(|r| r.fact.nested) {
            ancestor = r.nested_in.as_deref().and_then(|id| by_id.get(id)).copied();
        }
        let Some(ancestor) = ancestor else {
            problems.push((
                nested.fact.run_id.clone(),
                "A nested request lacks its root's replay inputs.",
            ));
            continue;
        };
        let root_interval = ancestor
            .fact
            .resources
            .as_ref()
            .and_then(|v| Some((v.started_at?, v.ended_at?)));
        let child_end = nested.fact.resources.as_ref().and_then(|v| v.ended_at);
        if ancestor.fact.requested_at < trace.since
            || root_interval
                .zip(child_end)
                .is_none_or(|((start, end), child_end)| {
                    nested.fact.requested_at < start || child_end > end
                })
        {
            problems.push((nested.fact.run_id.clone(), "A nested interval is not fully covered by its synthesized root; surviving nested capacity requires separate replay evidence."));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        reporting::fixtures::{T0, base},
        scheduling::{
            record::{HoldBasis, HoldLimitRecord, Outcome, WorkspaceIdentity},
            request::{AtDeadline, Placement, Urgency},
            timestamp::Timestamp,
        },
        workload::{
            demand::Demand,
            trace::{Member, Request, TraceOutcome},
        },
    };
    use std::time::Duration;
    fn trace() -> Trace {
        let mut r = base(1, "build fixture", "alpha");
        r.member = Some("alpha".into());
        r.run_ms = Some(8000);
        r.started_at = Some(r.requested_at);
        r.ended_at = Some(r.requested_at.after(Duration::from_secs(8)));
        r.outcome = Some(Outcome::Exited { exit_code: 7 });
        r.workspace = Some(WorkspaceIdentity {
            workspace: "a".repeat(64),
            repository: "b".repeat(64),
        });
        r.hold_limit = Some(HoldLimitRecord {
            seconds: Duration::from_secs(10),
            basis: HoldBasis::Requested,
            expected_run_seconds: None,
        });
        Trace {
            rehearsal: None,
            placement_replay: None,
            trace_version: 1,
            incomplete: false,
            since: Timestamp::from_millis(T0),
            until: Timestamp::from_millis(T0 + 60000),
            master: Some("alpha".into()),
            members: vec![Member {
                member: "alpha".into(),
                load: vec![],
                errors: vec![],
                scheduler_settings: vec![],
            }],
            requests: vec![Request::of(&r).unwrap()],
            leftovers: vec![],
            demand: vec![Demand {
                member: "alpha".into(),
                family: "build fixture".into(),
                runs: 5,
                fitted_runs: 5,
                declared_cores: Some(2.0),
                process_tree_cores: Some(0.5),
                effective_cores: Some(0.75),
                interval_cores: Some([0.5, 1.0]),
                fitted_minutes: 10,
                idle_minutes: 3,
                outside_baseline_cores: Some(0.0),
                residual_rmse_cores: Some(0.1),
                status: "identified".into(),
            }],
            contention: vec![],
            limits: vec![],
        }
    }
    #[test]
    fn scales_arrivals_and_execution_but_preserves_hold_size_and_placement() {
        let t = trace();
        let p = Plan::new(&t, 4.0).unwrap();
        let a = &p.arrivals[0];
        assert_eq!(a.at_ms, 250);
        assert_eq!(a.request.duration_ms, 2000);
        assert_eq!(a.request.hold_ms, Some(10000));
        assert_eq!(a.request.size, t.requests[0].fact.size);
        assert_eq!(a.request.placement, Placement::Pinned);
        assert_eq!(a.request.exit_code, 7);
        assert_eq!(a.request.demand["alpha"], 0.75);
        let mut t = t;
        t.requests[0].fact.deferrable = true;
        t.requests[0].urgency = Urgency::Deferrable {
            deadline: Timestamp::from_millis(T0 + 12000),
            at_deadline: AtDeadline::Skip,
        };
        let p = Plan::new(&t, 4.0).unwrap();
        assert_eq!(
            p.arrivals[0].request.urgency,
            Urgency::Deferrable {
                deadline: Timestamp::from_millis(3000),
                at_deadline: AtDeadline::Skip
            }
        );
        assert_eq!(p.arrivals[0].request.hold_ms, None);
    }
    #[test]
    fn load_factor_changes_only_arrival_spacing_on_top_of_compression() {
        let t = trace();
        let ordinary = Plan::new(&t, 4.0).unwrap();
        let busy = Plan::with_load(&t, 4.0, "5".parse().unwrap()).unwrap();
        assert_eq!(busy.arrivals[0].at_ms, ordinary.arrivals[0].at_ms / 5);
        let mut expected = serde_json::to_value(&ordinary.arrivals[0]).unwrap();
        expected["at_ms"] = serde_json::json!(50);
        assert_eq!(serde_json::to_value(&busy.arrivals[0]).unwrap(), expected);
    }
    #[test]
    fn nests_are_aggregated_and_known_outside_tree_leases_become_run_grants() {
        let mut t = trace();
        let mut nested = t.requests[0].clone();
        nested.fact.run_id = "r-000000000002".into();
        nested.fact.nested = true;
        nested.nested_in = Some(t.requests[0].fact.run_id.clone());
        nested.shares_capacity_of = nested.nested_in.clone();
        t.requests.push(nested);
        let mut lease = t.requests[0].clone();
        lease.fact.run_id = "r-000000000003".into();
        lease.fact.kind = Kind::Lease;
        lease.workspace = None;
        lease.outcome = Some(TraceOutcome::Released);
        t.requests.push(lease);
        let p = Plan::new(&t, 2.0).unwrap();
        assert_eq!(p.arrivals.len(), 2);
        assert_eq!(p.aggregated_nested, ["r-000000000002"]);
        assert_eq!(p.arrivals[1].original_kind, Kind::Lease);
        assert_eq!(p.arrivals[1].request.exit_code, 0);
        assert_eq!(p.workspaces["alpha"].len(), 2);
        t.requests[1].fact.resources.as_mut().unwrap().ended_at =
            Some(Timestamp::from_millis(T0 + 20000));
        assert!(Plan::new(&t, 2.0).is_err());
        t.requests.remove(0);
        assert!(Plan::new(&t, 2.0).is_err());
    }
    #[test]
    fn a_refused_required_request_uses_the_target_as_its_work_reference() {
        let mut t = trace();
        let mut target = t.members[0].clone();
        target.member = "beta".into();
        t.members.push(target);
        let mut demand = t.demand[0].clone();
        demand.member = "beta".into();
        t.demand.push(demand);
        let mut r = t.requests[0].clone();
        r.fact.run_id = "r-000000000002".into();
        r.fact.member = None;
        r.fact.run_ms = None;
        r.fact.completed = false;
        r.fact.failure = Some("fleet_busy".into());
        r.outcome = Some(TraceOutcome::Failed {
            code: "fleet_busy".into(),
        });
        r.placement_request = Placement::Member("beta".into());
        r.fact.placement = crate::reporting::facts::PlacementKind::Required;
        t.requests.push(r);
        let plan = Plan::new(&t, 4.0).unwrap();
        assert_eq!(plan.arrivals[1].reference_member, "beta");
        assert_eq!(
            plan.arrivals[1].request.demand.keys().collect::<Vec<_>>(),
            vec!["beta"]
        );
    }
    #[test]
    fn fixed_work_rejects_a_target_that_would_skip_positive_cpu_work() {
        let mut t = trace();
        t.requests[0].placement_request = Placement::Placeable;
        t.requests[0].fact.placement = crate::reporting::facts::PlacementKind::Placeable;
        let mut target = t.members[0].clone();
        target.member = "beta".into();
        t.members.push(target);
        let mut demand = t.demand[0].clone();
        demand.member = "beta".into();
        demand.effective_cores = Some(0.0);
        demand.interval_cores = Some([0.0, 0.0]);
        t.demand.push(demand);
        assert!(
            Plan::new(&t, 4.0)
                .err()
                .unwrap()
                .message
                .contains("zero and positive")
        );
    }
    #[test]
    fn unknown_inputs_and_active_carry_in_block_before_execution() {
        for factor in [0.0, 0.5, 1001.0, f64::NAN, f64::INFINITY] {
            assert!(Plan::new(&trace(), factor).is_err());
        }
        let mut t = trace();
        t.demand[0].effective_cores = None;
        assert!(
            Plan::new(&t, 6.0)
                .err()
                .unwrap()
                .message
                .contains("effective demand")
        );
        let mut t = trace();
        t.requests[0].hold_limit = None;
        assert!(Plan::new(&t, 6.0).is_err());
        let mut t = trace();
        t.since = Timestamp::from_millis(T0 + 2000);
        assert!(Plan::new(&t, 6.0).is_err());
        let mut t = trace();
        t.requests[0].workspace = None;
        t.requests[0].workspace_recorded = false;
        assert!(Plan::new(&t, 6.0).is_err());
    }
}
