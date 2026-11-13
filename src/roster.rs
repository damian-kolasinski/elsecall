//! The fleet roster: enrolled members, their keys and endpoints, and the master.
//!
//! A `Roster` can only be built through validation, so every roster in memory has
//! unique names and keys, at least one endpoint per member, and a master that is a
//! member. Changes produce a new roster with a higher epoch.
use crate::{
    failure::{Failure, Result},
    member_key::PublicKey,
    tailnet::is_member_address,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

pub(crate) const MAX_MEMBERS: usize = 64;
const MAX_ENDPOINTS: usize = 8;
/// Epochs stay far below overflow, so every roster leaves room for later changes.
const MAX_EPOCH: u64 = 1 << 53;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MemberName(String);
impl MemberName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for MemberName {
    type Error = Failure;
    fn try_from(name: String) -> Result<Self> {
        let bytes = name.as_bytes();
        let valid = (1..=63).contains(&bytes.len())
            && bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
            && bytes[0] != b'-'
            && bytes[bytes.len() - 1] != b'-';
        if valid {
            Ok(Self(name))
        } else {
            Err(Failure::new(
                "invalid_member_name",
                "Member names are 1-63 lowercase letters, digits or inner hyphens.",
                "Use the machine's tailnet name, as elsecall fleet identity reports it.",
            ))
        }
    }
}
impl std::str::FromStr for MemberName {
    type Err = Failure;
    fn from_str(name: &str) -> Result<Self> {
        Self::try_from(name.to_owned())
    }
}
impl From<MemberName> for String {
    fn from(name: MemberName) -> Self {
        name.0
    }
}
impl std::fmt::Display for MemberName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawMember")]
pub struct Member {
    pub name: MemberName,
    pub key: PublicKey,
    /// Where the member listens, in preference order.
    pub endpoints: Vec<SocketAddr>,
}
#[derive(Deserialize)]
struct RawMember {
    name: MemberName,
    key: PublicKey,
    endpoints: Vec<SocketAddr>,
}
impl TryFrom<RawMember> for Member {
    type Error = Failure;
    fn try_from(raw: RawMember) -> Result<Self> {
        Member::new(raw.name, raw.key, raw.endpoints)
    }
}
impl Member {
    pub fn new(name: MemberName, key: PublicKey, endpoints: Vec<SocketAddr>) -> Result<Self> {
        if endpoints.is_empty()
            || endpoints.len() > MAX_ENDPOINTS
            || !endpoints
                .iter()
                .all(|e| e.port() != 0 && is_member_address(e.ip()))
        {
            return Err(Failure::new(
                "invalid_member_endpoints",
                "A member needs one to eight tailnet endpoints.",
                "Check that Tailscale is running on that machine, then copy its member code again.",
            ));
        }
        Ok(Self {
            name,
            key,
            endpoints,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawRoster")]
pub struct Roster {
    epoch: u64,
    master: MemberName,
    members: Vec<Member>,
}
#[derive(Deserialize)]
struct RawRoster {
    epoch: u64,
    master: MemberName,
    members: Vec<Member>,
}
impl TryFrom<RawRoster> for Roster {
    type Error = Failure;
    fn try_from(raw: RawRoster) -> Result<Self> {
        Roster::new(raw.epoch, raw.master, raw.members)
    }
}

/// What a roster change did.
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    /// A new roster with a higher epoch.
    Changed(Roster),
    /// The roster already says this; nothing to record, but members may need it again.
    Unchanged,
}

/// How a member treats a roster offered by another member.
#[derive(Debug, PartialEq, Eq)]
pub enum Offer {
    Accept,
    AlreadyCurrent,
    Stale,
    Conflict,
}

impl Roster {
    pub fn new(epoch: u64, master: MemberName, members: Vec<Member>) -> Result<Self> {
        let invalid = |message: &str| {
            Failure::new(
                "invalid_roster",
                message.to_owned(),
                "Fleet state was rejected; inspect the members with elsecall fleet.",
            )
        };
        if members.is_empty() || members.len() > MAX_MEMBERS {
            return Err(invalid("A fleet has one to 64 members."));
        }
        if epoch >= MAX_EPOCH {
            return Err(invalid("The roster epoch is out of range."));
        }
        for (i, member) in members.iter().enumerate() {
            if members[..i]
                .iter()
                .any(|other| other.name == member.name || other.key == member.key)
            {
                return Err(invalid("Member names and keys must be unique."));
            }
        }
        if !members.iter().any(|m| m.name == master) {
            return Err(invalid("The master must be an enrolled member."));
        }
        Ok(Self {
            epoch,
            master,
            members,
        })
    }
    /// The roster of a machine that has not joined a fleet: itself, as master.
    pub fn solo(member: Member) -> Self {
        Self {
            epoch: 0,
            master: member.name.clone(),
            members: vec![member],
        }
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn master(&self) -> &Member {
        self.member(&self.master).expect("master is a member")
    }
    pub fn members(&self) -> &[Member] {
        &self.members
    }
    pub fn member(&self, name: &MemberName) -> Option<&Member> {
        self.members.iter().find(|m| &m.name == name)
    }
    pub fn member_by_key(&self, key: &PublicKey) -> Option<&Member> {
        self.members.iter().find(|m| &m.key == key)
    }

    pub fn enrol(&self, member: Member) -> Result<Change> {
        match (self.member(&member.name), self.member_by_key(&member.key)) {
            (Some(by_name), Some(by_key))
                if by_name == by_key && by_name.endpoints == member.endpoints =>
            {
                Ok(Change::Unchanged)
            }
            (None, None) => {
                let mut members = self.members.clone();
                members.push(member);
                Roster::new(self.next_epoch(), self.master.clone(), members).map(Change::Changed)
            }
            (Some(by_name), Some(by_key)) if by_name == by_key => {
                // Same member with new endpoints, e.g. after its tailnet addresses changed.
                let members = self
                    .members
                    .iter()
                    .map(|m| {
                        if m.key == member.key {
                            member.clone()
                        } else {
                            m.clone()
                        }
                    })
                    .collect();
                Roster::new(self.next_epoch(), self.master.clone(), members).map(Change::Changed)
            }
            _ => Err(Failure::new(
                "member_conflict",
                format!(
                    "Another member is already enrolled under the name {} or with this key.",
                    member.name
                ),
                "Remove the old member with elsecall fleet remove, then enrol again.",
            )),
        }
    }

    fn next_epoch(&self) -> u64 {
        // Bounded by MAX_EPOCH, so this cannot overflow; Roster::new rejects the result
        // if it reaches the bound.
        self.epoch + 1
    }

    /// The same roster with an epoch above `highest`, so it replaces every roster seen.
    pub fn reissued_after(&self, highest: u64) -> Result<Roster> {
        Roster::new(
            highest.max(self.epoch).saturating_add(1),
            self.master.clone(),
            self.members.clone(),
        )
    }

    /// Removes a member. `by` is the member making the change, which cannot remove
    /// itself (it uses `leave`) and cannot remove the master.
    pub fn remove(&self, name: &MemberName, by: &PublicKey) -> Result<Roster> {
        let member = self.member(name).ok_or_else(|| not_a_member(name))?;
        if &member.key == by {
            return Err(Failure::new(
                "cannot_remove_self",
                "A member cannot remove itself.",
                "Run elsecall fleet remove on another member, then elsecall fleet leave here.",
            ));
        }
        if name == &self.master {
            return Err(Failure::new(
                "member_is_master",
                format!("{name} is the master and cannot be removed."),
                "Move the master role to another member first.",
            ));
        }
        let members = self
            .members
            .iter()
            .filter(|m| &m.name != name)
            .cloned()
            .collect();
        Roster::new(self.next_epoch(), self.master.clone(), members)
    }

    /// Makes `name` the master.
    pub fn with_master(&self, name: &MemberName) -> Result<Change> {
        self.member(name).ok_or_else(|| not_a_member(name))?;
        if name == &self.master {
            return Ok(Change::Unchanged);
        }
        Roster::new(self.epoch + 1, name.clone(), self.members.clone()).map(Change::Changed)
    }

    /// Decides whether `offered`, sent by `sender`, should replace this roster on the
    /// member identified by `own`.
    pub fn offer(&self, offered: &Roster, own: &PublicKey, sender: &PublicKey) -> Result<Offer> {
        if offered.member_by_key(own).is_none() || offered.member_by_key(sender).is_none() {
            return Err(Failure::new(
                "invalid_roster",
                "An offered roster must list both the sender and the receiving member.",
                "Run the change again from a member that is still enrolled.",
            ));
        }
        Ok(if offered.epoch > self.epoch {
            Offer::Accept
        } else if offered == self {
            Offer::AlreadyCurrent
        } else if offered.epoch == self.epoch {
            Offer::Conflict
        } else {
            Offer::Stale
        })
    }
}

pub fn not_a_member(name: &MemberName) -> Failure {
    Failure::new(
        "not_a_member",
        format!("{name} is not an enrolled member."),
        "Run elsecall fleet to list members.",
    )
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::member_key::MemberKey;

    pub fn member(name: &str, port: u16) -> Member {
        crate::tailnet::allow_loopback_members();
        Member::new(
            name.parse().unwrap(),
            MemberKey::generate().unwrap().public(),
            vec![SocketAddr::from(([127, 0, 0, 1], port))],
        )
        .unwrap()
    }
    fn changed(change: Change) -> Roster {
        match change {
            Change::Changed(roster) => roster,
            Change::Unchanged => panic!("expected a change"),
        }
    }

    #[test]
    fn names_are_dns_labels() {
        for ok in ["a", "mac-mini", "macbook-pro"] {
            assert!(ok.parse::<MemberName>().is_ok());
        }
        for no in ["", "-a", "a-", "Mac", "a_b", "a.b", &"a".repeat(64)] {
            assert!(no.parse::<MemberName>().is_err(), "{no}");
        }
    }

    #[test]
    fn rosters_reject_duplicates_missing_master_and_bad_endpoints() {
        let a = member("a", 1);
        let mut same_key = member("b", 2);
        same_key.key = a.key;
        assert!(Roster::new(1, a.name.clone(), vec![a.clone(), same_key]).is_err());
        assert!(Roster::new(1, a.name.clone(), vec![a.clone(), member("a", 3)]).is_err());
        assert!(Roster::new(1, "b".parse().unwrap(), vec![a.clone()]).is_err());
        assert!(Member::new(a.name.clone(), a.key, vec![]).is_err());
        assert!(
            Member::new(
                a.name.clone(),
                a.key,
                vec!["192.168.1.2:1".parse().unwrap()]
            )
            .is_err()
        );
        let json = serde_json::to_string(&Roster::solo(a.clone())).unwrap();
        let tampered = json.replace("\"master\":\"a\"", "\"master\":\"zz\"");
        assert!(serde_json::from_str::<Roster>(&tampered).is_err());
        assert_eq!(
            serde_json::from_str::<Roster>(&json).unwrap(),
            Roster::solo(a)
        );
    }

    #[test]
    fn enrol_and_remove_bump_the_epoch_and_keep_invariants() {
        let a = member("a", 1);
        let b = member("b", 2);
        let solo = Roster::solo(a.clone());
        let two = changed(solo.enrol(b.clone()).unwrap());
        assert_eq!(two.epoch(), 1);
        assert_eq!(two.master().name, a.name);
        assert_eq!(two.enrol(b.clone()).unwrap(), Change::Unchanged);
        let mut impostor = member("b", 3);
        assert_eq!(
            two.enrol(impostor.clone()).unwrap_err().code,
            "member_conflict"
        );
        impostor.name = "c".parse().unwrap();
        impostor.key = b.key;
        assert_eq!(two.enrol(impostor).unwrap_err().code, "member_conflict");
        let mut moved = b.clone();
        moved.endpoints = vec!["127.0.0.1:9".parse().unwrap()];
        assert_eq!(changed(two.enrol(moved).unwrap()).epoch(), 2);

        assert_eq!(
            two.remove(&a.name, &b.key).unwrap_err().code,
            "member_is_master"
        );
        assert_eq!(
            two.remove(&b.name, &b.key).unwrap_err().code,
            "cannot_remove_self"
        );
        assert_eq!(
            two.remove(&"zz".parse().unwrap(), &a.key).unwrap_err().code,
            "not_a_member"
        );
        let back = two.remove(&b.name, &a.key).unwrap();
        assert_eq!((back.epoch(), back.members().len()), (2, 1));
    }

    #[test]
    fn moving_the_master_role_needs_a_member_and_bumps_the_epoch() {
        let a = member("a", 1);
        let b = member("b", 2);
        let two = changed(Roster::solo(a.clone()).enrol(b.clone()).unwrap());
        let moved = changed(two.with_master(&b.name).unwrap());
        assert_eq!((moved.epoch(), &moved.master().name), (2, &b.name));
        assert_eq!(moved.with_master(&b.name).unwrap(), Change::Unchanged);
        assert_eq!(
            two.with_master(&"zz".parse().unwrap()).unwrap_err().code,
            "not_a_member"
        );
        // The old master can now be removed; the new one cannot.
        assert!(moved.remove(&a.name, &b.key).is_ok());
        assert_eq!(
            moved.remove(&b.name, &a.key).unwrap_err().code,
            "member_is_master"
        );
    }

    #[test]
    fn offers_are_accepted_only_when_newer_and_naming_both_parties() {
        let a = member("a", 1);
        let b = member("b", 2);
        let c = member("c", 3);
        let one = changed(Roster::solo(a.clone()).enrol(b.clone()).unwrap());
        let two = changed(one.enrol(c.clone()).unwrap());
        assert_eq!(one.offer(&two, &b.key, &a.key).unwrap(), Offer::Accept);
        assert_eq!(
            two.offer(&two, &b.key, &a.key).unwrap(),
            Offer::AlreadyCurrent
        );
        assert_eq!(two.offer(&one, &b.key, &a.key).unwrap(), Offer::Stale);
        let other = changed(one.enrol(member("d", 4)).unwrap());
        assert_eq!(two.offer(&other, &b.key, &a.key).unwrap(), Offer::Conflict);
        assert!(one.offer(&two, &c.key, &member("x", 9).key).is_err());
        let without_b = two.remove(&b.name, &a.key).unwrap();
        assert!(two.offer(&without_b, &b.key, &a.key).is_err());
        // Re-issuing replaces rosters at any epoch seen, within bounds.
        let reissued = one.reissued_after(other.epoch()).unwrap();
        assert_eq!(
            other.offer(&reissued, &b.key, &a.key).unwrap(),
            Offer::Accept
        );
        assert!(one.reissued_after(MAX_EPOCH).is_err());
        assert!(one.reissued_after(u64::MAX).is_err());
    }
}
