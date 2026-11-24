//! The window's side of provisioning: members it asked the master to
//! provision, the member whose Setup dialog is open and what it read for it,
//! and the consent checkboxes of the attention list. The service does the
//! work; this only remembers what the window asked and showed.
//!
//! Every read runs on a thread of its own, never on the pool the fleet's
//! refresh shares, since a plan may take minutes: at most one plan and one
//! apply read per member at a time, a read asked for meanwhile running once
//! the current one lands.
use crate::{
    add_mac_source::Inbox,
    fleet_source::FleetSnapshot,
    provisioning_source::{Apply, Plan, ProvisioningSource, Setup},
};
use elsecall_contract::scheduling_model::timestamp::Timestamp;
use elsecall_peers::roster::Readiness;
use rune::Waker;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

/// How long an ask shows as starting at most: longer than the master takes
/// to plan, which reads every member.
const STARTING_FOR: f64 = 200.0;

/// An ask in flight: when, on the window's clock, and which apply the member
/// had then, so a new one, running or already finished, ends it.
struct Asked {
    at: f64,
    before: Option<Timestamp>,
    retrying_ready_member: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Plan,
    Apply,
}

enum Read {
    Plan(String, Result<Plan, String>),
    Apply(String, Result<Option<Apply>, String>),
}

pub(crate) struct ProvisioningState {
    source: Arc<dyn ProvisioningSource>,
    requested: HashMap<String, Asked>,
    /// The last ask that failed: its member and why.
    pub(crate) refused: Option<(String, String)>,
    answers: Arc<Inbox<(String, Result<(), String>)>>,
    /// The member whose Setup dialog is open.
    pub(crate) setup: Option<String>,
    /// What was read for it.
    pub(crate) details: Setup,
    reads: Arc<Inbox<Read>>,
    /// Reads running, and reads asked for while they ran.
    reading: HashSet<(Kind, String)>,
    again: HashSet<(Kind, String)>,
    /// Secret consent items whose "from now on" box is checked, by id.
    always: HashSet<String>,
}

impl ProvisioningState {
    pub(crate) fn new(source: Arc<dyn ProvisioningSource>) -> Self {
        Self {
            source,
            requested: HashMap::new(),
            refused: None,
            answers: Arc::new(Inbox::default()),
            setup: None,
            details: Setup::default(),
            reads: Arc::new(Inbox::default()),
            reading: HashSet::new(),
            again: HashSet::new(),
            always: HashSet::new(),
        }
    }

    pub(crate) fn set_waker(&self, waker: Waker) {
        self.answers.set_waker(waker.clone());
        self.reads.set_waker(waker);
    }

    /// Asks the master to provision `member` now, off the UI thread.
    pub(crate) fn provision(&mut self, member: &str, now: f64, fleet: Option<&FleetSnapshot>) {
        let row = fleet.and_then(|fleet| fleet.members.iter().find(|row| row.name == member));
        let previous = row.and_then(|row| row.provisioning.as_ref());
        self.requested.insert(
            member.to_owned(),
            Asked {
                at: now,
                before: previous.map(|apply| apply.started_at),
                retrying_ready_member: row.is_some_and(|row| row.readiness == Readiness::Ready)
                    && previous.is_some_and(|apply| !apply.failed.is_empty()),
            },
        );
        self.refused = None;
        let source = Arc::clone(&self.source);
        let answers = Arc::clone(&self.answers);
        let member = member.to_owned();
        std::thread::spawn(move || {
            let answer = source.provision(&member);
            answers.push((member, answer));
        });
    }

    pub(crate) fn requested(&self, member: &str) -> bool {
        self.requested.contains_key(member)
    }

    /// Why the last ask for `member` was refused, if it was.
    pub(crate) fn refusal(&self, member: &str) -> Option<&str> {
        self.refused
            .as_ref()
            .filter(|(name, _)| name == member)
            .map(|(_, why)| why.as_str())
    }

    /// Opens the Setup dialog for `member` and reads its setup.
    pub(crate) fn open_setup(&mut self, member: &str) {
        self.setup = Some(member.to_owned());
        self.details = Setup::default();
        self.read(Kind::Apply);
        self.read(Kind::Plan);
    }

    /// Reads the open dialog's plan and apply again.
    pub(crate) fn reload(&mut self) {
        self.details = Setup::default();
        self.read(Kind::Apply);
        self.read(Kind::Plan);
    }

    /// Reads the open dialog's apply again, as it moved.
    pub(crate) fn reload_apply(&mut self) {
        self.read(Kind::Apply);
    }

    fn read(&mut self, kind: Kind) {
        let Some(member) = self.setup.clone() else {
            return;
        };
        if !self.reading.insert((kind, member.clone())) {
            self.again.insert((kind, member));
            return;
        }
        let source = Arc::clone(&self.source);
        let reads = Arc::clone(&self.reads);
        std::thread::spawn(move || {
            let read = match kind {
                Kind::Plan => Read::Plan(member.clone(), source.plan(&member)),
                Kind::Apply => Read::Apply(member.clone(), source.apply(&member)),
            };
            reads.push(read);
        });
    }

    pub(crate) fn close_setup(&mut self) {
        self.setup = None;
        self.details = Setup::default();
    }

    pub(crate) fn always(&self, item: &str) -> bool {
        self.always.contains(item)
    }

    pub(crate) fn toggle_always(&mut self, item: &str) {
        if !self.always.remove(item) {
            self.always.insert(item.to_owned());
        }
    }

    /// Takes in answers and reads that landed, and lets go of asks the fleet
    /// shows started, finished or no longer needed. Whether anything changed.
    pub(crate) fn poll(&mut self, now: f64, fleet: Option<&FleetSnapshot>) -> bool {
        let mut changed = false;
        for (member, answer) in self.answers.drain() {
            if let Err(message) = answer {
                self.requested.remove(&member);
                self.refused = Some((member, message));
            }
            changed = true;
        }
        for read in self.reads.drain() {
            let (kind, member) = match &read {
                Read::Plan(member, _) => (Kind::Plan, member.clone()),
                Read::Apply(member, _) => (Kind::Apply, member.clone()),
            };
            self.reading.remove(&(kind, member.clone()));
            if self.setup.as_deref() == Some(member.as_str()) {
                match read {
                    Read::Plan(_, plan) => self.details.plan = Some(plan),
                    Read::Apply(_, apply) => self.details.apply = Some(apply),
                }
                changed = true;
            }
            if self.again.remove(&(kind, member.clone())) && self.setup.as_ref() == Some(&member) {
                self.read(kind);
            }
        }
        let before = self.requested.len();
        self.requested.retain(|member, asked| {
            let row = fleet.and_then(|fleet| fleet.members.iter().find(|row| &row.name == member));
            let moved = row
                .and_then(|row| row.provisioning.as_ref())
                .is_some_and(|summary| Some(summary.started_at) != asked.before);
            let ready = !asked.retrying_ready_member
                && row.is_some_and(|row| row.readiness == Readiness::Ready);
            !moved && !ready && now - asked.at < STARTING_FOR
        });
        changed | (self.requested.len() != before)
    }

    /// For scenes: what the dialog shows, with nothing read in the background.
    pub(crate) fn show(&mut self, member: &str, setup: Setup) {
        self.setup = Some(member.to_owned());
        self.details = setup;
    }

    /// For scenes: an ask in flight.
    pub(crate) fn asked(&mut self, member: &str) {
        self.requested.insert(
            member.to_owned(),
            Asked {
                at: 0.0,
                before: None,
                retrying_ready_member: false,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        fleet_source::FleetSnapshot,
        provisioning_source::{Apply, Plan, ProvisioningSource},
        provisioning_state::*,
    };
    use elsecall_fleet::provisioning_model::{ApplyState, ProvisioningSummary};

    struct Quiet;
    impl ProvisioningSource for Quiet {
        fn provision(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn plan(&self, _: &str) -> Result<Plan, String> {
            Err("not read".into())
        }
        fn apply(&self, _: &str) -> Result<Option<Apply>, String> {
            Ok(None)
        }
    }

    fn fleet(readiness: Readiness, started: Option<&str>) -> FleetSnapshot {
        let mut fleet = crate::app::scene_fleets::members_snapshot();
        let row = fleet
            .members
            .iter_mut()
            .find(|row| row.name == "macbook-pro")
            .unwrap();
        row.readiness = readiness;
        row.provisioning = started.map(|at| ProvisioningSummary {
            state: ApplyState::Finished,
            started_at: Timestamp::parse_rfc3339(at).unwrap(),
            finished_at: None,
            steps: 0,
            installed: 0,
            failed: Vec::new(),
        });
        fleet
    }

    #[test]
    fn starting_ends_with_a_new_apply_ready_or_time() {
        let mut state = ProvisioningState::new(Arc::new(Quiet));
        let before = fleet(
            Readiness::AwaitingProvisioning,
            Some("2026-11-20T09:00:00Z"),
        );
        state.provision("macbook-pro", 0.0, Some(&before));
        state.poll(1.0, Some(&before));
        assert!(state.requested("macbook-pro"), "the old apply is no news");
        // A new apply that finished between two reads ends it too.
        let after = fleet(
            Readiness::AwaitingProvisioning,
            Some("2026-11-20T10:00:00Z"),
        );
        state.poll(2.0, Some(&after));
        assert!(!state.requested("macbook-pro"));

        state.provision("macbook-pro", 3.0, Some(&after));
        state.poll(
            4.0,
            Some(&fleet(Readiness::Ready, Some("2026-11-20T10:00:00Z"))),
        );
        assert!(
            !state.requested("macbook-pro"),
            "marked ready with nothing to apply"
        );

        state.provision("macbook-pro", 5.0, Some(&after));
        state.poll(5.0 + STARTING_FOR, Some(&after));
        assert!(
            !state.requested("macbook-pro"),
            "the master decided nothing in time"
        );
    }
}
