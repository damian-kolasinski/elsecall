//! Where a request runs. Only the master places work on other members. Placeable
//! work runs here when it cannot be placed: on any other member, in a fleet of one,
//! or outside a git working tree, since placement moves a working tree. Work
//! required on a named member is placed there with or without a working tree: one
//! requested outside a tree runs in the member's home directory, with no copy.
use super::Scheduler;
use elsecall_contract::failure::{Failure, Result};
use elsecall_contract::scheduling_model::request::{Placement, WorkRequest};

/// Where a request runs.
pub(super) enum Route {
    /// On this member, with a note saying why when the caller asked for more.
    Here(Option<String>),
    /// On the named member, placed by this master, in its copy of the request's
    /// working tree or, without one, in its home directory.
    There(String),
    /// On the member with the earliest expected completion, chosen by this master.
    Auto,
}

const NOT_MASTER: &str = "and only the master places work: the request runs on the calling member.";

impl Scheduler {
    pub(super) fn route(&self, request: &WorkRequest) -> Result<Route> {
        let standing = self.members.standing();
        let member = match &request.placement {
            Placement::Pinned => return Ok(Route::Here(None)),
            Placement::Placeable => {
                let note = if standing
                    .members
                    .iter()
                    .all(|m| Some(m) == standing.name.as_ref())
                {
                    "This member is in no fleet with other members, so placeable work runs on it."
                        .to_owned()
                } else if standing.master != standing.name {
                    format!(
                        "This member is not the master ({}), {NOT_MASTER}",
                        standing.master.as_deref().unwrap_or("unknown")
                    )
                } else if request.workspace.is_none() {
                    "Requested outside a git working tree, so it runs on the calling member."
                        .to_owned()
                } else {
                    return Ok(Route::Auto);
                };
                return Ok(Route::Here(Some(note)));
            }
            Placement::Member(member) => member,
        };
        if self.is_own_name(member, standing.name.as_deref()) {
            return Ok(Route::Here(Some(
                "The required member is the calling member.".into(),
            )));
        }
        if standing.members.is_empty() {
            return Err(Failure::new(
                "member_unavailable",
                format!("Member {member} is not reachable: this member is not in a fleet yet."),
                format!("Drop --member, or require this member ({}).", self.name()),
            ));
        }
        if !standing.members.iter().any(|m| m == member) {
            return Err(Failure::new(
                "member_unavailable",
                format!("{member} is not a member of this fleet."),
                "List the members with elsecall fleet.",
            ));
        }
        if standing.master != standing.name {
            return Ok(Route::Here(Some(format!(
                "This member is not the master ({}), {NOT_MASTER}",
                standing.master.as_deref().unwrap_or("unknown")
            ))));
        }
        Ok(Route::There(member.clone()))
    }
}
