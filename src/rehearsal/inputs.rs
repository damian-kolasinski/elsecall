//! Freeze per-request rehearsal inputs once, using the same contract for
//! workload selection and execution planning.
use super::{invalid, plan::Arrival, protocol::Launch};
use crate::{
    failure::Result,
    scheduling::{
        request::{Kind, Placement, Urgency},
        timestamp::Timestamp,
    },
    workload::{
        demand::Demand,
        trace::{Request, Trace, TraceOutcome},
    },
};
use std::collections::BTreeMap;

pub(crate) struct Inputs<'a> {
    trace: &'a Trace,
    references: BTreeMap<&'a str, crate::workload::trace::ExecutionReference>,
    demand: BTreeMap<(&'a str, &'a str), &'a Demand>,
}
impl<'a> Inputs<'a> {
    pub fn new(trace: &'a Trace) -> Self {
        let references = trace
            .requests
            .iter()
            .map(|r| {
                (
                    r.fact.run_id.as_str(),
                    crate::simulation::freeze_reference(trace, r),
                )
            })
            .collect();
        let mut demand = BTreeMap::new();
        for d in &trace.demand {
            demand
                .entry((d.member.as_str(), d.family.as_str()))
                .or_insert(d);
        }
        Self {
            trace,
            references,
            demand,
        }
    }
    pub fn freeze(&self, trace: &mut Trace) {
        for request in &mut trace.requests {
            request.execution_reference =
                self.references.get(request.fact.run_id.as_str()).cloned();
        }
    }
    fn label(&self, r: &Request) -> Result<String> {
        let label = format!(
            "{}{}",
            super::PREFIX,
            r.fact.label.as_deref().unwrap_or("unlabelled")
        );
        if label.chars().count() > super::label_limit(&label) {
            return Err(invalid(
                "A source label is too long for the reserved namespace.",
            ));
        }
        Ok(label)
    }
    fn duration(&self, r: &Request, compress: f64) -> Result<u64> {
        match self.references.get(r.fact.run_id.as_str()) {
            Some(crate::workload::trace::ExecutionReference::Known { milliseconds, .. }) => {
                let ms = (milliseconds.round() / compress).round() as u64;
                if ms > 3_600_000 {
                    return Err(invalid(
                        "A synthetic command must be at most one hour; increase compression.",
                    ));
                }
                Ok(ms.max(1))
            }
            _ => Err(invalid("No reference run duration.")),
        }
    }
    fn demand(&self, r: &Request) -> Result<BTreeMap<String, f64>> {
        let family = crate::workload::demand::family(&r.fact)
            .ok_or_else(|| invalid("Effective demand requires a recorded label/family."))?;
        let mut demand = BTreeMap::new();
        let mut missing = Vec::new();
        for member in crate::simulation::structural_members(self.trace, r) {
            let profile = self.demand.get(&(member.as_str(), family)).copied();
            match profile
                .and_then(|d| d.effective_cores)
                .filter(|c| c.is_finite() && (0.0..=256.0).contains(c))
            {
                Some(cores) => {
                    demand.insert(member.to_owned(), cores);
                }
                None => missing.push(format!(
                    "{member} / {family}: {}",
                    profile.map_or("missing_family_observations", |d| d.status.as_str())
                )),
            }
        }
        if !missing.is_empty() {
            return Err(invalid(format!(
                "{} lacks identified H1 effective demand for {}.",
                r.fact.run_id,
                missing.join("; ")
            )));
        }
        if demand.values().any(|d| *d == 0.0) && demand.values().any(|d| *d > 0.0) {
            return Err(invalid(
                "A fixed-work request cannot mix zero and positive CPU demand across eligible members.",
            ));
        }
        Ok(demand)
    }
    fn hold(&self, r: &Request) -> Result<Option<u64>> {
        match r.urgency {
            Urgency::Interactive { hold_limit_ms } => r
                .hold_limit
                .map(|h| h.seconds.as_millis() as u64)
                .or(hold_limit_ms)
                .ok_or_else(|| invalid("Interactive requests need their resolved hold limit."))
                .map(Some),
            _ => Ok(None),
        }
    }
    fn workspace(&self, r: &Request) -> Result<String> {
        let workspace = match &r.workspace {
            Some(w) => w.workspace.clone(),
            None if r.workspace_recorded => {
                use sha2::{Digest, Sha256};
                format!(
                    "{:x}",
                    Sha256::digest(format!("outside-tree:{}", r.fact.calling_member).as_bytes())
                )
            }
            None => {
                return Err(invalid(
                    "H4a requires recorded workspace identities or known outside-tree work.",
                ));
            }
        };
        if !super::protocol::valid_workspace(&workspace) {
            return Err(invalid(
                "A synthetic workspace must be a lowercase SHA-256 identity.",
            ));
        }
        Ok(workspace)
    }
    fn exit(&self, r: &Request) -> Result<i32> {
        let exit_code = match &r.outcome {
            Some(TraceOutcome::Exited { exit_code }) => *exit_code,
            Some(TraceOutcome::Released | TraceOutcome::HolderExited)
                if r.fact.kind == Kind::Lease =>
            {
                0
            }
            Some(TraceOutcome::Failed { code })
                if matches!(
                    code.as_str(),
                    "fleet_busy"
                        | "request_too_large"
                        | "deadline_skipped"
                        | "member_unavailable"
                        | "config_sync_failed"
                ) =>
            {
                0
            }
            _ => {
                return Err(invalid(
                    "H4a requires terminal command exits or scheduling refusals; cancellations and infrastructure failures cannot be synthesized.",
                ));
            }
        };
        if !(0..=255).contains(&exit_code) {
            return Err(invalid("A synthetic command requires a Unix exit code."));
        }
        Ok(exit_code)
    }
    /// Diagnose independent requirements together, using the same checks as
    /// execution planning. Missing terminal evidence cannot hide missing demand.
    pub fn problems(&self, r: &Request, compress: f64) -> Vec<String> {
        [
            self.label(r).map(|_| ()),
            self.duration(r, compress).map(|_| ()),
            self.demand(r).map(|_| ()),
            self.hold(r).map(|_| ()),
            self.workspace(r).map(|_| ()),
            self.exit(r).map(|_| ()),
        ]
        .into_iter()
        .filter_map(|result| result.err())
        .map(|e| format!("{}: {}", r.fact.run_id, e.message))
        .collect()
    }
    pub fn arrival(&self, r: &Request, index: usize, compress: f64) -> Result<Arrival> {
        let scale = |ms: u64| (ms as f64 / compress).round() as u64;
        let urgency = match r.urgency {
            Urgency::Interactive { .. } => Urgency::Interactive {
                hold_limit_ms: None,
            },
            Urgency::Deferrable {
                deadline,
                at_deadline,
            } => Urgency::Deferrable {
                deadline: Timestamp::from_millis(scale(
                    deadline.millis().saturating_sub(self.trace.since.millis()),
                )),
                at_deadline,
            },
        };
        let hold_ms = self.hold(r)?;
        let request = Launch {
            index,
            workspace: self.workspace(r)?,
            label: self.label(r)?,
            size: r.fact.size,
            placement: r.placement_request.clone(),
            urgency,
            hold_ms,
            duration_ms: self.duration(r, compress)?,
            work: None,
            demand: self.demand(r)?,
            exit_code: self.exit(r)?,
        };
        Ok(Arrival {
            at_ms: scale(
                r.fact
                    .requested_at
                    .millis()
                    .saturating_sub(self.trace.since.millis()),
            ),
            member: r.fact.calling_member.clone(),
            original_run_id: r.fact.run_id.clone(),
            reference_member: r
                .fact
                .member
                .clone()
                .unwrap_or_else(|| match &r.placement_request {
                    Placement::Member(target) => target.clone(),
                    _ => r.fact.calling_member.clone(),
                }),
            original_hold_ms: hold_ms,
            original_kind: r.fact.kind,
            original_label: r.fact.label.clone(),
            original_family: r.fact.family.clone(),
            request,
        })
    }
}
