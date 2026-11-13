//! This member's place in the fleet: its key, the roster it trusts, and the answers
//! it gives other members.
//!
//! Operator changes live in `fleet_changes` and the fleet-wide view in `fleet_view`;
//! both act through this module's state.
use crate::{
    failure::{Failure, Result},
    inspection::Inspection,
    member_code,
    member_key::{MemberKey, PublicKey},
    membership_store::{Membership, MembershipStore},
    peer_auth::{self, ReplayGuard, RequestProof},
    roster::{Member, MemberName, Offer, Roster},
};
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

pub type MachineState = serde_json::Map<String, serde_json::Value>;

/// What a member reports about itself to another member.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerStatus {
    /// Recent outgoing peer failures, including attempts recovered by retry.
    #[serde(default)]
    pub peer_io: crate::peer_diagnostics::History,
    pub member: MemberName,
    pub version: String,
    /// The member's build ID; absent from members that predate upgrades.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<crate::build_identity::BuildId>,
    /// The roster this member records, so others can tell whether they agree.
    pub roster: Roster,
    /// The member's own machine inspection, passed through as reported so newer
    /// members can add fields.
    pub machine: Option<MachineState>,
    pub machine_error: Option<Failure>,
    /// The copies of other members' workspaces this member holds; absent from
    /// members that predate housekeeping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copies: Option<CopiesUsage>,
    /// Why the copies could not be reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copies_error: Option<Failure>,
    /// The member's load over the last hour and day; absent from members that
    /// predate the load history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_history: Option<crate::load_log::buckets::LoadSummary>,
    /// Why the load history could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_history_error: Option<Failure>,
    /// The member's configuration sync state; absent from members that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_sync: Option<crate::config_sync::report::SyncSummary>,
    /// Set while outside pressure holds the member's admission gate closed;
    /// absent otherwise, from members that predate it, and when this build
    /// cannot read the member's report.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::outside_pressure::blocked::lenient"
    )]
    pub blocked: Option<crate::outside_pressure::Blocked>,
    /// Early warnings about outside processes that have spun or grown for
    /// hours, the longest-standing first. Absent while there are none, from
    /// members that predate them; entries this build cannot read are dropped.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "crate::outside_pressure::blocked::lenient_list"
    )]
    pub outside_warnings: Vec<crate::outside_pressure::Warning>,
}

/// Where the fleet learns this member's configuration sync state.
pub trait SyncReport: Send + Sync {
    fn summary(&self) -> crate::config_sync::report::SyncSummary;
}

/// What must happen before the master role moves, such as configuration sync's
/// final push. Its report is shown with the move; an error refuses the move.
pub trait RoleHandover: Send + Sync {
    fn prepare<'a>(
        &'a self,
        roster: &'a Roster,
        to: &'a MemberName,
        force: bool,
    ) -> futures_util::future::BoxFuture<'a, Result<serde_json::Value>>;
}

/// Told when this member gains (`true`) or loses (`false`) the master role, as the
/// roster it records changes. Called with the membership lock held: it must only
/// start work, never wait on the fleet.
pub trait RoleObserver: Send + Sync {
    fn role_changed(&self, master: bool);
}

/// Proof that a request came from the recorded master, for configuration sync.
/// Only `Fleet::authorize_sync` creates it.
#[derive(Debug)]
pub struct SyncAuthority {
    master: MemberName,
}
impl SyncAuthority {
    pub fn master(&self) -> &MemberName {
        &self.master
    }
}

/// Proof that a request came from the recorded master, for installing a build it
/// distributes: code execution on this member. Only `Fleet::authorize_upgrade`
/// creates it.
#[derive(Debug)]
pub struct UpgradeAuthority {
    master: MemberName,
}
impl UpgradeAuthority {
    pub fn master(&self) -> &MemberName {
        &self.master
    }
}

/// What a member's copies of other members' workspaces take on its disk, as
/// last measured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopiesUsage {
    pub copies: usize,
    pub copies_bytes: u64,
    /// Copies made since the last pass, not measured yet and not in the bytes.
    #[serde(default)]
    pub unmeasured_copies: usize,
    pub stores: usize,
    pub stores_bytes: u64,
    /// The workspace root's volume, when it could be read.
    #[serde(default)]
    pub disk: Option<CopiesDisk>,
    /// The latest pass since the service started.
    #[serde(default)]
    pub last_pass_at: Option<crate::scheduling::timestamp::Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopiesDisk {
    pub available_bytes: u64,
    /// Free space below this is disk pressure.
    pub threshold_bytes: u64,
    pub pressure: bool,
}

/// Where the fleet learns what this member's copies take.
pub trait CopiesReport: Send + Sync {
    fn usage(&self) -> Result<CopiesUsage>;
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OfferOutcome {
    Accepted,
    AlreadyCurrent,
}

#[derive(Debug, Serialize)]
pub struct Identity {
    pub name: MemberName,
    pub key: PublicKey,
    pub endpoints: Vec<SocketAddr>,
    pub enrolled: bool,
    /// Pass this to elsecall fleet enroll or join on another member.
    pub code: String,
}

/// Proof that a request came from the member this member records as master. Only
/// `Fleet::authorize_placement` creates it; placement handlers require it.
#[derive(Debug)]
pub struct PlacementAuthority {
    master: MemberName,
}
impl PlacementAuthority {
    pub fn master(&self) -> &MemberName {
        &self.master
    }
}

/// What a member answers to a placement-authority probe.
#[derive(Debug, Serialize, Deserialize)]
pub struct PlacementAccepted {
    pub member: MemberName,
    pub accepted_from: MemberName,
}

/// A request that passed authentication: the enrolled member that signed it.
#[derive(Debug, Clone)]
pub struct AuthenticatedPeer {
    pub key: PublicKey,
    pub nonce: peer_auth::Nonce,
}

pub struct Fleet {
    key: MemberKey,
    store: MembershipStore,
    membership: Mutex<Membership>,
    /// This member's tailnet name and the endpoints its listener has bound.
    presence: Mutex<Option<(MemberName, Vec<SocketAddr>)>>,
    /// Serialises operator changes on this member; incoming offers do not take it.
    changes: tokio::sync::Mutex<()>,
    replay: ReplayGuard,
    inspection: Arc<Inspection>,
    copies: std::sync::OnceLock<Arc<dyn CopiesReport>>,
    load: std::sync::OnceLock<Arc<dyn crate::load_log::LoadReport>>,
    blocked: std::sync::OnceLock<Arc<dyn crate::outside_pressure::BlockedReport>>,
    warnings: std::sync::OnceLock<Arc<dyn crate::outside_pressure::WarningReport>>,
    sync: std::sync::OnceLock<Arc<dyn SyncReport>>,
    handover: std::sync::OnceLock<Arc<dyn RoleHandover>>,
    role_observer: std::sync::OnceLock<Arc<dyn RoleObserver>>,
}

impl Fleet {
    pub fn open(
        key: MemberKey,
        store: MembershipStore,
        inspection: Arc<Inspection>,
    ) -> Result<Self> {
        let membership = store.load()?;
        if let Membership::Enrolled(roster) = &membership
            && roster.member_by_key(&key.public()).is_none()
        {
            return Err(Failure::new(
                "invalid_fleet_state",
                "The fleet membership record does not list this member's key, so the key changed after enrolment.",
                "Move fleet.json out of the Elsecall state directory, run elsecall service restart, then enrol this member again.",
            ));
        }
        Ok(Self {
            key,
            store,
            membership: Mutex::new(membership),
            presence: Mutex::new(None),
            changes: tokio::sync::Mutex::new(()),
            replay: ReplayGuard::default(),
            inspection,
            copies: std::sync::OnceLock::new(),
            load: std::sync::OnceLock::new(),
            blocked: std::sync::OnceLock::new(),
            warnings: std::sync::OnceLock::new(),
            sync: std::sync::OnceLock::new(),
            handover: std::sync::OnceLock::new(),
            role_observer: std::sync::OnceLock::new(),
        })
    }

    /// Reports this member's copies with its status from now on.
    pub fn report_copies(&self, report: Arc<dyn CopiesReport>) {
        let _ = self.copies.set(report);
    }

    /// Reports this member's load history with its status from now on.
    pub fn report_load(&self, report: Arc<dyn crate::load_log::LoadReport>) {
        let _ = self.load.set(report);
    }

    /// Reports whether outside pressure blocks this member with its status from
    /// now on.
    pub fn report_blocked(&self, report: Arc<dyn crate::outside_pressure::BlockedReport>) {
        let _ = self.blocked.set(report);
    }

    /// Reports this member's early warnings about outside processes with its
    /// status from now on.
    pub fn report_warnings(&self, report: Arc<dyn crate::outside_pressure::WarningReport>) {
        let _ = self.warnings.set(report);
    }

    /// Reports this member's configuration sync state with its status from now on.
    pub fn report_sync(&self, report: Arc<dyn SyncReport>) {
        let _ = self.sync.set(report);
    }

    /// Runs `handover` before every move of the master role from now on.
    pub fn hand_over_with(&self, handover: Arc<dyn RoleHandover>) {
        let _ = self.handover.set(handover);
    }

    pub(crate) fn handover(&self) -> Option<Arc<dyn RoleHandover>> {
        self.handover.get().cloned()
    }

    /// The member key, for components of the service that talk to other members.
    pub fn member_key(&self) -> &MemberKey {
        &self.key
    }

    /// The name the roster gives this member.
    pub fn name_in(&self, roster: &Roster) -> MemberName {
        self.own_name(roster)
    }

    /// This member's load history, once the service reports it.
    pub fn load_report(&self) -> Option<Arc<dyn crate::load_log::LoadReport>> {
        self.load.get().cloned()
    }

    pub(crate) fn key(&self) -> &MemberKey {
        &self.key
    }

    pub fn public_key(&self) -> PublicKey {
        self.key.public()
    }

    /// Records the tailnet name and the endpoints the listener has bound.
    pub fn set_presence(&self, name: MemberName, endpoints: Vec<SocketAddr>) {
        *self.presence.lock().expect("presence lock") = Some((name, endpoints));
    }

    pub(crate) fn presence(&self) -> Option<(MemberName, Vec<SocketAddr>)> {
        self.presence.lock().expect("presence lock").clone()
    }

    pub(crate) fn membership(&self) -> Membership {
        self.membership.lock().expect("membership lock").clone()
    }

    /// Takes the lock that serialises operator changes on this member.
    pub(crate) async fn lock_changes(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.changes.lock().await
    }

    /// This member as it presents itself to others, from its live presence.
    pub(crate) fn own_member(&self) -> Result<Member> {
        let (name, endpoints) = self
            .presence()
            .filter(|(_, endpoints)| !endpoints.is_empty())
            .ok_or_else(|| crate::tailnet::unavailable("no tailnet address is bound yet"))?;
        Member::new(name, self.key.public(), endpoints)
    }

    /// The roster this member acts on. A solo member is a fleet of one; without a
    /// tailnet presence it has no roster and trusts nobody.
    pub fn roster(&self) -> Result<Roster> {
        roster_of(&self.membership(), self)
    }

    /// Replaces the membership, provided it is still `base`.
    pub(crate) fn replace(&self, base: &Membership, next: Membership) -> Result<()> {
        let mut membership = self.membership.lock().expect("membership lock");
        if &*membership != base {
            return Err(changed_meanwhile());
        }
        let was_master = self.is_master_of(&membership);
        self.store.save(&next)?;
        *membership = next;
        self.notice_role(was_master, &membership);
        Ok(())
    }

    /// Records `next` locally, provided the roster this member acts on is still `base`.
    pub(crate) fn commit(&self, base: &Roster, next: &Roster) -> Result<()> {
        let mut membership = self.membership.lock().expect("membership lock");
        if &roster_of(&membership, self)? != base {
            return Err(changed_meanwhile());
        }
        let was_master = self.is_master_of(&membership);
        let next = Membership::Enrolled(next.clone());
        self.store.save(&next)?;
        *membership = next;
        self.notice_role(was_master, &membership);
        Ok(())
    }

    fn is_master_of(&self, membership: &Membership) -> bool {
        roster_of(membership, self).is_ok_and(|r| r.master().key == self.key.public())
    }

    /// Tells the role observer when this member gains or loses the master role.
    fn notice_role(&self, was_master: bool, now: &Membership) {
        let is_master = self.is_master_of(now);
        if was_master != is_master
            && let Some(observer) = self.role_observer.get()
        {
            observer.role_changed(is_master);
        }
    }

    /// Calls `observer` whenever this member gains or loses the master role.
    pub fn observe_role_with(&self, observer: Arc<dyn RoleObserver>) {
        let _ = self.role_observer.set(observer);
    }

    // Member-facing operations.

    /// Whether `sender` is enrolled; checked before a request body is read.
    pub fn is_enrolled(&self, sender: &PublicKey) -> bool {
        self.roster()
            .is_ok_and(|roster| roster.member_by_key(sender).is_some())
    }

    /// Authenticates a signed request. `None` means the request gets nothing.
    pub fn authenticate(
        &self,
        proof: &RequestProof,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Option<AuthenticatedPeer> {
        if !self.is_enrolled(&proof.sender) {
            return None;
        }
        let own = self.key.public();
        let request = peer_auth::Request {
            recipient: &own,
            method,
            path,
            body,
        };
        (request.verify(proof) && self.replay.admit(proof, peer_auth::now())).then_some(
            AuthenticatedPeer {
                key: proof.sender,
                nonce: proof.nonce,
            },
        )
    }

    pub fn sign_response(&self, peer: &AuthenticatedPeer, status: u16, body: &[u8]) -> String {
        peer_auth::Response {
            responder: &self.key.public(),
            requester: &peer.key,
            nonce: &peer.nonce,
            status,
            body,
        }
        .sign(&self.key)
    }

    /// Accepts a run channel `peer` asked for in its attach request, returning the
    /// signed header value. The key stays in the service.
    pub fn sign_channel(
        &self,
        peer: &AuthenticatedPeer,
        run: &str,
        requester_ephemeral: &[u8; 32],
        responder_ephemeral: &[u8; 32],
    ) -> (String, Vec<u8>) {
        let own = self.key.public();
        let acceptance = peer_auth::ChannelAcceptance {
            responder: &own,
            requester: &peer.key,
            nonce: &peer.nonce,
            run,
            requester_ephemeral,
            responder_ephemeral,
        };
        (acceptance.sign(&self.key), acceptance.context())
    }

    pub async fn status(&self) -> Result<PeerStatus> {
        let roster = self.roster()?;
        let inspection = self.inspection.clone();
        let copies = self
            .copies
            .get()
            .cloned()
            .map(|report| tokio::task::spawn_blocking(move || report.usage()));
        let load = self.load_report().map(|log| {
            tokio::task::spawn_blocking(move || {
                log.summary(crate::scheduling::timestamp::Timestamp::now())
            })
        });
        let sync = self
            .sync
            .get()
            .cloned()
            .map(|report| tokio::task::spawn_blocking(move || report.summary()));
        let machine = tokio::task::spawn_blocking(move || inspection.machine())
            .await
            .unwrap_or_else(|_| {
                Err(Failure::new(
                    "inspection_failed",
                    "Machine inspection failed.",
                    "Run elsecall service restart.",
                ))
            });
        let (machine, machine_error) = match machine {
            Ok(machine) => match serde_json::to_value(machine) {
                Ok(serde_json::Value::Object(map)) => (Some(map), None),
                _ => unreachable!("machine state serializes to an object"),
            },
            Err(error) => (None, Some(error)),
        };
        // Read alongside the machine inspection above.
        let (copies, copies_error) = match copies {
            None => (None, None),
            Some(task) => match task.await {
                Ok(Ok(usage)) => (Some(usage), None),
                Ok(Err(error)) => (None, Some(error)),
                Err(_) => (
                    None,
                    Some(Failure::new(
                        "copies_unavailable",
                        "This member's copies could not be read.",
                        "Run elsecall copies on that member.",
                    )),
                ),
            },
        };
        let (load_history, load_history_error) = match load {
            None => (None, None),
            Some(task) => match task.await {
                Ok(Ok(summary)) => (Some(summary), None),
                Ok(Err(error)) => (None, Some(error)),
                Err(_) => (
                    None,
                    Some(Failure::new(
                        "load_history_unavailable",
                        "This member's load history could not be read.",
                        "Run elsecall doctor on that member.",
                    )),
                ),
            },
        };
        Ok(PeerStatus {
            peer_io: crate::peer_diagnostics::recent(),
            member: self.own_name(&roster),
            version: env!("CARGO_PKG_VERSION").into(),
            build: crate::build_identity::this_build().ok().map(|b| b.build),
            roster,
            machine,
            machine_error,
            copies,
            copies_error,
            load_history,
            load_history_error,
            config_sync: match sync {
                Some(task) => task.await.ok(),
                None => None,
            },
            blocked: self.blocked.get().and_then(|report| report.blocked()),
            outside_warnings: self
                .warnings
                .get()
                .map(|report| report.warnings())
                .unwrap_or_default(),
        })
    }

    /// The master as this member records it, if `sender` is that master's key: the
    /// only way to obtain sync authority.
    pub fn authorize_sync(&self, sender: &PublicKey) -> Result<SyncAuthority> {
        let roster = self.roster()?;
        let master = roster.master();
        if &master.key == sender {
            Ok(SyncAuthority {
                master: master.name.clone(),
            })
        } else {
            Err(Failure::new(
                "not_master",
                format!(
                    "This member accepts synced configuration only from its recorded master, {}.",
                    master.name
                ),
                format!("Run sync commands on {}.", master.name),
            ))
        }
    }

    /// The master as this member records it, if `sender` is that master's key: the
    /// only way to obtain upgrade authority.
    pub fn authorize_upgrade(&self, sender: &PublicKey) -> Result<UpgradeAuthority> {
        let roster = self.roster()?;
        let master = roster.master();
        if &master.key == sender {
            Ok(UpgradeAuthority {
                master: master.name.clone(),
            })
        } else {
            Err(Failure::new(
                "not_master",
                format!(
                    "This member installs builds only from its recorded master, {}.",
                    master.name
                ),
                format!("Run elsecall fleet upgrade on {}.", master.name),
            ))
        }
    }

    pub(crate) fn own_name(&self, roster: &Roster) -> MemberName {
        roster
            .member_by_key(&self.key.public())
            .expect("own key is enrolled")
            .name
            .clone()
    }

    /// The master as this member records it, if `sender` is that master's key. This is
    /// the only way to obtain placement authority.
    pub fn authorize_placement(&self, sender: &PublicKey) -> Result<PlacementAuthority> {
        let roster = self.roster()?;
        let master = roster.master();
        if &master.key == sender {
            Ok(PlacementAuthority {
                master: master.name.clone(),
            })
        } else {
            Err(Failure::new(
                "not_master",
                format!(
                    "This member accepts placement only from its recorded master, {}.",
                    master.name
                ),
                "Send placement requests from the master, or move the master role with elsecall fleet move-master.",
            ))
        }
    }

    /// Considers a roster offered by an enrolled member.
    pub fn offer(&self, sender: &PublicKey, offered: Roster) -> Result<OfferOutcome> {
        let own = self.key.public();
        let mut membership = self.membership.lock().expect("membership lock");
        let current = roster_of(&membership, self)?;
        match current.offer(&offered, &own, sender)? {
            Offer::Accept => {
                let was_master = self.is_master_of(&membership);
                let next = Membership::Enrolled(offered);
                self.store.save(&next)?;
                *membership = next;
                self.notice_role(was_master, &membership);
                Ok(OfferOutcome::Accepted)
            }
            Offer::AlreadyCurrent => Ok(OfferOutcome::AlreadyCurrent),
            Offer::Stale => Err(Failure::new(
                "stale_roster",
                format!(
                    "The offered roster (epoch {}) is older than this member's (epoch {}).",
                    offered.epoch(),
                    current.epoch()
                ),
                "Run elsecall fleet sync on an up-to-date member.",
            )),
            Offer::Conflict => Err(Failure::new(
                "roster_conflict",
                format!(
                    "This member records a different roster at epoch {}.",
                    current.epoch()
                ),
                "Members changed the fleet independently. Run elsecall fleet sync on the member whose roster should win.",
            )),
        }
    }

    pub fn identity(&self) -> Result<Identity> {
        let mut member = self.own_member()?;
        let enrolled = match self.membership() {
            Membership::Enrolled(roster) => {
                // The roster's name for this member is authoritative once enrolled.
                member.name = self.own_name(&roster);
                true
            }
            Membership::Solo => false,
        };
        Ok(Identity {
            code: member_code::encode(&member),
            name: member.name,
            key: member.key,
            endpoints: member.endpoints,
            enrolled,
        })
    }
}

fn roster_of(membership: &Membership, fleet: &Fleet) -> Result<Roster> {
    match membership {
        Membership::Enrolled(roster) => Ok(roster.clone()),
        Membership::Solo => fleet.own_member().map(Roster::solo),
    }
}

pub(crate) fn changed_meanwhile() -> Failure {
    Failure::new(
        "roster_changed",
        "The fleet changed while this operation ran; nothing was recorded.",
        "Run elsecall fleet, then retry.",
    )
}

#[cfg(test)]
mod copies_tests {
    use super::*;

    #[test]
    fn copies_are_optional_in_both_directions() {
        // An older member's status has no copies.
        let older: serde_json::Value = serde_json::json!({
            "member": "alpha", "version": "0.1.0", "machine": null, "machine_error": null
        });
        assert!(
            serde_json::from_value::<CopiesFields>(older)
                .unwrap()
                .copies
                .is_none()
        );
        // An older reader ignores the new fields.
        #[derive(Deserialize)]
        struct OlderStatus {
            member: String,
        }
        let newer = serde_json::json!({
            "member": "alpha",
            "copies": {"copies": 1, "copies_bytes": 2, "stores": 1, "stores_bytes": 3,
                       "disk": {"available_bytes": 4, "threshold_bytes": 5, "pressure": true}},
            "copies_error": null
        });
        assert_eq!(
            serde_json::from_value::<OlderStatus>(newer.clone())
                .unwrap()
                .member,
            "alpha"
        );
        let read: CopiesFields = serde_json::from_value(newer).unwrap();
        let usage = read.copies.unwrap();
        assert_eq!(usage.unmeasured_copies, 0, "absent from earlier builds");
        assert!(usage.disk.unwrap().pressure);
        assert!(usage.last_pass_at.is_none());
    }

    /// The copies fields of `PeerStatus`, as a reader of them sees them.
    #[derive(Deserialize)]
    struct CopiesFields {
        #[serde(default)]
        copies: Option<CopiesUsage>,
        #[serde(default)]
        #[allow(dead_code)]
        copies_error: Option<Failure>,
    }
}
