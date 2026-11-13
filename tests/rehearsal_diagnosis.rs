use elsecall::{simulation, workload::trace::Trace};
use serde_json::Value;

fn recording() -> Trace {
    serde_json::from_str(include_str!("fixtures/run5-rehearsal.json")).unwrap()
}

#[test]
fn run8_aa_short_queue_passes_fidelity_while_run5_divergence_fails() {
    for (source, expected) in [
        (include_str!("fixtures/run8-aa-rehearsal.json"), "pass"),
        (include_str!("fixtures/run5-rehearsal.json"), "fail"),
    ] {
        let trace: Trace = serde_json::from_str(source).unwrap();
        let report = simulation::simulate(
            trace,
            simulation::Options {
                fidelity: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report["fidelity"]["status"], expected, "{report}");
        for queue in report["fidelity"]["queues"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|q| q["member"] == "mac-mini")
        {
            assert_eq!(queue["status"], expected, "{queue}");
            if expected == "pass" {
                assert_eq!(queue["exact_count"], 4);
                assert_eq!(queue["of"], 6);
                assert_eq!(queue["within_tolerance_count"], 6);
                assert_eq!(queue["minimum_matching_count"], 6);
                assert_eq!(queue["maximum_error"], 1);
                assert_eq!(queue["recorded_peak"], 6);
                assert_eq!(queue["simulated_peak"], 6);
            } else {
                assert_eq!(queue["recorded_peak"], 16);
                assert_eq!(queue["simulated_peak"], 4);
                assert_eq!(queue["maximum_error"], 13);
            }
        }
    }
}

#[test]
fn measured_cpu_cost_explains_the_real_rehearsals_queue_without_certifying_it() {
    let trace = recording();
    let original = serde_json::to_vec(&trace).unwrap();
    let baseline = simulation::simulate(
        trace.clone(),
        simulation::Options {
            fidelity: true,
            ..Default::default()
        },
    )
    .unwrap();
    let replay = simulation::rehearsal_cpu::replay(trace.clone()).unwrap();
    assert_eq!(serde_json::to_vec(&trace).unwrap(), original);
    assert!(
        baseline["fidelity"]["waits"]["median_absolute_error_ms"]
            .as_f64()
            .unwrap()
            > 90_000.0
    );
    assert!(
        replay["fidelity"]["waits"]["median_absolute_error_ms"]
            .as_f64()
            .unwrap()
            < 10_000.0
    );
    let peak = |report: &Value| {
        report["fidelity"]["queues"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["member"] == "mac-mini")
            .unwrap()["simulated_peak"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(peak(&baseline), 4);
    assert_eq!(peak(&replay), 15);
    assert_eq!(replay["fidelity"]["run_durations"]["status"], "pass");
    assert_eq!(replay["diagnostic"]["predictive"], false);
    assert_eq!(replay["fidelity"]["status"], "diagnostic");
    assert_eq!(replay["fidelity"]["trusted_for_tuning"], false);
    assert!(replay["certificate"].is_null());
}

#[test]
fn diagnostic_replay_requires_cpu_measurements_and_complete_evidence() {
    let mut trace = recording();
    trace.requests[0].fact.resources.as_mut().unwrap().cpu_ms = None;
    assert!(simulation::rehearsal_cpu::replay(trace).is_err());
    let mut trace = recording();
    trace.rehearsal.as_mut().unwrap().complete = false;
    assert!(simulation::rehearsal_cpu::replay(trace).is_err());
}

#[test]
fn incomplete_per_member_runtime_returns_an_error_instead_of_panicking() {
    let mut trace = recording();
    let id = trace.requests[0].fact.run_id.clone();
    let other = "macbook-pro".to_owned();
    let rec = trace.rehearsal.as_mut().unwrap();
    rec.demand.get_mut(&id).unwrap().insert(other.clone(), 1.0);
    rec.uncontended_ms
        .insert(id, std::collections::BTreeMap::from([(other, 1000.0)]));
    assert!(simulation::validate_trace(&trace).is_ok());
    assert!(simulation::rehearsal_cpu::replay(trace).is_err());
}

#[test]
fn diagnostic_replay_rejects_inconsistent_or_overflowing_core_counts() {
    let mut trace = recording();
    let cores = &mut trace.requests[0].decision.as_mut().unwrap().candidates[0]
        .load
        .as_mut()
        .unwrap()
        .cores;
    cores.performance = u32::MAX;
    cores.efficiency = 1;
    assert!(simulation::validate_trace(&trace).is_ok());
    assert!(simulation::rehearsal_cpu::replay(trace).is_err());
}
