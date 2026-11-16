//! Thin same-user onboarding API client. It never reads a member key.
use crate::{client, failure::Result, onboarding_id::OperationId, paths::Paths};
use clap::{Args, Subcommand};
use hyper::Method;
use serde_json::Value;
use std::time::Duration;

#[derive(Args)]
pub struct OnboardingArgs {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Prepare an onboarding transfer, or report why no installer can be sent
    Prepare,
    /// Read an operation after an interruption or a service restart
    Status { id: String },
    /// Cancel an operation; repeated cancellation is safe
    Cancel { id: String },
}
pub async fn run(paths: &Paths, args: OnboardingArgs) -> Result<(Value, bool)> {
    let (method, route, body) = match args.command {
        Command::Prepare => (
            Method::POST,
            "/v1/onboarding/prepare".to_owned(),
            Some(serde_json::json!({})),
        ),
        Command::Status { id } => {
            let id = OperationId::try_from(id)?;
            (Method::GET, format!("/v1/onboarding/{}", id.as_str()), None)
        }
        Command::Cancel { id } => {
            let id = OperationId::try_from(id)?;
            (
                Method::POST,
                format!("/v1/onboarding/{}/cancel", id.as_str()),
                Some(serde_json::json!({})),
            )
        }
    };
    let data: Value = client::send(
        &paths.socket(),
        method,
        &route,
        body,
        Duration::from_secs(5),
    )
    .await?;
    let complete = data["stage"] != "blocked" && data["stage"] != "expired";
    Ok((data, complete))
}
