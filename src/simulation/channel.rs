//! In-memory member communication implementing the production member port.
use super::{
    model,
    world::{CopyState, World},
};
use crate::{
    failure::{Failure, Result},
    scheduling::{
        placed::{Members, OfferReply, PlacementOffer, Standing, Warmth},
        quote::{CopyQuote, Quote, QuoteRequest},
        service::Scheduler,
    },
};
use futures_util::future::BoxFuture;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
};

pub(super) type Registry = Arc<Mutex<BTreeMap<String, Weak<Scheduler>>>>;

pub(super) struct Channel {
    pub(super) own: String,
    pub(super) master: Option<String>,
    pub(super) registry: Registry,
    pub(super) world: Arc<Mutex<World>>,
}
impl Channel {
    fn target(&self, member: &str) -> Result<Arc<Scheduler>> {
        let w = self.world.lock().unwrap();
        if model::bucket(&w.trace, member, w.clock.now().millis()).is_none() {
            return Err(Failure::new(
                "member_unavailable",
                "Member has no observation in this minute.",
                "Collect member availability history.",
            ));
        }
        drop(w);
        self.registry
            .lock()
            .unwrap()
            .get(member)
            .and_then(Weak::upgrade)
            .ok_or_else(|| crate::scheduling::placed::not_in_fleet(member))
    }
}
impl Members for Channel {
    fn standing(&self) -> Standing {
        Standing {
            name: Some(self.own.clone()),
            master: self.master.clone(),
            members: self.registry.lock().unwrap().keys().cloned().collect(),
        }
    }
    fn offer(
        &self,
        member: &str,
        offer: &PlacementOffer,
    ) -> BoxFuture<'static, Result<OfferReply>> {
        let target = self.target(member);
        let member = member.to_owned();
        let from = self.own.clone();
        let offer = offer.clone();
        let world = self.world.clone();
        Box::pin(async move {
            let target = target?;
            let key = (member, offer.workspace.root.clone());
            let copy = world
                .lock()
                .unwrap()
                .copies
                .get(&key)
                .cloned()
                .unwrap_or(CopyState {
                    warmth: Warmth::Cold,
                    busy: false,
                });
            let reply = target.accept_placed(
                &from,
                &offer,
                !copy.busy,
                "/virtual/copy",
                "mapped",
                copy.warmth,
            )?;
            if matches!(reply, OfferReply::Accepted { .. }) {
                world
                    .lock()
                    .unwrap()
                    .copies
                    .insert(key, CopyState { busy: true, ..copy });
            }
            Ok(reply)
        })
    }
    fn quote(&self, member: &str, request: &QuoteRequest) -> BoxFuture<'static, Result<Quote>> {
        let target = self.target(member);
        let member = member.to_owned();
        let asked = request.workspaces.clone();
        let world = self.world.clone();
        Box::pin(async move {
            let mut q = target?.quote_state();
            let w = world.lock().unwrap();
            q.copies = asked
                .iter()
                .map(|s| {
                    let c = w.copies.get(&(member.clone(), s.root.clone()));
                    CopyQuote {
                        workspace: s.root.clone(),
                        warmth: c.map_or(Warmth::Cold, |c| c.warmth),
                        busy: c.is_some_and(|c| c.busy),
                        unavailable: false,
                    }
                })
                .collect();
            Ok(q)
        })
    }
}
