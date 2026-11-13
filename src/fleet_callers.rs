//! Housekeeping's view of the fleet: which members exist, and how a question
//! reaches the member that owns copies here. The adapter of
//! `housekeeping::callers::Callers` over the authenticated member API.
use crate::{
    failure::Result,
    fleet::Fleet,
    housekeeping::{
        callers::Callers,
        exchange::{Answer, Question, ROUTE},
    },
    peer_client,
};
use futures_util::future::BoxFuture;
use hyper::Method;
use std::{sync::Arc, time::Duration};

/// How long a member may take to answer; it checks paths on its own disk.
const ASK_TIMEOUT: Duration = Duration::from_secs(20);

pub struct FleetCallers {
    fleet: Arc<Fleet>,
}

impl FleetCallers {
    pub fn new(fleet: Arc<Fleet>) -> Self {
        Self { fleet }
    }
}

impl Callers for FleetCallers {
    fn members(&self) -> Option<Vec<String>> {
        let roster = self.fleet.roster().ok()?;
        Some(
            roster
                .members()
                .iter()
                .map(|m| m.name.to_string())
                .collect(),
        )
    }

    fn ask(&self, member: &str, question: Question) -> BoxFuture<'static, Result<Answer>> {
        let fleet = self.fleet.clone();
        let member = member.to_owned();
        Box::pin(async move {
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
                ROUTE,
                Some(&question),
                ASK_TIMEOUT,
            )
            .await
        })
    }
}
