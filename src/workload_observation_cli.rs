//! A wrapper's one terminal assertion, posted from inside its admitted command.
use crate::{
    client::{self, Wait},
    failure::Result,
    paths::Paths,
    scheduling::{request::invalid, run_id::RunId},
    workload_metadata::parse_observation,
};
use clap::{Args, Subcommand};

#[derive(Args, Debug)]
pub struct WorkloadArgs {
    #[command(subcommand)]
    pub command: WorkloadCommand,
}
#[derive(Subcommand, Debug)]
pub enum WorkloadCommand {
    /// Record one terminal plan observation for this command's inherited grant
    Observe {
        /// Bounded JSON observation; never command arguments, environment or secrets
        #[arg(long, value_name = "JSON")]
        observation: String,
    },
}

pub async fn run(args: WorkloadArgs, paths: &Paths) -> Result<serde_json::Value> {
    let WorkloadCommand::Observe { observation } = args.command;
    let observation = parse_observation(&observation)?;
    // This is an opt-out/context check only. PID/grant authorization is always
    // enforced by the server; an inherited outer ID is unsafe in bypass modes.
    if !matches!(
        std::env::var(crate::run_admission::VARIABLE).as_deref(),
        Ok("granted" | "nested")
    ) {
        return Err(invalid(
            "Observe requires granted or nested admission context; bypassed or unmanaged work cannot report an inherited run ID.",
        ));
    }
    let id = std::env::var("ELSECALL_RUN_ID")
        .ok()
        .and_then(|id| RunId::parse(&id))
        .ok_or_else(|| {
            invalid("Observe must run inside an admitted command with ELSECALL_RUN_ID.")
        })?;
    let route = format!("/v1/runs/{id}/observation");
    // A child can reach the service before its launching executor finishes the
    // command binding. Retry only this assertion, never admission or execution.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        let result = tokio::time::timeout_at(deadline,
            client::post::<(), _>(&paths.socket(), &route, &observation, Wait::Bounded)
        ).await.map_err(|_| crate::failure::Failure::new(
            "observation_timeout", "The observation acknowledgement did not arrive within one second.",
            "Inspect the run record; an unacknowledged assertion may already have been recorded."
        ))?;
        match result {
            Err(failure)
                if failure.code == "not_found" && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep_until(
                    (tokio::time::Instant::now() + std::time::Duration::from_millis(20))
                        .min(deadline),
                )
                .await;
                if tokio::time::Instant::now() >= deadline {
                    return Err(failure);
                }
            }
            result => {
                result?;
                return Ok(serde_json::json!({"run_id": id, "recorded": true}));
            }
        }
    }
}
