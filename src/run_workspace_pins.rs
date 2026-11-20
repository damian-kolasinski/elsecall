//! The workspaces this member's runs started in, as the tool plan reads them:
//! the runs' record (`elsecall_runs::run_workspaces`) behind the tools' port
//! (`elsecall_tools::plan_api::RecentWorkspaces`). Neither slice knows the other.
use elsecall_contract::scheduling_model::timestamp::Timestamp;
use elsecall_runs::run_workspaces::RunWorkspaces;
use elsecall_tools::plan_api::{RecentWorkspace, RecentWorkspaces};
use std::sync::Arc;

pub(crate) struct RunWorkspacePins(pub(crate) Arc<RunWorkspaces>);

impl RecentWorkspaces for RunWorkspacePins {
    fn recent(&self) -> Vec<RecentWorkspace> {
        self.0
            .recent(Timestamp::now())
            .into_iter()
            .map(|workspace| RecentWorkspace {
                root: workspace.root,
                last_run_at: workspace.last_run_at,
            })
            .collect()
    }
}
