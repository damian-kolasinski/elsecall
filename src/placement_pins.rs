//! Placement by pins: the fleet's members as placement reaches them
//! (`elsecall_fleet::fleet_placement`), with what each lacks of a workspace's
//! pins from the master's provisioning loop (`elsecall_tools`). Neither slice
//! knows the other. The loop starts after the scheduler, which it installs
//! through, so it is handed over once it exists; until then nothing is known
//! to be lacking.
use elsecall_contract::failure::Result;
use elsecall_scheduling::{
    pin_fit::Lacking,
    placed::{Members, OfferReply, PlacementOffer, Standing, WorkspaceRef},
    quote::{Quote, QuoteRequest},
};
use elsecall_tools::provisioning_loop::ProvisioningLoop;
use futures_util::future::BoxFuture;
use std::{
    path::Path,
    sync::{Arc, OnceLock},
};

pub(crate) struct PinnedMembers {
    pub(crate) members: Arc<dyn Members>,
    pub(crate) provisioning: OnceLock<Arc<ProvisioningLoop>>,
}

impl Members for PinnedMembers {
    fn standing(&self) -> Standing {
        self.members.standing()
    }

    fn offer(
        &self,
        member: &str,
        offer: &PlacementOffer,
    ) -> BoxFuture<'static, Result<OfferReply>> {
        self.members.offer(member, offer)
    }

    fn quote(&self, member: &str, request: &QuoteRequest) -> BoxFuture<'static, Result<Quote>> {
        self.members.quote(member, request)
    }

    fn lacking(&self, workspace: &WorkspaceRef) -> Lacking {
        self.provisioning
            .get()
            .map(|provisioning| provisioning.lacking(Path::new(&workspace.root)))
            .unwrap_or_default()
    }
}
