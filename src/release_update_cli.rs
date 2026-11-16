//! Release updates for operators and scripts; the service owns the durable job.
use crate::{
    client,
    failure::{Failure, Result},
    paths::Paths,
    release_update::{Job, Phase, Status},
};
use clap::{Args, Subcommand};
use std::time::Duration;

#[derive(Args)]
pub struct UpdateArgs {
    #[command(subcommand)]
    command: UpdateCommand,
}
#[derive(Subcommand)]
enum UpdateCommand {
    /// Check the published stable release channel (never installs)
    Check,
    /// Read cached release information and the latest update job
    Status,
    /// Install the newest stable release, waiting for active work to finish
    Install {
        #[arg(long,default_value="5m",value_parser=humantime::parse_duration)]
        wait: Duration,
        /// Reopen the native window after the service is ready
        #[arg(long)]
        reopen: bool,
        /// Return the durable job immediately rather than waiting for it
        #[arg(long)]
        no_wait: bool,
    },
}
pub async fn run(paths: &Paths, args: UpdateArgs) -> Result<(serde_json::Value, bool)> {
    match args.command {
        UpdateCommand::Check => {
            let status = check(paths).await?;
            let complete = status.check_error.is_none();
            Ok((
                serde_json::to_value(status).expect("update status"),
                complete,
            ))
        }
        UpdateCommand::Status => Ok((
            serde_json::to_value(status(paths).await?).expect("update status"),
            true,
        )),
        UpdateCommand::Install {
            wait,
            reopen,
            no_wait,
        } => {
            if wait > Duration::from_secs(3600) {
                return Err(crate::release_update::invalid(
                    "An update may wait for work for at most one hour.",
                ));
            }
            let discovery = check(paths).await?;
            if let Some(error) = discovery.check_error {
                return Err(error);
            }
            if discovery.available.is_none() {
                return Ok((
                    serde_json::json!({"status":"up_to_date","current_version":discovery.current_version}),
                    true,
                ));
            }
            let job: Job = client::post_within(
                &paths.socket(),
                "/v1/update/install",
                serde_json::json!({"reopen":reopen,"wait_seconds":wait.as_secs()}),
                Duration::from_secs(30),
            )
            .await?;
            if no_wait {
                return Ok((serde_json::to_value(job).expect("job"), true));
            }
            let deadline = tokio::time::Instant::now() + Duration::from_secs(wait.as_secs() + 600);
            loop {
                if let Ok(state) = status(paths).await {
                    let Some(latest) = state.job else {
                        return Err(crate::release_update::invalid(
                            "The update job disappeared.",
                        ));
                    };
                    if latest.id != job.id {
                        return Err(crate::release_update::invalid(
                            "Another job replaced the requested update.",
                        ));
                    }
                    if latest.phase.terminal() {
                        let success = matches!(latest.phase, Phase::Succeeded);
                        return Ok((serde_json::to_value(latest).expect("job"), success));
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(Failure::new(
                        "update_timeout",
                        "The update is still running or the service has not reconnected.",
                        "Run elsecall update status to follow the durable job.",
                    ));
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}
pub async fn status(paths: &Paths) -> Result<Status> {
    client::get(&paths.socket(), "/v1/update").await
}
pub async fn check(paths: &Paths) -> Result<Status> {
    client::post_within(
        &paths.socket(),
        "/v1/update/check",
        serde_json::json!({}),
        Duration::from_secs(40),
    )
    .await
}
