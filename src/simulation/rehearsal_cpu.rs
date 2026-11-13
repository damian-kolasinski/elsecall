//! Forensic replay: inject measured CPU service cost, never observed waits or
//! wall runtimes. This explains an old worker's calibration error without
//! granting predictive trust to a model fitted on the evaluated population.
use crate::{failure::Result, workload::trace::Trace};
use serde_json::{Value, json};

pub fn replay(mut trace: Trace) -> Result<Value> {
    super::validate_trace(&trace)?;
    let recording = trace.rehearsal.as_ref().ok_or_else(|| {
        super::engine_invalid("CPU-service replay requires a complete rehearsal recording.")
    })?;
    if !recording.complete_for(&trace) {
        return Err(super::engine_invalid(
            "CPU-service replay requires a complete rehearsal recording.",
        ));
    }
    let mut costs = Vec::new();
    for r in &trace.requests {
        let member = r.fact.member.as_ref().ok_or_else(|| {
            super::engine_invalid("CPU-service replay requires executed requests on known members.")
        })?;
        let calibration = &recording.calibration[member];
        let candidate = r
            .decision
            .as_ref()
            .and_then(|d| d.candidates.iter().find(|c| &c.member == member))
            .ok_or_else(|| {
                super::engine_invalid("CPU-service replay requires recorded core counts.")
            })?;
        let cores = candidate
            .load
            .as_ref()
            .ok_or_else(|| {
                super::engine_invalid("CPU-service replay requires recorded core counts.")
            })?
            .cores;
        if cores.performance.checked_add(cores.efficiency) != Some(calibration.logical_cores) {
            return Err(super::engine_invalid(
                "CPU-service replay core counts differ from calibration.",
            ));
        }
        let demand = recording.demand[r.fact.run_id.as_str()][member];
        let workers = crate::rehearsal::worker::logical_demand(demand, cores).ceil();
        let cpu_ms = r
            .fact
            .resources
            .as_ref()
            .and_then(|r| r.cpu_ms)
            .ok_or_else(|| {
                super::engine_invalid("CPU-service replay requires measured process-tree CPU.")
            })? as f64;
        let frozen = *recording
            .uncontended_ms
            .get(&r.fact.run_id)
            .and_then(|r| r.get(member))
            .ok_or_else(|| {
                super::engine_invalid(
                    "CPU-service replay requires the executed member's frozen runtime.",
                )
            })?;
        let predicted = if workers > 0.0 {
            frozen.max(cpu_ms / workers)
        } else {
            frozen
        };
        costs.push((r.fact.run_id.clone(), member.clone(), predicted,
            json!({"run_id":r.fact.run_id,"cpu_ms":cpu_ms,"workers":workers,"frozen_ms":frozen,"service_ms":predicted,"calibration_rate":calibration.iterations_per_second})));
    }
    let recording = trace.rehearsal.as_mut().unwrap();
    for (id, member, cost, _) in &costs {
        // No alternative placement is inferred from an observed CPU cost.
        recording
            .uncontended_ms
            .get_mut(id)
            .unwrap()
            .insert(member.clone(), *cost);
    }
    let mut report = super::simulate(
        trace,
        super::Options {
            fidelity: true,
            ..Default::default()
        },
    )?;
    report["diagnostic"] = json!({"kind":"observed_cpu_service_replay","predictive":false,"costs":costs.iter().map(|(_,_,_,c)|c).collect::<Vec<_>>()});
    report["fidelity"]["status"] = json!("diagnostic");
    report["fidelity"]["trusted_for_tuning"] = json!(false);
    report["fidelity"]["blockers"].as_array_mut().unwrap().push(json!("Diagnostic replay uses evaluated requests' measured CPU costs; it cannot certify predictive fidelity."));
    report["certificate"] = Value::Null;
    Ok(report)
}
