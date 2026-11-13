//! How housekeeping reaches the calling members that own this member's copies: a
//! port, with the authenticated member API as its adapter (`fleet_callers`).
use crate::{
    failure::Result,
    housekeeping::exchange::{Answer, Question},
};
use futures_util::future::BoxFuture;

pub trait Callers: Send + Sync {
    /// The fleet's members by name, when this member knows its fleet.
    fn members(&self) -> Option<Vec<String>>;
    /// Asks `member`, which owns copies here, about its workspaces.
    fn ask(&self, member: &str, question: Question) -> BoxFuture<'static, Result<Answer>>;
}

/// A member outside any fleet: nobody to ask.
pub struct Nobody;
impl Callers for Nobody {
    fn members(&self) -> Option<Vec<String>> {
        None
    }
    fn ask(&self, member: &str, _: Question) -> BoxFuture<'static, Result<Answer>> {
        let failure = crate::scheduling::placed::not_in_fleet(member);
        Box::pin(async move { Err(failure) })
    }
}
