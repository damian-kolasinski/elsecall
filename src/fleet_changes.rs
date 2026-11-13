//! Operator changes to the fleet: enrolling, joining, removing, leaving, moving the
//! master role and syncing.
//!
//! A change is computed from this member's roster, refused while another member is
//! unreachable unless forced, recorded locally, then delivered to every other member.
use crate::{
    failure::{Failure, Result},
    fleet::{Fleet, OfferOutcome, PeerStatus, PlacementAccepted},
    member_code,
    membership_store::Membership,
    peer_client::{self, PEER_TIMEOUT},
    roster::{Change, Member, MemberName, Offer, Roster, not_a_member},
};
use hyper::Method;
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum DeliveryOutcome {
    Updated,
    AlreadyCurrent,
    /// The member being enrolled has not run `fleet join` yet; it will pull the
    /// roster when it does.
    AwaitingJoin,
    Failed {
        error: Failure,
    },
}

#[derive(Debug, Serialize)]
pub struct Delivery {
    pub member: MemberName,
    #[serde(flatten)]
    pub outcome: DeliveryOutcome,
}

/// The result of an operator change: the roster this member now records, and which
/// other members received it.
#[derive(Debug, Serialize)]
pub struct ChangeReport {
    pub epoch: u64,
    pub master: MemberName,
    pub members: Vec<MemberName>,
    pub deliveries: Vec<Delivery>,
    /// Every other member records this roster.
    pub complete: bool,
    pub next_step: String,
    /// What configuration sync did before the master role moved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_sync: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct LeaveReport {
    pub status: &'static str,
    pub next_step: String,
}

pub async fn enroll(fleet: &Fleet, code: &str, force: bool) -> Result<ChangeReport> {
    let candidate = member_code::decode(code)?;
    if candidate.key == fleet.public_key() {
        return Err(Failure::new(
            "cannot_enrol_self",
            "That code is this member's own code.",
            "Run elsecall fleet identity on the machine you want to enrol and use its code here.",
        ));
    }
    let _change = fleet.lock_changes().await;
    let roster = fleet.roster()?;
    let already_enrolled = roster.member_by_key(&candidate.key).is_some();
    let change = roster.enrol(candidate.clone())?;
    // The member being enrolled may not have joined yet, so it is not required to be
    // reachable; its join step establishes contact.
    let probed: Vec<Member> = others(fleet, &roster)
        .into_iter()
        .filter(|m| m.key != candidate.key)
        .collect();
    require_reachable(fleet, &probed, force).await?;
    let next = apply(fleet, &roster, change)?;
    // A newly enrolled member has not joined yet and would refuse the roster. One
    // enrolled before is offered it at the endpoints in its new code.
    let recipients: Vec<Member> = others(fleet, &next)
        .into_iter()
        .filter(|m| already_enrolled || m.key != candidate.key)
        .collect();
    let mut deliveries = deliver(fleet, &next, &recipients).await;
    for delivery in &mut deliveries {
        if delivery.member == candidate.name
            && matches!(&delivery.outcome, DeliveryOutcome::Failed { error } if error.code == "member_rejected")
        {
            delivery.outcome = DeliveryOutcome::AwaitingJoin;
        }
    }
    let own = fleet.identity()?;
    Ok(report(
        fleet,
        &next,
        deliveries,
        format!(
            "On {}, run: elsecall fleet join {}",
            candidate.name, own.code
        ),
    ))
}

pub async fn join(fleet: &Fleet, code: &str) -> Result<ChangeReport> {
    let sponsor = member_code::decode(code)?;
    let own = fleet.public_key();
    if sponsor.key == own {
        return Err(Failure::new(
            "cannot_join_self",
            "That code is this member's own code.",
            "Use the code that elsecall fleet enroll printed on the enrolling member.",
        ));
    }
    let _change = fleet.lock_changes().await;
    let current = fleet.membership();
    if let Membership::Enrolled(roster) = &current
        && roster.member_by_key(&sponsor.key).is_none()
    {
        return Err(Failure::new(
            "already_enrolled",
            "This member already belongs to a fleet that does not include that member.",
            "Run elsecall fleet leave first, after removing this member from its current fleet.",
        ));
    }
    let offered: Roster = peer_client::call(
        fleet.key(),
        &sponsor,
        Method::GET,
        "/peer/v1/roster",
        None::<&()>,
        PEER_TIMEOUT,
    )
    .await?;
    match offered.member_by_key(&sponsor.key) {
        Some(listed) if listed.name == sponsor.name => (),
        _ => {
            return Err(Failure::new(
                "invalid_roster",
                "The enrolling member's roster does not list it under the name in the code.",
                "Copy the join code again from elsecall fleet enroll.",
            ));
        }
    }
    let accepted = match &current {
        Membership::Solo if offered.member_by_key(&own).is_some() => Offer::Accept,
        Membership::Solo => return Err(not_enrolled_there(&sponsor.name)),
        Membership::Enrolled(roster) => roster
            .offer(&offered, &own, &sponsor.key)
            .map_err(|_| not_enrolled_there(&sponsor.name))?,
    };
    match accepted {
        Offer::Accept => fleet.replace(&current, Membership::Enrolled(offered.clone()))?,
        Offer::AlreadyCurrent => (),
        Offer::Stale | Offer::Conflict => {
            return Err(Failure::new(
                "roster_conflict",
                format!(
                    "{} records an older or different roster than this member.",
                    sponsor.name
                ),
                "Run elsecall fleet sync on the member whose roster should win.",
            ));
        }
    }
    Ok(report(
        fleet,
        &offered,
        Vec::new(),
        "Run elsecall fleet to see every member.".into(),
    ))
}

pub async fn remove(fleet: &Fleet, name: &MemberName, force: bool) -> Result<ChangeReport> {
    let _change = fleet.lock_changes().await;
    let roster = enrolled_roster(fleet, name)?;
    let next = roster.remove(name, &fleet.public_key())?;
    let recipients = others(fleet, &next);
    require_reachable(fleet, &recipients, force).await?;
    fleet.commit(&roster, &next)?;
    let deliveries = deliver(fleet, &next, &recipients).await;
    Ok(report(
        fleet,
        &next,
        deliveries,
        format!("On {name}, run: elsecall fleet leave"),
    ))
}

pub async fn leave(fleet: &Fleet, force: bool) -> Result<LeaveReport> {
    let _change = fleet.lock_changes().await;
    let current = fleet.membership();
    let Membership::Enrolled(roster) = &current else {
        return Ok(LeaveReport {
            status: "solo",
            next_step: "No action needed.".into(),
        });
    };
    if roster.master().key == fleet.public_key() && roster.members().len() > 1 && !force {
        return Err(Failure::new(
            "member_is_master",
            "This member is the master of a fleet with other members.",
            "Move the master role to another member first, or pass --force to leave anyway.",
        ));
    }
    let own = fleet.own_name(roster);
    fleet.replace(&current, Membership::Solo)?;
    Ok(LeaveReport {
        status: "solo",
        next_step: format!(
            "If other members still list {own}, run elsecall fleet remove {own} on one of them."
        ),
    })
}

/// Moves the master role to `name` on every member.
pub async fn move_master(fleet: &Fleet, name: &MemberName, force: bool) -> Result<ChangeReport> {
    let _change = fleet.lock_changes().await;
    let roster = enrolled_roster(fleet, name)?;
    let change = roster.with_master(name)?;
    let recipients = others(fleet, &roster);
    require_reachable(fleet, &recipients, force).await?;
    // The old master's state must reach every member before the role moves.
    let handed_over = match fleet.handover() {
        Some(handover) => Some(handover.prepare(&roster, name, force).await?),
        None => None,
    };
    // Repeating the move delivers the same roster to members that missed it.
    let next = apply(fleet, &roster, change)?;
    let deliveries = deliver(fleet, &next, &recipients).await;
    let mut report = report(
        fleet,
        &next,
        deliveries,
        format!("{name} is the master on every member."),
    );
    report.config_sync = handed_over;
    Ok(report)
}

/// Asks `name` whether it accepts placement authority from this member.
pub async fn probe_placement(fleet: &Fleet, name: &MemberName) -> Result<PlacementAccepted> {
    let roster = fleet.roster()?;
    let member = roster.member(name).ok_or_else(|| not_a_member(name))?;
    peer_client::call(
        fleet.key(),
        member,
        Method::POST,
        "/peer/v1/placement/probe",
        None::<&()>,
        PEER_TIMEOUT,
    )
    .await
}

/// Makes every member record this member's roster. Members that missed a change, or
/// that changed the fleet independently at the same epoch, are brought to it: this
/// roster is re-issued above every epoch seen. A member that records a newer roster
/// means this one is stale; sync then refuses unless told to override it.
pub async fn sync(fleet: &Fleet, force: bool, override_newer: bool) -> Result<ChangeReport> {
    let _change = fleet.lock_changes().await;
    let roster = match fleet.membership() {
        Membership::Enrolled(roster) => roster,
        Membership::Solo => {
            return Err(Failure::new(
                "not_enrolled",
                "This member has not joined a fleet.",
                "Enrol it with elsecall fleet enroll on a member, then join.",
            ));
        }
    };
    let recipients = others(fleet, &roster);
    let statuses = require_reachable(fleet, &recipients, force).await?;
    let differing: Vec<&Roster> = statuses
        .iter()
        .flatten()
        .map(|status| &status.roster)
        .filter(|seen| **seen != roster)
        .collect();
    let newer: Vec<String> = statuses
        .iter()
        .flatten()
        .filter(|status| status.roster.epoch() > roster.epoch())
        .map(|status| status.member.to_string())
        .collect();
    if !newer.is_empty() && !override_newer {
        return Err(Failure::new(
            "stale_roster",
            format!(
                "These members record a newer roster than this one: {}. Nothing was changed.",
                newer.join(", ")
            ),
            format!(
                "Run elsecall fleet sync on {} instead, or pass --override to replace their newer roster with this one.",
                newer[0]
            ),
        ));
    }
    let next = if differing.is_empty() {
        roster
    } else {
        let highest = differing
            .iter()
            .map(|r| r.epoch())
            .chain([roster.epoch()])
            .max()
            .expect("at least this member's epoch");
        let next = roster.reissued_after(highest)?;
        fleet.commit(&roster, &next)?;
        next
    };
    let deliveries = deliver(fleet, &next, &recipients).await;
    let own = fleet.own_name(&next);
    Ok(report(
        fleet,
        &next,
        deliveries,
        format!("Every member records the roster of {own}."),
    ))
}

// Shared steps.

pub(crate) fn enrolled_roster(fleet: &Fleet, name: &MemberName) -> Result<Roster> {
    match fleet.membership() {
        Membership::Enrolled(roster) => Ok(roster),
        Membership::Solo => Err(not_a_member(name)),
    }
}

pub(crate) fn others(fleet: &Fleet, roster: &Roster) -> Vec<Member> {
    roster
        .members()
        .iter()
        .filter(|m| m.key != fleet.public_key())
        .cloned()
        .collect()
}

/// Records a change locally; an unchanged roster is delivered again as it is.
pub(crate) fn apply(fleet: &Fleet, roster: &Roster, change: Change) -> Result<Roster> {
    match change {
        Change::Changed(next) => {
            fleet.commit(roster, &next)?;
            Ok(next)
        }
        Change::Unchanged => Ok(roster.clone()),
    }
}

/// Reads every member's status, refusing while any is unreachable unless forced.
/// Unreachable members are `None` in a forced change.
pub(crate) async fn require_reachable(
    fleet: &Fleet,
    members: &[Member],
    force: bool,
) -> Result<Vec<Option<PeerStatus>>> {
    let results = peer_client::call_each::<PeerStatus>(
        fleet.key(),
        members,
        Method::GET,
        "/peer/v1/status",
        None::<&()>,
    )
    .await;
    let unreachable: Vec<String> = members
        .iter()
        .zip(&results)
        .filter(|(_, result)| result.is_err())
        .map(|(member, _)| member.name.to_string())
        .collect();
    if !unreachable.is_empty() && !force {
        return Err(Failure::new(
            "member_unavailable",
            format!(
                "Not changed: these members are unreachable: {}.",
                unreachable.join(", ")
            ),
            "Bring them online and retry, or pass --force; unreachable members keep their old roster until elsecall fleet sync reaches them.",
        ));
    }
    Ok(results.into_iter().map(Result::ok).collect())
}

pub(crate) async fn deliver(fleet: &Fleet, roster: &Roster, to: &[Member]) -> Vec<Delivery> {
    let results = peer_client::call_each::<OfferOutcome>(
        fleet.key(),
        to,
        Method::PUT,
        "/peer/v1/roster",
        Some(roster),
    )
    .await;
    to.iter()
        .zip(results)
        .map(|(member, result)| Delivery {
            member: member.name.clone(),
            outcome: match result {
                Ok(OfferOutcome::Accepted) => DeliveryOutcome::Updated,
                Ok(OfferOutcome::AlreadyCurrent) => DeliveryOutcome::AlreadyCurrent,
                Err(error) => DeliveryOutcome::Failed { error },
            },
        })
        .collect()
}

pub(crate) fn report(
    fleet: &Fleet,
    roster: &Roster,
    deliveries: Vec<Delivery>,
    next_step: String,
) -> ChangeReport {
    let complete = deliveries
        .iter()
        .all(|d| !matches!(d.outcome, DeliveryOutcome::Failed { .. }));
    ChangeReport {
        epoch: roster.epoch(),
        master: roster.master().name.clone(),
        members: roster.members().iter().map(|m| m.name.clone()).collect(),
        deliveries,
        complete,
        next_step: if complete {
            next_step
        } else {
            format!(
                "Some members did not record the change. Once they are reachable, run elsecall fleet sync on {}.",
                fleet.own_name(roster)
            )
        },
        config_sync: None,
    }
}

fn not_enrolled_there(name: &MemberName) -> Failure {
    Failure::new(
        "not_enrolled",
        format!("{name} has not enrolled this member."),
        format!("Run elsecall fleet enroll with this member's code on {name} first."),
    )
}
