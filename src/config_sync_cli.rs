//! `elsecall sync` commands: thin requests to the local service.
use crate::{
    client,
    config_sync::synced_path::SyncedPath,
    config_sync_local_api::{HistoryRequest, PathRequest, PlanRequest, PushRequest},
    failure::{Failure, Result},
    paths::Paths,
};
use clap::{Args, Subcommand};
use serde_json::Value;

#[derive(Args)]
pub struct SyncArgs {
    #[command(subcommand)]
    command: SyncCommand,
}

#[derive(Subcommand)]
enum SyncCommand {
    /// Review service settings from fleet-config.toml; confirm to enable them
    Service {
        #[arg(long)]
        confirm: Option<String>,
    },
    /// Show every member's sync state: in sync, pending, overridden, locally
    /// edited and backed up
    Status,
    /// Dry run on the master: how syncing each declared path would change each
    /// member, per file, by fingerprints only
    Plan {
        /// Paths to plan, as ~/…; by default every declared or enabled path
        paths: Vec<String>,
    },
    /// Start syncing a path declared in [sync] paths. Without --confirm, shows the
    /// plan and its ID; with it, enables exactly that plan
    Enable {
        path: String,
        /// The plan ID shown by sync plan or sync enable
        #[arg(long)]
        confirm: Option<String>,
    },
    /// Stop syncing a path; every member keeps its files. Without --confirm, shows
    /// what happens and the ID to confirm
    Disable {
        path: String,
        #[arg(long)]
        confirm: Option<String>,
    },
    /// Push the master's current files now (asks the master): it issues any change,
    /// then pushes to every member, or with --member only to that one, and only if
    /// it differs
    Push {
        #[arg(long)]
        member: Option<crate::roster::MemberName>,
    },
    /// This member's records of what synced: paths, sizes, fingerprints, outcomes
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

fn path(text: &str) -> Result<SyncedPath> {
    SyncedPath::parse(text).map_err(|why| {
        Failure::new(
            "invalid_request",
            format!("Invalid sync path: {why}."),
            "Write paths relative to home, such as ~/.config/app; see README \"Configuration sync\".",
        )
    })
}

/// Runs a sync command; returns the data to print and whether it fully succeeded.
pub async fn run(paths: &Paths, args: SyncArgs) -> Result<(Value, bool)> {
    let socket = paths.socket();
    let data: Value = match args.command {
        SyncCommand::Service { confirm } => {
            client::post_large(
                &socket,
                "/v1/sync/service",
                &serde_json::json!({"confirm": confirm}),
            )
            .await?
        }
        SyncCommand::Status => {
            client::post_large(&socket, "/v1/sync", &serde_json::json!({})).await?
        }
        SyncCommand::Plan { paths } => {
            let paths = paths.iter().map(|p| path(p)).collect::<Result<_>>()?;
            client::post_large(&socket, "/v1/sync/plan", &PlanRequest { paths }).await?
        }
        SyncCommand::Enable {
            path: text,
            confirm,
        } => {
            let request = PathRequest {
                path: path(&text)?,
                confirm,
            };
            client::post_large(&socket, "/v1/sync/enable", &request).await?
        }
        SyncCommand::Disable {
            path: text,
            confirm,
        } => {
            let request = PathRequest {
                path: path(&text)?,
                confirm,
            };
            client::post_large(&socket, "/v1/sync/disable", &request).await?
        }
        SyncCommand::Push { member } => {
            client::post_large(&socket, "/v1/sync/push", &PushRequest { member }).await?
        }
        SyncCommand::History { limit } => {
            client::post_large(&socket, "/v1/sync/history", &HistoryRequest { limit }).await?
        }
    };
    let complete = data
        .get("complete")
        .or_else(|| data.get("round").and_then(|r| r.get("complete")))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    Ok((data, complete))
}
