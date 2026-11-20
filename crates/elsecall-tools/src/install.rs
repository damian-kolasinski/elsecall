//! Installing a member's plan: the steps the master sends, carried out on the
//! member as its own admitted work. See `docs/fleet-provisioning.md`,
//! "Delivery 4: installing".
//!
//! [`Installer`] is the installing half of the `ToolManager` port: each manager
//! installs one version of one tool, running every command it needs through
//! [`Work`], which binds the command to the step's admitted run. [`applier`]
//! runs a request's steps one at a time, each admitted through this member's
//! scheduler ([`InstallAdmission`]) like any job, and keeps the latest apply
//! ([`record`]). Nothing is ever uninstalled.
pub(crate) mod applier;
pub(crate) mod mise;
pub(crate) mod record;
pub(crate) mod request;
mod work;

use crate::tool_inventory::Manager;
use elsecall_contract::{
    failure::Result,
    scheduling_model::{request::AdmissionRequest, run_id::RunId},
};
use elsecall_scheduling::{messages::RunEnded, service::Scheduler};
use futures_util::future::BoxFuture;
use request::InstallStep;
use std::{process::ExitStatus, sync::Arc, time::Duration};

pub use applier::Applier;

/// The installing half of a tool manager.
pub(crate) trait Installer: Send + Sync {
    fn manager(&self) -> Manager;
    /// Installs `step`, running each command it needs through `work`.
    fn install<'a>(
        &'a self,
        step: &'a InstallStep,
        work: &'a dyn Work,
    ) -> BoxFuture<'a, std::result::Result<(), StepFailure>>;
}

/// Runs one command of a step as part of the step's admitted run.
pub(crate) trait Work: Send + Sync {
    /// Starts `command` with no input and its output discarded, binds it to the
    /// run, and waits for it; past `limit` its process group is killed.
    fn run(&self, command: tokio::process::Command, limit: Duration) -> BoxFuture<'_, Ran>;
}

/// How a command ended.
#[derive(Debug)]
pub(crate) enum Ran {
    Ended(ExitStatus),
    TimedOut,
    NotStarted,
}

/// The stage of a step that failed, and why, as a code. Never what a command
/// printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StepFailure {
    pub(crate) stage: &'static str,
    pub(crate) code: &'static str,
}

/// How installs ask this member's scheduler for room, as its own work.
pub trait InstallAdmission: Send + Sync {
    /// Waits until the request is admitted; fails with `deadline_skipped`,
    /// `stopping` and the like.
    fn admit(&self, request: AdmissionRequest) -> BoxFuture<'_, Result<RunId>>;
    /// Binds a command this process started to the run, as its holder.
    fn started(&self, id: &RunId, pid: u32);
    fn end(&self, id: &RunId, ended: RunEnded);
}

/// The member's own scheduler, asked as the service's own process.
impl InstallAdmission for Arc<Scheduler> {
    fn admit(&self, request: AdmissionRequest) -> BoxFuture<'_, Result<RunId>> {
        Box::pin(async move {
            self.request(std::process::id(), request)
                .await
                .map(|grant| grant.run_id)
        })
    }

    fn started(&self, id: &RunId, pid: u32) {
        if let Err(error) = self.command_started(
            std::process::id(),
            id,
            elsecall_scheduling::messages::CommandStarted { pid },
        ) {
            elsecall_host::service_log!(
                "{}",
                serde_json::json!({"event": "tools_install_unbound", "detail": error})
            );
        }
    }

    fn end(&self, id: &RunId, ended: RunEnded) {
        if let Err(error) = self.run_ended(std::process::id(), id, ended) {
            elsecall_host::service_log!(
                "{}",
                serde_json::json!({"event": "tools_install_unrecorded", "detail": error})
            );
        }
    }
}
