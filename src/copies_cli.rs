//! `elsecall copies`: this member's workspace copies, and housekeeping on demand.
use crate::{client, copies_api::PruneRequest, failure::Result, paths::Paths};
use clap::{Args, Subcommand};
use serde_json::Value;

#[derive(Args)]
pub struct CopiesArgs {
    #[command(subcommand)]
    pub command: Option<CopiesCommand>,
}

#[derive(Subcommand)]
pub enum CopiesCommand {
    /// Run a housekeeping pass now: remove copies whose time has come, evict idle
    /// copies under disk pressure, and remove object stores no copy uses
    Prune {
        /// Report what the pass would remove, changing nothing
        #[arg(long)]
        dry_run: bool,
    },
}

/// Runs the command; the flag says whether it completed without problems.
pub async fn run(paths: &Paths, args: CopiesArgs) -> Result<(Value, bool)> {
    match args.command {
        None => Ok((
            client::get_large(&paths.socket(), "/v1/copies").await?,
            true,
        )),
        Some(CopiesCommand::Prune { dry_run }) => {
            let report: Value = client::post_large(
                &paths.socket(),
                "/v1/copies/prune",
                &PruneRequest { dry_run },
            )
            .await?;
            let clean = report["problems"].as_array().is_none_or(Vec::is_empty);
            Ok((report, clean))
        }
    }
}
