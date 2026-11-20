//! `elsecall sync` commands: thin requests to the local service.
use crate::config_sync_local_api::{
    ConsentRequest, HistoryRequest, PathRequest, PlanRequest, PushRequest,
};
use clap::{Args, Subcommand};
use elsecall_contract::failure::{Failure, Result};
use elsecall_fleet::config_sync_model::synced_path::SyncedPath;
use elsecall_host::client;
use elsecall_host::paths::Paths;
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
    /// Confirm syncing a change to files that look like secrets. Without
    /// --confirm, shows what waits and its ID. Only a person confirms: with
    /// --confirm, this needs a terminal
    Consent {
        path: String,
        /// The consent ID shown by sync consent or the attention item
        #[arg(long)]
        confirm: Option<String>,
        /// Sync changes to these files automatically from now on
        #[arg(long, requires = "confirm")]
        always: bool,
    },
    /// Push the master's current files now (asks the master): it issues any change,
    /// then pushes to every member, or with --member only to that one, and only if
    /// it differs
    Push {
        #[arg(long)]
        member: Option<elsecall_peers::roster::MemberName>,
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

/// Refuses to confirm consent unless a terminal is attached to standard input
/// and error, as when a person types the command. An agent's tool calls run
/// without one, so they stop here; this is a guard against confirming by
/// accident, not a security boundary, since anything running as the same user
/// can attach a terminal or call the service directly.
fn person_present() -> Result<()> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
        return Ok(());
    }
    Err(Failure::new(
        "person_required",
        "Only a person confirms syncing files that look like secrets, from their own terminal.",
        "Ask the operator to review the files (elsecall sync consent <path>) and run the confirm command themselves.",
    ))
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
        SyncCommand::Consent {
            path: text,
            confirm,
            always,
        } => {
            if confirm.is_some() {
                person_present()?;
            }
            let request = ConsentRequest {
                path: path(&text)?,
                confirm,
                always,
            };
            client::post_large(&socket, "/v1/sync/consent", &request).await?
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
