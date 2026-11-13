//! CLI adapter for the offline readiness check. Collection uses read-only trace routes.
use crate::{
    failure::{Failure, Result},
    scheduling::timestamp::Timestamp,
};
use clap::Args;
use std::{num::NonZeroUsize, path::PathBuf, time::Duration};
#[derive(Args, Debug)]
pub struct PreflightArgs {
    #[command(flatten)]
    pub replay_placement: crate::replay_placement::ReplayPlacement,
    /// Complete installed-build rehearsal trace; repeat for each tested F
    #[arg(long, requires = "calibrate_busy_hours")]
    pub busy_rehearsal: Vec<PathBuf>,
    /// Match measured busy hours using complete observed rehearsal traces
    #[arg(long, requires_all = ["busy_since", "busy_until"], conflicts_with = "load_factor")]
    pub calibrate_busy_hours: bool,
    /// Busy-hour reference bounds in the full source history, RFC 3339
    #[arg(long, requires = "calibrate_busy_hours")]
    pub busy_since: Option<String>,
    #[arg(long, requires = "calibrate_busy_hours")]
    pub busy_until: Option<String>,
    /// Replay arrivals F times closer; preserve all command work (1–32)
    #[arg(long, default_value = "1")]
    pub load_factor: crate::replay_load::LoadFactor,
    /// Existing full-fleet export; when omitted, collect history read-only
    #[arg(long)]
    pub trace: Option<PathBuf>,
    /// RFC 3339 arrival start (or duration back when collecting)
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long)]
    pub until: Option<String>,
    /// busiest:DURATION or explicit (uses the given/source bounds)
    #[arg(long, default_value = "busiest:50m")]
    pub rehearsal_window: crate::rehearsal::window::Busiest,
    #[arg(long, default_value = "40")]
    pub min_interactive: NonZeroUsize,
    #[arg(long, default_value_t = 6.0)]
    pub compress: f64,
    #[arg(long, default_value = "15m", value_parser = humantime::parse_duration)]
    pub limit: Duration,
    /// Small offline search sample (baseline included), 2–16
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u8).range(2..=16))]
    pub sample: u8,
    /// Budget used to extrapolate full iteration search time
    #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u16).range(1..=1024))]
    pub iteration_budget: u16,
    #[arg(long, default_value_t = 42)]
    pub seed: u64,
}
pub async fn run(args: PreflightArgs) -> Result<serde_json::Value> {
    let mut trace = if let Some(path) = &args.trace {
        crate::workload::input::read(path)?
    } else {
        let now = Timestamp::now();
        let start = crate::report_cli::since(args.since.as_deref().unwrap_or("7d"), now)?;
        // Collect through now, so every historical drain and sensitivity trial
        // can use the available evidence beyond the arrival cutoff.
        crate::trace_cli::collect(&crate::paths::Paths::current()?, start, now).await?
    };
    crate::simulation::validate_trace(&trace)?;
    let placement = args.replay_placement.apply(&mut trace)?;
    let busy_hours = if args.calibrate_busy_hours {
        Some(crate::busy_hour_calibration::Target::observed(
            &trace,
            Timestamp::parse_rfc3339(args.busy_since.as_deref().expect("required busy bound"))
                .ok_or_else(|| crate::tuning::invalid("--busy-since must be RFC 3339."))?,
            Timestamp::parse_rfc3339(args.busy_until.as_deref().expect("required busy bound"))
                .ok_or_else(|| crate::tuning::invalid("--busy-until must be RFC 3339."))?,
        )?)
    } else {
        None
    };
    let observed_rehearsals = args
        .busy_rehearsal
        .iter()
        .map(|p| crate::workload::input::read(p))
        .collect::<Result<Vec<_>>>()?;
    let start_text = if args.trace.is_some() {
        args.since.as_deref()
    } else {
        None
    };
    let (start, end) = crate::workload::bounds::within(&trace, start_text, args.until.as_deref())?;
    let trace = crate::rehearsal::window::bound(&trace, start, end)?;
    let mut result = tokio::task::spawn_blocking(move || {
        crate::tuning_preflight::check(
            trace,
            crate::tuning_preflight::Options {
                busy_hours,
                observed_rehearsals,
                load_factor: args.load_factor,
                rehearsal_window: args.rehearsal_window,
                min_interactive: args.min_interactive,
                compress: args.compress,
                limit: args.limit,
                sample: args.sample,
                iteration_budget: args.iteration_budget,
                seed: args.seed,
            },
        )
    })
    .await
    .map_err(|_| {
        Failure::new(
            "preflight_failed",
            "Offline preflight task failed.",
            "Check the trace and retry.",
        )
    })??;
    result["placement_replay"] = placement;
    Ok(result)
}
