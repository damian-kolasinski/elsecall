//! What the loop remembers about each member between minutes, and when a member
//! is due to be planned. Pure: the loop feeds it what it read and the time.
//!
//! A member's **want** is what its plan depends on: every member's pins (each
//! member's last known, so one that sleeps keeps its pins counting), the
//! master's tools, which resolve every pin, and the member's own tools. A
//! changed want waits until it has stayed the same for the quiet period, and
//! at most the longest deferral, before the member is due.
use crate::{
    drift::{ApplySeen, Signal},
    install::request::HeldStep,
    provisioning_loop::decision,
};
use elsecall_contract::{failure::Failure, scheduling_model::timestamp::Timestamp};
use elsecall_fleet::provisioning_model::ApplyState;
use elsecall_peers::roster::MemberName;
use std::{collections::BTreeMap, time::Duration};
use tokio::time::Instant;

/// How long the loop waits, and how often it looks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    pub(crate) poll: Duration,
    /// How long a change must stay put before it is planned.
    pub(crate) quiet: Duration,
    /// The longest a change waits for that, and the longest a member waits
    /// after a plan or apply that failed.
    pub(crate) longest: Duration,
    /// A member not planned for this long is planned anyway.
    pub(crate) periodic: Duration,
}

impl Timing {
    pub(crate) const PRODUCTION: Self = Self {
        poll: Duration::from_secs(60),
        quiet: Duration::from_secs(60),
        longest: Duration::from_secs(10 * 60),
        periodic: Duration::from_secs(6 * 60 * 60),
    };
}

/// Why a member is planned now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trigger {
    /// Someone asked: `elsecall tools provision`.
    Asked,
    /// What its plan depends on changed, or it is new.
    Changed,
    /// Its latest apply finished.
    Finished,
    /// The last plan or apply failed, or marking it ready was refused.
    Again,
    /// It was not planned for six hours.
    Periodic,
}

/// What the last plan said about a member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Planned {
    pub(crate) at: Timestamp,
    pub(crate) steps: usize,
    pub(crate) unresolved: usize,
    pub(crate) unread: usize,
    /// Some pins could not be read: a member's workspaces, a workspace, or
    /// the master's global config. Its plan may lack what they pin.
    pub(crate) partial: bool,
    pub(crate) held: Vec<HeldStep>,
}

impl Planned {
    /// Everything the fleet's pins ask of it is installed, as far as anyone
    /// can tell: what makes a member awaiting provisioning ready.
    pub(crate) fn complete(&self) -> bool {
        self.steps == 0 && self.unresolved == 0 && self.unread == 0 && !self.partial
    }
}

/// Where marking a member ready stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Ready {
    /// Nothing to do.
    #[default]
    Settled,
    /// Refused, as while a member is unreachable: planned again, then tried.
    Refused,
    /// Recorded here, but not every member received it: delivered again.
    Undelivered,
}

#[derive(Debug, Default)]
pub(crate) struct Tracked {
    pub(crate) signal: Option<Signal>,
    pub(crate) signal_at: Option<Timestamp>,
    /// Why its signal could not be read at the last look.
    pub(crate) unreachable: Option<Failure>,
    /// The want last seen, and since when it has been the same.
    want: Option<String>,
    want_since: Option<Instant>,
    /// When a change not yet planned was first seen.
    pending_since: Option<Instant>,
    /// The want it was last planned at.
    settled: Option<String>,
    /// The want when its latest apply was first seen finished.
    baseline: Option<String>,
    apply: Option<ApplySeen>,
    finished: bool,
    /// When someone asked for it, until a plan begun after that settles it.
    asked_at: Option<Instant>,
    planned_instant: Option<Instant>,
    pub(crate) planned: Option<Planned>,
    /// Why the last plan or apply for it went wrong.
    pub(crate) error: Option<Failure>,
    /// Failures in a row, and when it may be tried again.
    failures: u32,
    not_before: Option<Instant>,
    pub(crate) ready: Ready,
}

impl Tracked {
    /// Takes in this minute's signal, with `fleet`, what every member's plan
    /// depends on: all pins and the master's tools.
    pub(crate) fn observe(&mut self, signal: Signal, fleet: &str, now: Instant) {
        let want = format!("{fleet}:{}", signal.tools);
        if self.want.as_ref() != Some(&want) {
            self.want = Some(want.clone());
            self.want_since = Some(now);
        }
        if self.settled.as_ref() != Some(&want) {
            self.pending_since.get_or_insert(now);
        } else {
            self.pending_since = None;
        }
        let previous = self.apply;
        self.apply = signal.apply;
        if let Some(apply) = signal.apply
            && apply.state == ApplyState::Finished
            && previous.is_some_and(|seen| {
                seen.started_at != apply.started_at || seen.state == ApplyState::Running
            })
        {
            self.finished = true;
            self.baseline = Some(want);
        }
        self.signal = Some(signal);
        self.unreachable = None;
    }

    pub(crate) fn lost(&mut self, failure: Failure) {
        self.unreachable = Some(failure);
    }

    /// The loop started an apply there itself, so its end is news even if
    /// it ends before the next look.
    pub(crate) fn sent(&mut self, apply: ApplySeen) {
        self.apply = Some(apply);
    }

    pub(crate) fn ask(&mut self, now: Instant) {
        self.asked_at = Some(now);
    }

    pub(crate) fn applying(&self) -> bool {
        self.unreachable.is_none()
            && self
                .apply
                .is_some_and(|apply| apply.state == ApplyState::Running)
    }

    /// Why the member is due now, if it is.
    pub(crate) fn due(&self, now: Instant, timing: &Timing) -> Option<Trigger> {
        if self.unreachable.is_some() || self.signal.is_none() || self.applying() {
            return None;
        }
        if self.asked_at.is_some() {
            return Some(Trigger::Asked);
        }
        if self.not_before.is_some_and(|at| now < at) {
            return None;
        }
        if self.finished {
            return Some(Trigger::Finished);
        }
        if let (Some(pending), Some(since)) = (self.pending_since, self.want_since)
            && (now.duration_since(since) >= timing.quiet
                || now.duration_since(pending) >= timing.longest)
        {
            return Some(Trigger::Changed);
        }
        // A mark only some members received is delivered again, unplanned.
        if self.ready == Ready::Refused
            || (self.error.is_some() && self.ready != Ready::Undelivered)
        {
            return Some(Trigger::Again);
        }
        self.planned_instant
            .is_some_and(|at| now.duration_since(at) >= timing.periodic)
            .then_some(Trigger::Periodic)
    }

    /// Whether a ready mark is due to be delivered again, outside any plan.
    pub(crate) fn redeliver(&self, now: Instant) -> bool {
        self.ready == Ready::Undelivered && self.not_before.is_none_or(|at| now >= at)
    }

    /// Whether failures retrying can fix are retried in this plan: someone
    /// asked, or something changed since the apply that failed them.
    pub(crate) fn retry(&self, trigger: Trigger) -> bool {
        trigger == Trigger::Asked || self.baseline.is_none() || self.baseline != self.want
    }

    /// Records the plan begun at `started` and made for it now. Whether it
    /// was acted on, the caller says next: [`Self::succeeded`] or
    /// [`Self::failed`].
    pub(crate) fn settle(&mut self, planned: Planned, started: Instant) {
        self.settled = self.want.clone();
        self.pending_since = None;
        self.finished = false;
        self.attempted(started);
        self.planned_instant = Some(started);
        self.planned = Some(planned);
        if self.ready == Ready::Refused {
            self.ready = Ready::Settled;
        }
    }

    /// An ask made before the attempt begun at `started` is answered by it,
    /// whether it succeeded or not.
    fn attempted(&mut self, started: Instant) {
        if self.asked_at.is_some_and(|asked| asked <= started) {
            self.asked_at = None;
        }
    }

    /// The plan was acted on: no failure stands, and the next one waits for
    /// a reason again.
    pub(crate) fn succeeded(&mut self) {
        self.error = None;
        self.failures = 0;
        self.not_before = None;
    }

    /// A plan, apply or ready mark begun at `started` failed: tried again
    /// after a wait that starts at a poll and doubles each time in a row, up
    /// to the longest.
    pub(crate) fn failed(
        &mut self,
        failure: Failure,
        started: Instant,
        now: Instant,
        timing: &Timing,
    ) {
        self.attempted(started);
        self.error = Some(failure);
        self.failures = self.failures.saturating_add(1);
        let wait = timing
            .poll
            .saturating_mul(1 << (self.failures - 1).min(16))
            .min(timing.longest);
        self.not_before = Some(now + wait);
    }

    /// How marking it ready went.
    pub(crate) fn readied(&mut self, ready: Ready) {
        self.ready = ready;
        if ready == Ready::Settled {
            self.succeeded();
        }
    }

    /// Its state, as `tools provisioning` names it.
    pub(crate) fn state(&self) -> &'static str {
        if self.unreachable.is_some() {
            return "unreachable";
        }
        if self.applying() {
            return "applying";
        }
        if self.pending_since.is_some() || self.asked_at.is_some() || self.finished {
            return "waiting";
        }
        let Some(planned) = &self.planned else {
            return "unknown";
        };
        if planned
            .held
            .iter()
            .any(|held| decision::for_person(&held.code))
        {
            return "needs_person";
        }
        if planned.complete() && self.error.is_none() && self.ready == Ready::Settled {
            "in_step"
        } else {
            "behind"
        }
    }
}

/// Every member the loop follows, by name.
#[derive(Debug, Default)]
pub(crate) struct Members(pub(crate) BTreeMap<MemberName, Tracked>);

impl Members {
    /// Keeps only `names`, the roster's members, adding new ones.
    pub(crate) fn follow(&mut self, names: &[MemberName]) {
        self.0.retain(|name, _| names.contains(name));
        for name in names {
            self.0.entry(name.clone()).or_default();
        }
    }

    /// What every member's plan depends on: each member's last known pins,
    /// by name, and the master's tools.
    pub(crate) fn fleet(&self, master_tools: &str) -> String {
        let mut fleet = String::from(master_tools);
        for (name, tracked) in &self.0 {
            let pins = tracked.signal.as_ref().map_or("", |signal| &signal.pins);
            fleet.push_str(&format!(";{name}={pins}"));
        }
        fleet
    }
}

#[cfg(test)]
mod tests {
    use crate::{drift::*, provisioning_loop::tracking::*};

    const TIMING: Timing = Timing {
        poll: Duration::from_secs(60),
        quiet: Duration::from_secs(60),
        longest: Duration::from_secs(600),
        periodic: Duration::from_secs(6 * 3600),
    };

    fn signal(tools: &str, apply: Option<(ApplyState, &str)>) -> Signal {
        Signal {
            pins: String::new(),
            tools: tools.into(),
            privileges_ready: false,
            apply: apply.map(|(state, at)| ApplySeen {
                state,
                started_at: Timestamp::parse_rfc3339(at).unwrap(),
            }),
        }
    }

    fn planned() -> Planned {
        Planned {
            at: Timestamp::now(),
            steps: 0,
            unresolved: 0,
            unread: 0,
            partial: false,
            held: Vec::new(),
        }
    }

    #[test]
    fn a_change_waits_until_it_settles() {
        let start = Instant::now();
        let mut member = Tracked::default();
        member.observe(signal("a", None), "m", start);
        assert_eq!(member.state(), "waiting", "new members are planned too");
        assert_eq!(member.due(start, &TIMING), None);
        let minute = start + Duration::from_secs(60);
        member.observe(signal("a", None), "m", minute);
        assert_eq!(member.due(minute, &TIMING), Some(Trigger::Changed));
        member.settle(planned(), minute);
        assert_eq!(member.state(), "in_step");
        assert_eq!(member.due(minute, &TIMING), None, "nothing changed");

        // A change that keeps changing waits at most ten minutes.
        let mut at = minute;
        for step in 0..=10 {
            at += Duration::from_secs(60);
            member.observe(signal(&format!("b{step}"), None), "m", at);
        }
        assert_eq!(member.due(at, &TIMING), Some(Trigger::Changed));
        member.settle(planned(), at);

        // The fleet's change is every member's.
        at += Duration::from_secs(60);
        member.observe(signal("b10", None), "m2", at);
        assert_eq!(member.state(), "waiting");
        let later = at + TIMING.periodic + Duration::from_secs(60);
        member.observe(signal("b10", None), "m2", later);
        member.settle(planned(), later);
        assert_eq!(
            member.due(later + TIMING.periodic, &TIMING),
            Some(Trigger::Periodic)
        );
    }

    #[test]
    fn every_members_pins_count_for_every_member() {
        let mut members = Members::default();
        let alpha: MemberName = "alpha".parse().unwrap();
        let bravo: MemberName = "bravo".parse().unwrap();
        members.follow(&[alpha.clone(), bravo.clone()]);
        let start = Instant::now();
        let with_pins = |pins: &str| Signal {
            pins: pins.into(),
            ..signal("t", None)
        };
        members
            .0
            .get_mut(&bravo)
            .unwrap()
            .observe(with_pins("p1"), "", start);
        let before = members.fleet("m");
        members
            .0
            .get_mut(&bravo)
            .unwrap()
            .observe(with_pins("p2"), "", start);
        assert_ne!(members.fleet("m"), before, "a pin changed on bravo");
        let changed = members.fleet("m");
        members
            .0
            .get_mut(&bravo)
            .unwrap()
            .lost(Failure::new("x", "x", "x"));
        assert_eq!(
            members.fleet("m"),
            changed,
            "an unreachable member's pins still count as last seen"
        );
        assert!(members.fleet("m").contains("bravo=p2"));
    }

    #[test]
    fn a_finished_apply_is_checked_and_failures_wait_for_a_change() {
        let start = Instant::now();
        let mut member = Tracked::default();
        member.observe(signal("a", None), "m", start);
        member.settle(planned(), start);
        member.sent(ApplySeen {
            state: ApplyState::Running,
            started_at: Timestamp::parse_rfc3339("2026-11-20T09:00:00Z").unwrap(),
        });
        assert_eq!(member.state(), "applying");
        assert_eq!(member.due(start, &TIMING), None, "never while it applies");
        // It finished before the next look: still news.
        let finished = signal("b", Some((ApplyState::Finished, "2026-11-20T09:00:00Z")));
        let minute = start + Duration::from_secs(60);
        member.observe(finished.clone(), "m", minute);
        assert_eq!(member.due(minute, &TIMING), Some(Trigger::Finished));
        assert!(
            !member.retry(Trigger::Finished),
            "what it changed itself is no reason to retry"
        );
        assert!(member.retry(Trigger::Asked));
        member.settle(planned(), minute);
        member.observe(finished.clone(), "m", minute + Duration::from_secs(60));
        assert_eq!(member.due(minute + Duration::from_secs(60), &TIMING), None);
        let changed = signal("c", Some((ApplyState::Finished, "2026-11-20T09:00:00Z")));
        member.observe(changed, "m", minute + Duration::from_secs(120));
        assert!(member.retry(Trigger::Changed), "something changed since");

        // A member re-added without its record is no longer applying.
        member.sent(ApplySeen {
            state: ApplyState::Running,
            started_at: Timestamp::now(),
        });
        member.observe(signal("c", None), "m", minute + Duration::from_secs(180));
        assert!(!member.applying());
    }

    #[test]
    fn failures_back_off_and_asking_overrides_them() {
        let start = Instant::now();
        let mut member = Tracked::default();
        member.observe(signal("a", None), "m", start);
        let fail = |member: &mut Tracked| {
            member.failed(Failure::new("x", "x", "x"), start, start, &TIMING);
        };
        fail(&mut member);
        assert_eq!(member.due(start + Duration::from_secs(59), &TIMING), None);
        assert_eq!(
            member.due(start + Duration::from_secs(60), &TIMING),
            Some(Trigger::Changed),
            "a minute after the first failure"
        );
        // A plan that settles and then fails to apply keeps doubling.
        member.settle(planned(), start);
        fail(&mut member);
        assert_eq!(member.due(start + Duration::from_secs(119), &TIMING), None);
        assert_eq!(
            member.due(start + Duration::from_secs(120), &TIMING),
            Some(Trigger::Again)
        );
        for _ in 0..4 {
            fail(&mut member);
        }
        assert_eq!(
            member.due(start + Duration::from_secs(599), &TIMING),
            None,
            "at most ten minutes"
        );
        assert_eq!(
            member.due(start + Duration::from_secs(600), &TIMING),
            Some(Trigger::Again)
        );
        member.ask(start + Duration::from_secs(1));
        assert_eq!(member.due(start, &TIMING), Some(Trigger::Asked));
        // An ask that fails is answered, and waits like any failure.
        member.failed(
            Failure::new("x", "x", "x"),
            start + Duration::from_secs(2),
            start,
            &TIMING,
        );
        assert_eq!(member.due(start, &TIMING), None);
        member.succeeded();
        member.readied(Ready::Undelivered);
        member.failed(Failure::new("x", "x", "x"), start, start, &TIMING);
        assert_eq!(
            member.due(start + Duration::from_secs(3600), &TIMING),
            None,
            "an undelivered mark is delivered again, not planned"
        );
        assert!(member.redeliver(start + Duration::from_secs(3600)));
    }

    #[test]
    fn an_ask_during_a_plan_is_kept_for_the_next() {
        let start = Instant::now();
        let mut member = Tracked::default();
        member.observe(signal("a", None), "m", start);
        member.ask(start + Duration::from_secs(1));
        member.settle(planned(), start);
        assert_eq!(member.due(start, &TIMING), Some(Trigger::Asked));
        member.settle(planned(), start + Duration::from_secs(2));
        assert_eq!(member.due(start, &TIMING), None);
    }

    #[test]
    fn an_apply_finished_before_the_loop_started_is_not_news() {
        let start = Instant::now();
        let mut member = Tracked::default();
        let finished = signal("a", Some((ApplyState::Finished, "2026-11-20T09:00:00Z")));
        member.observe(finished, "m", start);
        assert!(!member.finished);
        assert!(member.retry(Trigger::Changed), "a restart is a change");
    }

    #[test]
    fn a_partial_plan_is_not_complete() {
        let mut partial = planned();
        assert!(partial.complete());
        partial.partial = true;
        assert!(!partial.complete());
    }
}
