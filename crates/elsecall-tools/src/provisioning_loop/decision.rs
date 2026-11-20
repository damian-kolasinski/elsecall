//! What the loop sends a member once it has planned it: the plan's steps,
//! each either run or held back, or nothing at all.
//!
//! A step that failed in the member's latest apply is held when only a person
//! can unblock it (Xcode privileges, an Xcode the master lacks) while that
//! cause stands, and when retrying is not due yet; a held step is recorded
//! failed again as it was, so its attention item stands with its own `since`.
use crate::{
    install::{
        record::{Record, State},
        request::{HeldStep, InstallStep},
    },
    tool_inventory::{Manager, ToolVersion},
};
use elsecall_contract::scheduling_model::timestamp::Timestamp;
use std::time::Duration;

/// A failure retrying can fix is retried once this has passed, at the latest.
pub(crate) const RETRY_AFTER: Duration = Duration::from_secs(6 * 60 * 60);

/// The codes only a person can unblock.
pub(crate) const PRIVILEGES_MISSING: &str = "xcode_privileges_missing";
pub(crate) const NOT_ON_MASTER: &str = "xcode_not_on_master";

/// What decides whether a failed step is held.
pub(crate) struct Holds<'a> {
    /// Whether the member's Xcode privileges are ready now.
    pub(crate) privileges_ready: bool,
    /// The master's Xcodes; none when it could not read them.
    pub(crate) master_xcodes: Option<&'a [ToolVersion]>,
    /// Whether failures retrying can fix are retried now: something changed
    /// since the apply that failed them, or someone asked.
    pub(crate) retry: bool,
    pub(crate) now: Timestamp,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Act {
    /// Nothing to send: the latest apply already shows what this one would.
    Nothing,
    Apply {
        steps: Vec<InstallStep>,
        held: Vec<HeldStep>,
    },
}

/// The decision for one member, and the steps held back in it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Decision {
    pub(crate) act: Act,
    pub(crate) held: Vec<HeldStep>,
}

pub(crate) fn decide(steps: &[InstallStep], latest: Option<&Record>, holds: &Holds) -> Decision {
    let last = |step: &InstallStep| {
        latest?
            .entries
            .iter()
            .find(|entry| &entry.step == step)
            .map(|entry| entry.state.clone())
    };
    let mut send = Vec::new();
    let mut held = Vec::new();
    for step in steps {
        match last(step) {
            Some(State::Failed(failure)) if holds.hold(step, &failure.code, failure.at) => {
                held.push(HeldStep {
                    step: step.clone(),
                    stage: failure.stage,
                    code: failure.code,
                    failed_at: failure.at,
                });
            }
            // Installed, and still planned: the inventory does not see it. Sent
            // again only once something changed, not after every apply.
            Some(State::Installed { .. }) if !holds.retry => {}
            _ => send.push(step.clone()),
        }
    }
    let standing: Vec<(&InstallStep, &str)> = latest
        .into_iter()
        .flat_map(|record| &record.entries)
        .filter_map(|entry| match &entry.state {
            State::Failed(failure) => Some((&entry.step, failure.code.as_str())),
            _ => None,
        })
        .collect();
    let unchanged = send.is_empty()
        && standing.len() == held.len()
        && held
            .iter()
            .all(|held| standing.contains(&(&held.step, held.code.as_str())));
    let act = if unchanged {
        Act::Nothing
    } else {
        Act::Apply {
            steps: send,
            held: held.clone(),
        }
    };
    Decision { act, held }
}

impl Holds<'_> {
    fn hold(&self, step: &InstallStep, code: &str, failed_at: Timestamp) -> bool {
        match code {
            PRIVILEGES_MISSING => !self.privileges_ready,
            NOT_ON_MASTER => {
                step.manager == Manager::Xcodes
                    && !self.master_xcodes.is_some_and(|xcodes| {
                        xcodes
                            .iter()
                            .any(|version| version.as_str() == step.version.as_str())
                    })
            }
            _ => !self.retry && failed_at.after(RETRY_AFTER) > self.now,
        }
    }
}

/// Whether a held step waits for a person rather than for its retry.
pub(crate) fn for_person(code: &str) -> bool {
    matches!(code, PRIVILEGES_MISSING | NOT_ON_MASTER)
}

#[cfg(test)]
mod tests {
    use crate::{
        install::{
            record::{Failure, Record, State},
            request::{ApplyRequest, InstallStep},
        },
        provisioning_loop::decision::*,
    };
    use serde_json::json;

    fn steps() -> Vec<InstallStep> {
        serde_json::from_value::<ApplyRequest>(json!({"steps": [
            {"manager": "mise", "tool": "node", "version": "22.19.0"},
            {"manager": "xcodes", "tool": "xcode", "version": "26.2"},
            {"manager": "simulators", "tool": "iOS", "version": "26.2"},
        ]}))
        .unwrap()
        .accept()
        .unwrap()
        .0
    }

    fn at(text: &str) -> Timestamp {
        Timestamp::parse_rfc3339(text).unwrap()
    }

    /// node failed to install, Xcode lacked privileges, the runtime installed.
    fn latest() -> Record {
        let mut record = Record::new(steps(), Vec::new());
        let failed = |stage: &str, code: &str| {
            State::Failed(Failure {
                stage: stage.into(),
                code: code.into(),
                at: at("2026-11-20T09:00:00Z"),
            })
        };
        record.entries[0].state = failed("install", "tool_install_failed");
        record.entries[1].state = failed("privileges", PRIVILEGES_MISSING);
        record.entries[2].state = State::Installed {
            at: at("2026-11-20T09:00:00Z"),
        };
        record.finished_at = Some(at("2026-11-20T09:01:00Z"));
        record
    }

    fn holds(privileges_ready: bool, retry: bool, now: &str) -> Holds<'static> {
        Holds {
            privileges_ready,
            master_xcodes: None,
            retry,
            now: at(now),
        }
    }

    fn names(steps: &[InstallStep]) -> Vec<&str> {
        steps.iter().map(|step| step.tool.as_str()).collect()
    }

    #[test]
    fn a_new_plan_runs_every_step() {
        let decision = decide(&steps(), None, &holds(false, false, "2026-11-20T10:00:00Z"));
        assert_eq!(
            decision.act,
            Act::Apply {
                steps: steps(),
                held: Vec::new()
            }
        );
    }

    #[test]
    fn failures_wait_for_their_cause_or_their_retry() {
        let plan = &steps()[..2];
        let latest = latest();
        let soon = decide(
            plan,
            Some(&latest),
            &holds(false, false, "2026-11-20T10:00:00Z"),
        );
        assert_eq!(soon.act, Act::Nothing, "the latest apply already says it");
        assert_eq!(
            names(&soon.held.iter().map(|h| h.step.clone()).collect::<Vec<_>>()),
            ["node", "xcode"]
        );

        let changed = decide(
            plan,
            Some(&latest),
            &holds(false, true, "2026-11-20T10:00:00Z"),
        );
        let Act::Apply { steps, held } = changed.act else {
            panic!("a change retries what retrying can fix");
        };
        assert_eq!(names(&steps), ["node"]);
        assert_eq!(held[0].code, PRIVILEGES_MISSING, "not what needs a person");
        assert_eq!(held[0].failed_at, at("2026-11-20T09:00:00Z"));

        let later = decide(
            plan,
            Some(&latest),
            &holds(false, false, "2026-11-20T15:00:01Z"),
        );
        assert!(matches!(later.act, Act::Apply { ref steps, .. } if names(steps) == ["node"]));

        let ready = decide(
            plan,
            Some(&latest),
            &holds(true, false, "2026-11-20T10:00:00Z"),
        );
        assert!(
            matches!(ready.act, Act::Apply { ref steps, ref held } if names(steps) == ["xcode"] && names(&held.iter().map(|h| h.step.clone()).collect::<Vec<_>>()) == ["node"]),
            "privileges installed: Xcode goes, node still waits"
        );
    }

    #[test]
    fn an_xcode_the_master_lacks_waits_for_the_master() {
        let mut latest = latest();
        latest.entries[1].state = State::Failed(Failure {
            stage: "copy".into(),
            code: NOT_ON_MASTER.into(),
            at: at("2026-11-20T09:00:00Z"),
        });
        let plan = &steps()[1..2];
        let lacking = Holds {
            master_xcodes: Some(&[]),
            ..holds(true, true, "2026-11-21T10:00:00Z")
        };
        let decision = decide(plan, Some(&latest), &lacking);
        assert!(
            matches!(decision.act, Act::Apply { ref steps, .. } if steps.is_empty()),
            "node's failure no longer stands, so an apply ends it: {decision:?}"
        );
        assert_eq!(decision.held[0].code, NOT_ON_MASTER);
        let version = ToolVersion::new("26.2").unwrap();
        let has = Holds {
            master_xcodes: Some(std::slice::from_ref(&version)),
            ..holds(true, false, "2026-11-20T10:00:00Z")
        };
        assert!(matches!(decide(plan, Some(&latest), &has).act,
            Act::Apply { ref steps, .. } if names(steps) == ["xcode"]));
    }

    #[test]
    fn a_step_installed_but_still_planned_waits_for_a_change() {
        let latest = latest();
        let plan = &steps()[2..];
        let quiet = decide(
            plan,
            Some(&latest),
            &holds(false, false, "2026-11-20T10:00:00Z"),
        );
        assert!(
            matches!(quiet.act, Act::Apply { ref steps, .. } if steps.is_empty()),
            "nothing to run, and the failures no longer planned end: {quiet:?}"
        );
        let changed = decide(
            plan,
            Some(&latest),
            &holds(false, true, "2026-11-20T10:00:00Z"),
        );
        assert!(matches!(changed.act, Act::Apply { ref steps, .. } if names(steps) == ["iOS"]));
    }

    #[test]
    fn nothing_left_ends_standing_failures_once() {
        let latest = latest();
        let decision = decide(
            &[],
            Some(&latest),
            &holds(false, false, "2026-11-20T10:00:00Z"),
        );
        assert_eq!(
            decision.act,
            Act::Apply {
                steps: Vec::new(),
                held: Vec::new()
            }
        );
        let mut clean = latest.clone();
        clean
            .entries
            .retain(|entry| !matches!(entry.state, State::Failed(_)));
        assert_eq!(
            decide(
                &[],
                Some(&clean),
                &holds(false, false, "2026-11-20T10:00:00Z")
            )
            .act,
            Act::Nothing
        );
        assert_eq!(
            decide(&[], None, &holds(false, false, "2026-11-20T10:00:00Z")).act,
            Act::Nothing
        );
    }
}
