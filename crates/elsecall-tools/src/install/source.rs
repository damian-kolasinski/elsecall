//! Where a member copies what it installs from: the master it records.
use crate::{
    artifact::{self, Artifact},
    artifact_api,
};
use elsecall_fleet::fleet::Fleet;
use futures_util::future::BoxFuture;
use std::sync::Arc;
use tokio::io::AsyncWrite;

/// The port: copying an artifact from the member that has it.
pub(crate) trait Source: Send + Sync {
    /// Writes `artifact` into `sink`, whole and at most `limit` bytes;
    /// otherwise the failure's code.
    fn fetch<'a>(
        &'a self,
        artifact: &'a Artifact,
        sink: &'a mut (dyn AsyncWrite + Unpin + Send),
        limit: u64,
    ) -> BoxFuture<'a, Result<u64, &'static str>>;

    /// Whether this member is the master itself, which has no one to copy from.
    fn master_is_self(&self) -> bool;
}

/// The master this member records, over the members' channel.
pub(crate) struct MasterSource(pub(crate) Result<Arc<Fleet>, elsecall_contract::failure::Failure>);

impl Source for MasterSource {
    fn master_is_self(&self) -> bool {
        match &self.0 {
            Err(_) => true,
            Ok(fleet) => fleet
                .roster()
                .map_or(true, |roster| roster.master().key == fleet.public_key()),
        }
    }

    fn fetch<'a>(
        &'a self,
        artifact: &'a Artifact,
        sink: &'a mut (dyn AsyncWrite + Unpin + Send),
        limit: u64,
    ) -> BoxFuture<'a, Result<u64, &'static str>> {
        Box::pin(async move {
            let fleet = self.0.as_ref().map_err(|_| "artifact_unreachable")?;
            let roster = fleet.roster().map_err(|_| "artifact_unreachable")?;
            let master = roster.master();
            if master.key == fleet.public_key() {
                return Err("artifact_on_master");
            }
            let mut channel =
                artifact_api::open(fleet, master, artifact)
                    .await
                    .map_err(|error| {
                        if error.code == "artifact_unavailable" {
                            "artifact_unavailable"
                        } else {
                            "artifact_unreachable"
                        }
                    })?;
            artifact::receive(&mut channel, sink, limit)
                .await
                .map_err(|_| "artifact_cut")
        })
    }
}
