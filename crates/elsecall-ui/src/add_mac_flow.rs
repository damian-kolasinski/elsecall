//! The Add Mac sheet's state: which page it shows, what the operator typed, the
//! Macs found nearby and the add's progress. Pure; the view draws it and the
//! source feeds it.

use elsecall_contract::failure::Failure;
use elsecall_fleet::add_mac::{AddReport, Approval, Event, Step};
use elsecall_fleet::nearby_macs::{Membership, NearbyMac, RemoteLogin};
use elsecall_fleet::remote_mac::LoginLink;
use rune::TextEdit;
use zeroize::Zeroizing;

/// The steps of an add, in the order the CLI runs them.
pub(crate) const STEPS: [Step; 7] = [
    Step::Check,
    Step::Tailnet,
    Step::Install,
    Step::Firewall,
    Step::SshKey,
    Step::Enroll,
    Step::Verify,
];

/// The Mac being added, as the operator picked it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub host: String,
    pub name: String,
    pub model: Option<String>,
}

/// What `fleet add` needs; the password leaves the sheet's state with it.
pub(crate) struct AddRequest {
    /// Which add this is, so updates from an earlier one are told apart.
    pub run: u64,
    pub host: String,
    pub account: String,
    pub password: Zeroizing<String>,
}

/// The Macs on the local network, as the last look found them.
#[derive(Debug, Clone)]
pub(crate) enum Nearby {
    Looking,
    /// What the last successful look found; `trouble` is why the latest one
    /// failed, when it did.
    Found {
        macs: Vec<NearbyMac>,
        trouble: Option<Failure>,
    },
    Failed(Failure),
}

/// What the background add reports.
#[derive(Debug)]
pub(crate) enum AddUpdate {
    Event(Event),
    Finished(Result<AddReport, Failure>),
}

/// An update and the add it belongs to.
#[derive(Debug)]
pub(crate) struct RunUpdate {
    pub run: u64,
    pub update: AddUpdate,
}

pub(crate) enum Page {
    Closed,
    /// What to do on the new Mac.
    Prepare,
    /// Connect the new Mac to the same local network.
    Network,
    /// Pick a Mac nearby, or type its address.
    Choose {
        selected: Option<String>,
    },
    SignIn {
        target: Target,
    },
    Adding {
        target: Target,
        /// The step running now; earlier ones are done.
        current: Option<Step>,
        approval: Option<(LoginLink, Approval)>,
    },
    Done {
        report: AddReport,
    },
    Failed {
        target: Target,
        failure: Failure,
    },
}

pub(crate) struct AddMac {
    pub page: Page,
    /// The add running now or last; updates for any other are stale.
    run: u64,
    pub nearby: Nearby,
    pub manual: TextEdit,
    pub account: TextEdit,
    pub password: TextEdit,
}

impl Default for AddMac {
    fn default() -> Self {
        Self {
            page: Page::Closed,
            run: 0,
            nearby: Nearby::Looking,
            manual: TextEdit::default(),
            account: TextEdit::default(),
            password: TextEdit::default().secure(),
        }
    }
}

impl NearbyMacExt for NearbyMac {
    fn status(&self) -> MacStatus {
        match (&self.membership, self.remote_login, &self.model) {
            (_, RemoteLogin::Off, _) => MacStatus::RemoteLoginOff,
            (Membership::PossibleMember { .. }, ..) => MacStatus::PossibleMember,
            (_, RemoteLogin::On { .. }, None) => MacStatus::Unknown,
            (_, RemoteLogin::On { .. }, Some(_)) => MacStatus::Ready,
        }
    }
}

/// How a nearby Mac stands for adding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MacStatus {
    Ready,
    RemoteLoginOff,
    /// An SSH host that reports no model; it may not be a Mac.
    Unknown,
    PossibleMember,
}

impl MacStatus {
    /// Whether the operator can pick it.
    pub(crate) fn selectable(&self) -> bool {
        // A discoverable candidate needs Remote Login to be picked.
        !matches!(self, MacStatus::RemoteLoginOff)
    }
}

pub(crate) trait NearbyMacExt {
    fn status(&self) -> MacStatus;
}

impl AddMac {
    pub(crate) fn open(&mut self) {
        self.page = Page::Prepare;
    }

    pub(crate) fn is_open(&self) -> bool {
        !matches!(self.page, Page::Closed)
    }

    /// Closes the sheet and forgets what was typed.
    pub(crate) fn close(&mut self) {
        *self = Self {
            run: self.run,
            nearby: std::mem::replace(&mut self.nearby, Nearby::Looking),
            ..Self::default()
        };
    }

    /// The add whose updates count now.
    pub(crate) fn run(&self) -> u64 {
        self.run
    }

    /// Whether the sheet should keep looking for Macs nearby.
    pub(crate) fn looking(&self) -> bool {
        matches!(self.page, Page::Choose { .. })
    }

    pub(crate) fn adding(&self) -> bool {
        matches!(self.page, Page::Adding { .. })
    }

    pub(crate) fn set_nearby(&mut self, found: Result<Vec<NearbyMac>, Failure>) {
        self.nearby = match (found, std::mem::replace(&mut self.nearby, Nearby::Looking)) {
            (Ok(mut macs), _) => {
                macs.retain(|mac| {
                    !matches!(mac.membership, Membership::Member { .. })
                        && !mac
                            .model
                            .as_deref()
                            .is_some_and(|model| model.starts_with("VirtualMac"))
                });
                Nearby::Found {
                    macs,
                    trouble: None,
                }
            }
            // What was found stays, with why the latest look failed.
            (Err(failure), Nearby::Found { macs, .. }) => Nearby::Found {
                macs,
                trouble: Some(failure),
            },
            (Err(failure), _) => Nearby::Failed(failure),
        };
        // A picked Mac that went away is no longer picked.
        let pickable: Vec<String> = self
            .found()
            .iter()
            .filter(|mac| mac.status().selectable())
            .map(|mac| mac.host.clone())
            .collect();
        // Picked for the operator only when exactly one Mac is ready: anything on
        // the network can advertise itself, so a choice among several is theirs.
        let ready: Vec<&NearbyMac> = self
            .found()
            .iter()
            .filter(|mac| mac.status() == MacStatus::Ready)
            .collect();
        let only = (ready.len() == 1).then(|| ready[0].host.clone());
        let typing = !self.manual.text.as_str().trim().is_empty();
        if let Page::Choose { selected } = &mut self.page {
            if selected
                .as_ref()
                .is_some_and(|host| !pickable.contains(host))
            {
                *selected = None;
            }
            if selected.is_none() && !typing {
                *selected = only;
            }
        }
    }

    pub(crate) fn found(&self) -> &[NearbyMac] {
        match &self.nearby {
            Nearby::Found { macs, .. } => macs,
            _ => &[],
        }
    }

    pub(crate) fn show_network(&mut self) {
        self.page = Page::Network;
    }

    /// From the network instructions to choosing a Mac.
    pub(crate) fn show_choose(&mut self) {
        self.page = Page::Choose { selected: None };
        // Picks the first Mac ready to add among those already found.
        let found = std::mem::replace(&mut self.nearby, Nearby::Looking);
        if let Nearby::Found { macs, .. } = found {
            self.set_nearby(Ok(macs));
        } else {
            self.nearby = found;
        }
    }

    pub(crate) fn select(&mut self, host: &str) {
        if let Page::Choose { selected } = &mut self.page {
            *selected = Some(host.to_owned());
            self.manual.replace_text("");
        }
    }

    /// The Mac to add: the one picked, or a typed host name or address.
    pub(crate) fn chosen(&self) -> Option<Target> {
        let typed = self.manual.text.as_str().trim();
        if !typed.is_empty() {
            return valid_host(typed).then(|| Target {
                host: typed.to_owned(),
                name: typed.to_owned(),
                model: None,
            });
        }
        let Page::Choose {
            selected: Some(host),
        } = &self.page
        else {
            return None;
        };
        self.found()
            .iter()
            .find(|mac| &mac.host == host && mac.status().selectable() && valid_host(&mac.host))
            .map(|mac| Target {
                host: mac.host.clone(),
                name: mac.name.clone(),
                model: mac.model.clone(),
            })
    }

    pub(crate) fn show_sign_in(&mut self) {
        if let Some(target) = self.chosen() {
            self.page = Page::SignIn { target };
        }
    }

    pub(crate) fn back(&mut self) {
        self.page = match std::mem::replace(&mut self.page, Page::Closed) {
            Page::Network => Page::Prepare,
            Page::Choose { .. } => Page::Network,
            Page::SignIn { target } | Page::Failed { target, .. } => Page::Choose {
                selected: Some(target.host),
            },
            other => other,
        };
    }

    pub(crate) fn can_add(&self) -> bool {
        matches!(self.page, Page::SignIn { .. })
            && valid_account(self.account.text.trim())
            && !self.password.text.is_empty()
    }

    /// Starts adding the Mac: the password moves into the request and the
    /// field is cleared.
    pub(crate) fn start(&mut self) -> Option<AddRequest> {
        if !self.can_add() {
            return None;
        }
        let Page::SignIn { target } = std::mem::replace(&mut self.page, Page::Closed) else {
            unreachable!("can_add checked the page");
        };
        self.run += 1;
        let request = AddRequest {
            run: self.run,
            host: target.host.clone(),
            account: self.account.text.as_str().trim().to_owned(),
            password: Zeroizing::new(self.password.text.as_str().to_owned()),
        };
        self.password = TextEdit::default().secure();
        self.page = Page::Adding {
            target,
            current: None,
            approval: None,
        };
        Some(request)
    }

    /// Applies what the add reported. Returns a link to open when one arrives.
    /// Updates from an earlier add are ignored.
    pub(crate) fn apply(&mut self, RunUpdate { run, update }: RunUpdate) -> Option<LoginLink> {
        if run != self.run {
            return None;
        }
        let Page::Adding {
            target,
            current,
            approval,
        } = &mut self.page
        else {
            return None;
        };
        match update {
            AddUpdate::Event(Event::Step { step }) => {
                *current = Some(step);
                *approval = None;
                None
            }
            AddUpdate::Event(Event::Approve {
                link,
                approval: kind,
            }) => {
                *approval = Some((link.clone(), kind));
                Some(link)
            }
            AddUpdate::Finished(result) => {
                let target = target.clone();
                self.page = match result {
                    Ok(report) => Page::Done { report },
                    Err(failure) => Page::Failed { target, failure },
                };
                None
            }
        }
    }

    /// Applies an update for the add running now, as scenes do.
    pub(crate) fn apply_current(&mut self, update: AddUpdate) -> Option<LoginLink> {
        let run = self.run;
        self.apply(RunUpdate { run, update })
    }

    /// A failed add starts again from signing in, with the same account.
    pub(crate) fn retry(&mut self) {
        if let Page::Failed { target, .. } = std::mem::replace(&mut self.page, Page::Closed) {
            self.page = Page::SignIn { target };
        }
    }
}

/// Whether `step` is done, running or still to come while `current` runs.
pub(crate) fn step_state(step: Step, current: Option<Step>) -> StepState {
    let index = |s: Step| STEPS.iter().position(|x| *x == s).unwrap_or(0);
    match current {
        None => StepState::Pending,
        Some(current) if index(step) < index(current) => StepState::Done,
        Some(current) if step == current => StepState::Running,
        Some(_) => StepState::Pending,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepState {
    Done,
    Running,
    Pending,
}

/// The labels a step shows in the sheet.
pub(crate) fn step_label(step: Step) -> &'static str {
    match step {
        Step::Check => "Check the Mac",
        Step::Tailnet => "Join your tailnet",
        Step::Install => "Install Elsecall",
        Step::Firewall => "Open the firewall",
        Step::SshKey => "Authorize this Mac's SSH key",
        Step::Enroll => "Enroll in the fleet",
        Step::Verify => "Verify it answers",
    }
}

/// A host the CLI accepts: a plain name or address.
fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && !host.starts_with('-')
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'%'))
}

/// A Mac nearby, for scenes and tests.
pub(crate) fn mac(name: &str, remote_login: bool, member: Option<&str>) -> NearbyMac {
    NearbyMac {
        name: name.into(),
        host: format!("{}.local", name.to_lowercase().replace(' ', "-")),
        addresses: vec!["192.168.64.30".parse().unwrap()],
        model: Some("Mac16,10".into()),
        remote_login: if remote_login {
            RemoteLogin::On { ssh_port: 22 }
        } else {
            RemoteLogin::Off
        },
        membership: match member {
            Some(member) => Membership::Member {
                member: member.into(),
            },
            None => Membership::NotMember,
        },
    }
}

/// A macOS short account name: letters, digits, `.`, `_` and `-`, not first.
fn valid_account(account: &str) -> bool {
    !account.is_empty()
        && account.len() <= 255
        && !account.starts_with('-')
        && account
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sheet_walks_from_instructions_to_adding_and_forgets_the_password() {
        let mut flow = AddMac::default();
        flow.open();
        flow.set_nearby(Ok(vec![
            mac("Studio", false, None),
            mac("MacBook Pro", true, None),
            mac("Mac mini", true, Some("mac-mini")),
        ]));
        flow.show_choose();
        // The first Mac that can be added is picked for the operator.
        assert_eq!(flow.chosen().unwrap().host, "macbook-pro.local");
        flow.show_sign_in();
        assert!(!flow.can_add());
        flow.account.replace_text("devbox");
        flow.password.replace_text("s3cret");
        let request = flow.start().unwrap();
        assert_eq!(
            (
                request.host.as_str(),
                request.account.as_str(),
                request.password.as_str()
            ),
            ("macbook-pro.local", "devbox", "s3cret")
        );
        assert_eq!(
            flow.password.text.as_str(),
            "",
            "the field no longer holds it"
        );
        assert!(flow.password.is_secure());
        assert!(flow.adding());
    }

    #[test]
    fn progress_steps_approval_and_the_end_are_applied() {
        let mut flow = AddMac::default();
        flow.open();
        flow.set_nearby(Ok(vec![mac("MacBook Pro", true, None)]));
        flow.show_choose();
        flow.show_sign_in();
        flow.account.replace_text("devbox");
        flow.password.replace_text("s3cret");
        flow.start();
        assert_eq!(
            flow.apply_current(AddUpdate::Event(Event::Step {
                step: Step::Tailnet
            })),
            None
        );
        let link = LoginLink::parse("https://login.tailscale.com/a/example").unwrap();
        let opened = flow.apply_current(AddUpdate::Event(Event::Approve {
            link: link.clone(),
            approval: Approval::Login,
        }));
        assert_eq!(opened, Some(link));
        let Page::Adding {
            current, approval, ..
        } = &flow.page
        else {
            panic!("adding");
        };
        assert_eq!(*current, Some(Step::Tailnet));
        assert!(approval.is_some());
        assert_eq!(step_state(Step::Check, *current), StepState::Done);
        assert_eq!(step_state(Step::Tailnet, *current), StepState::Running);
        assert_eq!(step_state(Step::Verify, *current), StepState::Pending);
        // The next step clears the approval.
        flow.apply_current(AddUpdate::Event(Event::Step {
            step: Step::Install,
        }));
        assert!(matches!(&flow.page, Page::Adding { approval: None, .. }));
        flow.apply_current(AddUpdate::Finished(Err(Failure::new("x", "m", "h"))));
        assert!(matches!(flow.page, Page::Failed { .. }));
        flow.retry();
        assert!(matches!(flow.page, Page::SignIn { .. }));
        assert_eq!(
            flow.account.text.as_str(),
            "devbox",
            "the account stays for a retry"
        );
    }

    #[test]
    fn a_typed_address_wins_and_members_cannot_be_picked() {
        let mut flow = AddMac::default();
        flow.open();
        flow.set_nearby(Ok(vec![mac("Mac mini", true, Some("mac-mini"))]));
        flow.show_choose();
        assert_eq!(flow.chosen(), None, "a member is not picked");
        flow.manual.replace_text(" 192.168.64.30 ");
        assert_eq!(flow.chosen().unwrap().host, "192.168.64.30");
        flow.manual.replace_text("a b; rm");
        assert_eq!(flow.chosen(), None, "only a plain host name or address");
        assert_eq!(
            mac("Studio", false, None).status(),
            MacStatus::RemoteLoginOff
        );
        assert!(!MacStatus::RemoteLoginOff.selectable());
    }

    #[test]
    fn a_failed_look_keeps_the_macs_already_found_and_a_lost_pick_is_dropped() {
        let mut flow = AddMac::default();
        flow.open();
        flow.set_nearby(Ok(vec![mac("MacBook Pro", true, None)]));
        flow.show_choose();
        flow.set_nearby(Err(Failure::new("bonjour_unavailable", "m", "h")));
        assert_eq!(flow.found().len(), 1);
        assert!(matches!(
            &flow.nearby,
            Nearby::Found {
                trouble: Some(_),
                ..
            }
        ));
        flow.set_nearby(Ok(vec![]));
        assert_eq!(flow.chosen(), None);
        flow.close();
        assert!(!flow.is_open());
    }
}

#[cfg(test)]
mod safety_tests {
    use super::*;

    fn choosing(macs: Vec<NearbyMac>) -> AddMac {
        let mut flow = AddMac::default();
        flow.open();
        flow.show_choose();
        flow.set_nearby(Ok(macs));
        flow
    }

    #[test]
    fn a_mac_is_picked_for_the_operator_only_when_it_is_the_only_one_ready() {
        let one = choosing(vec![
            mac("MacBook Pro", true, None),
            mac("Studio", false, None),
        ]);
        assert_eq!(one.chosen().unwrap().host, "macbook-pro.local");
        let two = choosing(vec![
            mac("MacBook Pro", true, None),
            mac("Mac Studio", true, None),
        ]);
        assert_eq!(two.chosen(), None, "with two ready, the operator picks");
        let member = choosing(vec![mac("Mac mini", true, Some("mac-mini"))]);
        assert_eq!(
            member.chosen(),
            None,
            "a member is never picked for the operator"
        );
    }

    #[test]
    fn virtual_macs_and_members_are_hidden_but_can_be_entered_by_address() {
        let mut vm = mac("Virtual Mac", true, None);
        vm.model = Some("VirtualMac2,1".into());
        let mut possible = mac("Studio", true, None);
        possible.membership = Membership::PossibleMember {
            member: "studio".into(),
        };
        let mut flow = choosing(vec![vm, mac("Mac mini", true, Some("mac-mini")), possible]);
        assert_eq!(flow.found().len(), 1);
        assert_eq!(
            flow.chosen(),
            None,
            "a possible member is never picked automatically"
        );
        flow.select("studio.local");
        assert_eq!(flow.chosen().unwrap().host, "studio.local");
        flow.manual.replace_text("virtual-mac.local");
        assert_eq!(flow.chosen().unwrap().host, "virtual-mac.local");
        flow.manual.replace_text("mac-mini.local");
        assert_eq!(flow.chosen().unwrap().host, "mac-mini.local");
    }

    #[test]
    fn hosts_and_accounts_that_could_be_options_are_refused() {
        let mut hostile = mac("Evil", true, None);
        hostile.host = "-oProxyCommand=x".into();
        let mut flow = choosing(vec![hostile]);
        flow.select("-oProxyCommand=x");
        assert_eq!(flow.chosen(), None);
        let mut flow = choosing(vec![mac("MacBook Pro", true, None)]);
        flow.show_sign_in();
        flow.password.replace_text("s3cret");
        for bad in ["-h", "a b", "", "x;y"] {
            flow.account.replace_text(bad);
            assert!(!flow.can_add(), "{bad:?}");
        }
        flow.account.replace_text("devbox");
        assert!(flow.can_add());
    }

    #[test]
    fn updates_from_an_earlier_add_are_ignored() {
        let mut flow = choosing(vec![mac("MacBook Pro", true, None)]);
        flow.show_sign_in();
        flow.account.replace_text("devbox");
        flow.password.replace_text("s3cret");
        let first = flow.start().unwrap().run;
        flow.close();
        flow.open();
        flow.show_choose();
        flow.set_nearby(Ok(vec![mac("MacBook Pro", true, None)]));
        flow.show_sign_in();
        flow.account.replace_text("devbox");
        flow.password.replace_text("s3cret");
        let second = flow.start().unwrap().run;
        assert_ne!(first, second);
        flow.apply(RunUpdate {
            run: first,
            update: AddUpdate::Finished(Err(Failure::new("add_stopped", "m", "h"))),
        });
        assert!(flow.adding(), "the earlier add's end does not end this one");
    }
}
