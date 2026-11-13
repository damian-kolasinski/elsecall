//! Bounded file input and CLI routing for the offline tuner.
use crate::{
    evaluation_day::day,
    failure::{Failure, Result},
    tuning::{self, Space, TuneOptions},
    workload::input::read as read_trace,
};
use clap::{Args, Subcommand};
use std::{io::Read, path::PathBuf};
#[derive(Subcommand, Debug)]
pub enum TuneCommand {
    /// Offline, read-only readiness check; no rehearsal or installation
    Preflight(crate::tuning_preflight_cli::PreflightArgs),
}
#[derive(Args, Debug)]
#[command(subcommand_negates_reqs = true)]
pub struct TuneArgs {
    #[command(flatten)]
    pub replay_placement: crate::replay_placement::ReplayPlacement,
    /// Replay arrivals F times closer; preserve all command work (1–32)
    #[arg(long, default_value = "1")]
    pub load_factor: crate::replay_load::LoadFactor,
    #[command(subcommand)]
    pub command: Option<TuneCommand>,
    #[arg(long, required = true)]
    pub trace: Option<PathBuf>,
    /// JSON file mapping canonical setting keys to arrays of string values
    #[arg(long, value_name = "SPEC")]
    pub space: Option<PathBuf>,
    /// Maximum unique tuning simulations, including the baseline
    #[arg(long, default_value_t = 64)]
    pub budget: usize,
    #[arg(long, default_value_t = 0)]
    pub seed: u64,
    /// Number of alternatives to show (1–20)
    #[arg(long, default_value_t = 5)]
    pub top: usize,
    /// Tuning UTC day, retaining earlier arrivals as causal context
    #[arg(long)]
    pub day: Option<String>,
    /// Independent evaluation trace; never searched
    #[arg(long)]
    pub held_out: Option<PathBuf>,
    /// Independent UTC day in --held-out, or in --trace when omitted
    #[arg(long)]
    pub held_out_day: Option<String>,
}
pub fn run(args: TuneArgs) -> Result<serde_json::Value> {
    let mut trace = read_trace(
        &args
            .trace
            .ok_or_else(|| tuning::invalid("--trace is required."))?,
    )?;
    let placement = args.replay_placement.apply(&mut trace)?;
    let mut held = if let Some(path) = args.held_out {
        Some(read_trace(&path)?)
    } else if args.held_out_day.is_some() {
        Some(trace.clone())
    } else {
        None
    };
    if let Some(held) = &mut held {
        args.replay_placement.apply(held)?;
    }
    let space = args
        .space
        .map(|path| -> Result<Space> {
            let mut bytes = Vec::new();
            std::fs::File::open(path)
                .and_then(|f| f.take(65537).read_to_end(&mut bytes))
                .map_err(|e| Failure::io("Read tuning space", e))?;
            if bytes.len() > 65536 {
                return Err(tuning::invalid("Space exceeds 64 KiB."));
            }
            serde_json::from_slice(&bytes).map_err(|_| {
                tuning::invalid(
                    "Space must be a JSON object of canonical keys to arrays of string values.",
                )
            })
        })
        .transpose()?;
    let mut result = tuning::tune(
        trace,
        held,
        TuneOptions {
            load_factor: args.load_factor,
            space,
            budget: args.budget,
            seed: args.seed,
            top: args.top,
            day: day(args.day)?,
            held_out_day: day(args.held_out_day)?,
        },
    )?;
    result["placement_replay"] = placement;
    Ok(result)
}

/// Attach current fleet trust after the CPU-only search. A recorded-day replay
/// remains optional stronger evidence, but cannot bypass a drift revocation.
pub async fn current_trust(mut result: serde_json::Value) -> serde_json::Value {
    use serde_json::json;
    let Ok(paths) = crate::paths::Paths::current() else {
        return result;
    };
    if !paths.data.join("tuning-evidence.sqlite").exists() && !paths.socket().exists() {
        return result;
    }
    let evidence: crate::failure::Result<serde_json::Value> = crate::client::post(
        &paths.socket(),
        "/v1/tuning/trust",
        &json!({}),
        crate::client::Wait::Bounded,
    )
    .await;
    let mut evidence = evidence
        .unwrap_or_else(|e| json!({"trusted_for_tuning":false,"incomplete":true,"error":e.code}));
    let matches = certificate_build_matches(&evidence, &result["baseline"]["code_build"]);
    evidence["build_matches_tuner"] = json!(matches);
    let trusted = (evidence["trusted_for_tuning"] == true && matches)
        || (result["trusted_for_tuning"] == true
            && evidence["revoked"] != true
            && evidence["incomplete"] == false);
    result["trusted_for_tuning"] = json!(trusted);
    result["certificate_evidence"] = evidence;
    result["accepted_win"] = json!(false);
    result["proposal_status"] = json!(if trusted {
        "awaiting_ab_rehearsal"
    } else {
        "untrusted"
    });
    result["trust_missing"] = if trusted {
        json!([
            "Validate the proposal with rehearse --compare; the operator decides whether to apply it."
        ])
    } else {
        json!([
            "A current rehearsal certificate or optional complete-day certificate is required, with no drift revocation or unreachable member."
        ])
    };
    result
}

fn certificate_build_matches(evidence: &serde_json::Value, build: &serde_json::Value) -> bool {
    !build.is_null()
        && evidence["members"].as_array().is_some_and(|members| {
            !members.is_empty()
                && members
                    .iter()
                    .all(|m| m["evidence"]["certificate"]["build"] == *build)
        })
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn newer_tuner_cannot_use_old_services_current_certificates() {
        let old = json!({"version":"0.1.0","build":"0000000000000000"});
        let new = json!({"version":"0.1.0","build":"1111111111111111"});
        let evidence = json!({"trusted_for_tuning":true,"members":[{"evidence":{"certificate":{"build":old}}}]});
        assert!(certificate_build_matches(&evidence, &old));
        assert!(!certificate_build_matches(&evidence, &new));
        assert!(!certificate_build_matches(&json!({"members":[]}), &new));
    }
}
