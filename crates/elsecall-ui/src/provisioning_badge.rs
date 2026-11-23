//! What a member's status cell says about installing its tools: the step an
//! apply is on, a step that waits for a person, the Provision button of a
//! member awaiting provisioning, or nothing, which leaves the usual status.
use crate::fleet_source::MemberRow;
use elsecall_contract::attention::{Category, Item};
use elsecall_fleet::provisioning_model::ApplyState;
use elsecall_peers::roster::Readiness;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Badge {
    /// An apply runs there: the step it is on, of how many.
    Installing { step: usize, of: usize },
    /// A provisioning step waits for a person, as the item's message says.
    NeedsYou { message: String },
    /// Provisioning was asked for from this window and has not started yet.
    Starting,
    /// Awaiting provisioning and nothing runs: the Provision button, which
    /// also retries `failed` steps.
    Provision { failed: usize },
}

/// The badge for `member`, given the fleet's attention and whether this
/// window asked to provision it; none for the usual status.
pub(crate) fn badge(member: &MemberRow, attention: &[Item], requested: bool) -> Option<Badge> {
    let summary = member.provisioning.as_ref();
    if let Some(summary) = summary.filter(|summary| summary.state == ApplyState::Running) {
        let finished = summary.installed + summary.failed.len();
        return Some(Badge::Installing {
            step: (finished + 1).min(summary.steps.max(1)),
            of: summary.steps,
        });
    }
    if let Some(item) = needs_person(&member.name, attention).next() {
        return Some(Badge::NeedsYou {
            message: item.message.clone(),
        });
    }
    let awaiting = member.readiness == Readiness::AwaitingProvisioning;
    if requested && awaiting {
        return Some(Badge::Starting);
    }
    // Steps held for a person are not retried by asking again.
    awaiting.then(|| Badge::Provision {
        failed: summary.map_or(0, |summary| {
            summary
                .failed
                .iter()
                .filter(|step| {
                    !matches!(
                        step.code.as_str(),
                        "xcode_privileges_missing" | "xcode_not_on_master"
                    )
                })
                .count()
        }),
    })
}

/// The provisioning items about `member` whose way out starts with a person:
/// Xcode privileges on it, or an Apple ID on the master.
pub(crate) fn needs_person<'a>(
    member: &'a str,
    attention: &'a [Item],
) -> impl Iterator<Item = &'a Item> {
    attention.iter().filter(move |item| {
        item.member == member
            && matches!(
                item.category,
                Category::ProvisioningStepFailed | Category::XcodeAppleIdRequired
            )
            && item
                .actions
                .first()
                .is_some_and(|action| action.person_only)
    })
}

#[cfg(test)]
mod tests {
    use crate::{fleet_source::*, provisioning_badge::*};
    use elsecall_contract::{attention::Action, scheduling_model::timestamp::Timestamp};
    use elsecall_fleet::provisioning_model::{FailedStep, ProvisioningSummary};

    fn row(readiness: Readiness, provisioning: Option<ProvisioningSummary>) -> MemberRow {
        MemberRow {
            name: "macbook-pro".into(),
            this_member: false,
            role: Role::Member,
            reach: Reachability::Reachable,
            readiness,
            gate: Gate::Open,
            cpu: None,
            logical_cpus: None,
            memory: None,
            load: None,
            release: Release::Unknown { version: None },
            contributors: Default::default(),
            provisioning,
        }
    }

    fn summary(
        state: ApplyState,
        steps: usize,
        installed: usize,
        failed: usize,
    ) -> ProvisioningSummary {
        let at = Timestamp::parse_rfc3339("2026-11-20T09:00:00Z").unwrap();
        ProvisioningSummary {
            state,
            started_at: at,
            finished_at: None,
            steps,
            installed,
            failed: (0..failed)
                .map(|_| FailedStep {
                    manager: "mise".into(),
                    tool: "node".into(),
                    version: "22.19.0".into(),
                    stage: "install".into(),
                    code: "tool_install_failed".into(),
                    failed_at: at,
                })
                .collect(),
        }
    }

    fn item(category: Category, person_only: bool) -> Item {
        Item {
            id: Item::id(category, "macbook-pro", Some("xcodes:xcode@26.2")),
            member: "macbook-pro".into(),
            category,
            subject: Some("xcodes:xcode@26.2".into()),
            code: "xcode_privileges_missing".into(),
            message: "Installing Xcode 26.2 on macbook-pro failed.".into(),
            hint: "Install the privileges.".into(),
            since: None,
            actions: vec![Action {
                summary: "Install them.".into(),
                command: vec![
                    "elsecall".into(),
                    "tools".into(),
                    "privileges".into(),
                    "install".into(),
                ],
                run_on: "macbook-pro".into(),
                person_only,
            }],
        }
    }

    #[test]
    fn a_running_apply_shows_its_step() {
        let running = row(
            Readiness::AwaitingProvisioning,
            Some(summary(ApplyState::Running, 5, 1, 1)),
        );
        assert_eq!(
            badge(&running, &[], true),
            Some(Badge::Installing { step: 3, of: 5 })
        );
        let last = row(
            Readiness::Ready,
            Some(summary(ApplyState::Running, 2, 2, 0)),
        );
        assert_eq!(
            badge(&last, &[], false),
            Some(Badge::Installing { step: 2, of: 2 })
        );
    }

    #[test]
    fn a_person_step_is_needs_you_and_an_agents_retry_is_not() {
        let waiting = row(
            Readiness::AwaitingProvisioning,
            Some(summary(ApplyState::Finished, 3, 2, 1)),
        );
        assert!(matches!(
            badge(
                &waiting,
                &[item(Category::ProvisioningStepFailed, true)],
                false
            ),
            Some(Badge::NeedsYou { .. })
        ));
        assert_eq!(
            badge(
                &waiting,
                &[item(Category::ProvisioningStepFailed, false)],
                false
            ),
            Some(Badge::Provision { failed: 1 })
        );
        assert_eq!(
            badge(&waiting, &[item(Category::SecretConsent, true)], false),
            Some(Badge::Provision { failed: 1 }),
            "not a provisioning step"
        );
    }

    #[test]
    fn asked_for_then_ready() {
        let awaiting = row(Readiness::AwaitingProvisioning, None);
        assert_eq!(
            badge(&awaiting, &[], false),
            Some(Badge::Provision { failed: 0 })
        );
        assert_eq!(badge(&awaiting, &[], true), Some(Badge::Starting));
        let ready = row(
            Readiness::Ready,
            Some(summary(ApplyState::Finished, 3, 3, 0)),
        );
        assert_eq!(badge(&ready, &[], false), None, "the usual status");
    }
}
