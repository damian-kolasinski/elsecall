//! Offline simulation never connects to, installs or changes an Elsecall service.
use crate::{
    evaluation_day::day,
    failure::Result,
    simulation::{self, Options},
    workload::input::read as read_trace,
};
use clap::Args;
use std::path::PathBuf;
#[derive(Args, Debug)]
pub struct SimulateArgs {
    #[command(flatten)]
    pub replay_placement: crate::replay_placement::ReplayPlacement,
    /// Diagnose a recorded rehearsal using measured CPU costs; never grants tuning trust
    #[arg(long, conflicts_with_all = ["fidelity", "settings", "load_factor", "day"])]
    pub rehearsal_cpu_replay: bool,
    /// Replay arrivals F times closer; preserve all command work (1–32)
    #[arg(long, default_value = "1")]
    pub load_factor: crate::replay_load::LoadFactor,
    #[arg(long)]
    pub trace: PathBuf,
    /// Override recorded scheduling parameters; repeat for multiple settings
    #[arg(long = "set", value_name = "KEY=VALUE")]
    pub settings: Vec<String>,
    #[arg(long, default_value_t = 0)]
    pub seed: u64,
    /// Validate the busiest finished UTC day using its recorded settings
    #[arg(long, conflicts_with = "settings")]
    pub fidelity: bool,
    /// Evaluate this UTC day (YYYY-MM-DD), keeping earlier trace arrivals as context
    #[arg(long)]
    pub day: Option<String>,
}
pub fn run(args: SimulateArgs) -> Result<serde_json::Value> {
    let mut trace = read_trace(&args.trace)?;
    if args.replay_placement.placement == crate::replay_placement::Mode::Current
        && (args.fidelity || args.rehearsal_cpu_replay)
    {
        return Err(crate::tuning::invalid(
            "Current placement is a counterfactual; historical fidelity and CPU diagnosis require historical placement.",
        ));
    }
    let placement = args.replay_placement.apply(&mut trace)?;
    if args.rehearsal_cpu_replay {
        return simulation::rehearsal_cpu::replay(trace);
    }
    let day = day(args.day)?;
    let mut result = simulation::simulate(
        trace,
        Options {
            load_factor: args.load_factor,
            settings: args.settings,
            seed: args.seed,
            fidelity: args.fidelity,
            day,
        },
    )?;
    if args.replay_placement.placement == crate::replay_placement::Mode::Current {
        result["placement_replay"] = placement;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_major_is_rejected_before_requiring_its_schema_and_errors_omit_input() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("trace.json");
        let args = || SimulateArgs {
            replay_placement: Default::default(),
            rehearsal_cpu_replay: false,
            load_factor: Default::default(),
            trace: file.clone(),
            settings: vec![],
            seed: 0,
            fidelity: false,
            day: None,
        };
        std::fs::write(&file, r#"{"trace_version":2,"private":"SECRET_SENTINEL"}"#).unwrap();
        let error = run(args()).unwrap_err();
        assert_eq!(error.code, "unsupported_trace_version");
        assert!(!error.message.contains("SECRET_SENTINEL"));
        std::fs::write(&file, r#"{"trace_version":1,"private":"SECRET_SENTINEL"}"#).unwrap();
        let error = run(args()).unwrap_err();
        assert_eq!(error.code, "invalid_trace");
        assert!(!error.message.contains("SECRET_SENTINEL"));
    }
}
