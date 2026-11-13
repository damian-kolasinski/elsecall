//! Placement's view of the fleet: which members exist, who is master, and how an
//! offer reaches a member. The adapter of `scheduling::placed::Members` over the
//! authenticated member API.
use crate::{
    failure::Result,
    fleet::Fleet,
    peer_client,
    scheduling::{
        placed::{Members, OfferReply, PlacementOffer, Standing},
        quote::{Quote, QuoteRequest},
    },
};
use futures_util::future::BoxFuture;
use hyper::Method;
use std::{sync::Arc, time::Duration};

/// How long an offer may take; a member under heavy load answers slowly.
const OFFER_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a quote may take. A member that cannot answer this quickly is left
/// out of the decision rather than delaying it.
const QUOTE_TIMEOUT: Duration = Duration::from_secs(6);

pub struct FleetMembers {
    /// `None` when the fleet state could not be opened; the member is then alone.
    fleet: Option<Arc<Fleet>>,
}

impl FleetMembers {
    pub fn new(fleet: Option<Arc<Fleet>>) -> Self {
        Self { fleet }
    }
}

impl Members for FleetMembers {
    fn standing(&self) -> Standing {
        let Some(roster) = self.fleet.as_ref().and_then(|fleet| fleet.roster().ok()) else {
            return Standing::default();
        };
        let fleet = self.fleet.as_ref().expect("a roster came from the fleet");
        Standing {
            name: Some(fleet.own_name(&roster).to_string()),
            master: Some(roster.master().name.to_string()),
            members: roster
                .members()
                .iter()
                .map(|m| m.name.to_string())
                .collect(),
        }
    }

    fn offer(
        &self,
        member: &str,
        offer: &PlacementOffer,
    ) -> BoxFuture<'static, Result<OfferReply>> {
        self.call(member, "/peer/v1/placements", offer.clone(), OFFER_TIMEOUT)
    }

    fn quote(&self, member: &str, request: &QuoteRequest) -> BoxFuture<'static, Result<Quote>> {
        self.call(
            member,
            "/peer/v1/placements/quote",
            request.clone(),
            QUOTE_TIMEOUT,
        )
    }
}

impl FleetMembers {
    /// A signed request to `member`, answered by a verified response.
    fn call<B, T>(
        &self,
        member: &str,
        path: &'static str,
        body: B,
        timeout: Duration,
    ) -> BoxFuture<'static, Result<T>>
    where
        B: serde::Serialize + Send + Sync + 'static,
        T: serde::de::DeserializeOwned + Send + 'static,
    {
        let fleet = self.fleet.clone();
        let member = member.to_owned();
        Box::pin(async move {
            let fleet = fleet.ok_or_else(|| crate::scheduling::placed::not_in_fleet(&member))?;
            let roster = fleet.roster()?;
            let target = roster
                .members()
                .iter()
                .find(|m| m.name.as_str() == member)
                .ok_or_else(|| crate::scheduling::placed::not_in_fleet(&member))?
                .clone();
            peer_client::call(
                fleet.key(),
                &target,
                Method::POST,
                path,
                Some(&body),
                timeout,
            )
            .await
        })
    }
}
