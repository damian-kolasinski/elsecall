//! A member's setup, next to its load details: what waits for a person, what
//! its plan still installs, and what its latest apply did, failures with
//! their hints. It reads the plan and the latest apply when it opens.
use crate::{
    app::AppState,
    fleet_source::{MemberRow, Role},
    provisioning_badge::needs_person,
    provisioning_source::{Applied, AppliedState, Apply, Plan, Setup},
    theme,
};
use elsecall_contract::attention::{Category, Item};
use elsecall_fleet::provisioning_model::ApplyState;
use elsecall_peers::roster::Readiness;
use rune::{El, ScrollSpec, col, el, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, Dialog, Kit, Spinner};

pub(crate) fn overlay(state: &AppState, kit: &Kit, layers: &mut Vec<El>) {
    let Some(member) = state
        .provisioning
        .setup
        .as_ref()
        .and_then(|name| state.rows().iter().find(|m| &m.name == name))
    else {
        return;
    };
    let attention = state.shown().map_or(&[][..], |fleet| &fleet.attention[..]);
    let details = &state.provisioning.details;
    let mut body = col()
        .w_full()
        .gap(theme::SPACE_4)
        .padding(theme::SPACE_3)
        .child(note(&standing(member, state), kit));
    if let Some(history) = &member.pin_exclusions {
        let count = history.decisions;
        let decisions = if count == 1 { "decision" } else { "decisions" };
        let limit = elsecall_fleet::fleet_view::TOOL_HISTORY_LIMIT;
        let last = history.last_at.to_string();
        let date = &last[..10];
        let time = &last[11..16];
        body = body.child(
            section("Placement history", kit.c.info.icon, kit)
                .key("setup-placement-history")
                .child(note(&format!(
                    "Missing tools excluded this Mac from {count} placement {decisions} in the latest {limit} requests on this machine."
                ), kit))
                .child(note(&format!("Last skipped {date} at {time} UTC · {}", history.last_reason), kit)),
        );
    }
    let person = person_steps(member, attention, details);
    if !person.is_empty() {
        let mut steps = section("For you", kit.c.warning.icon, kit);
        for item in person {
            steps = steps.child(person_step(item, kit));
        }
        body = body.child(steps);
    }
    // What went wrong comes before what is still to install.
    body = body
        .child(apply_section(
            details.apply.as_ref(),
            member,
            attention,
            kit,
        ))
        .child(plan_section(details.plan.as_ref(), kit));
    let scroll = col()
        .key("setup-details")
        .focusable(true)
        .label("Setup")
        .focus_return(format!("setup:{}", member.name))
        .w_full()
        .h(300.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(body);
    let mut buttons = row()
        .w_full()
        .gap(theme::SPACE_2)
        .justify_end()
        .padding(theme::SPACE_3)
        .child(
            Button::new("Read again")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .disabled(details.plan.is_none() || details.apply.is_none())
                .on_press(|state: &mut AppState| state.provisioning.reload())
                .build(kit)
                .key("setup-reload"),
        );
    // Only the master provisions, and never itself.
    let here = state.shown().is_some_and(|fleet| fleet.this_is_master());
    if member.role != Role::Master && here {
        let name = member.name.clone();
        buttons = buttons.child(
            Button::new("Provision now")
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Small)
                .disabled(state.provisioning.requested(&member.name))
                .on_press(move |state: &mut AppState| {
                    let now = state.clock();
                    let fleet = state.shown().cloned();
                    state.provisioning.provision(&name, now, fleet.as_ref());
                })
                .build(kit)
                .key("setup-provision"),
        );
    }
    Dialog::new("member-setup", format!("Setup · {}", member.name))
        .description(match (member.role, member.readiness) {
            (Role::Master, _) => "The master",
            (_, Readiness::AwaitingProvisioning) => "Awaiting provisioning",
            (_, Readiness::Ready) => "Ready",
        })
        .width(760.0)
        .fit()
        .open(true)
        .on_close(|state: &mut AppState| state.provisioning.close_setup())
        .child(scroll)
        .child(buttons)
        .build(kit, layers);
}

/// The member's standing, in a sentence or two.
fn standing(member: &MemberRow, state: &AppState) -> String {
    let standing = match (member.role, member.readiness) {
        (Role::Master, _) => {
            "The master: every member is kept in step with the tools it has.".to_owned()
        }
        (_, Readiness::AwaitingProvisioning) => {
            "Awaiting provisioning: placement gives it no work until its tools are installed, which the master does by itself.".to_owned()
        }
        (_, Readiness::Ready) => "Ready: placement may give it work.".to_owned(),
    };
    let running = member
        .provisioning
        .as_ref()
        .filter(|summary| summary.state == ApplyState::Running)
        .map(|summary| {
            format!(
                " Installing now: {} of {} done.",
                summary.installed + summary.failed.len(),
                summary.steps
            )
        })
        .unwrap_or_default();
    let refused = state
        .provisioning
        .refused
        .as_ref()
        .filter(|(name, _)| name == &member.name)
        .map(|(_, why)| format!(" Provisioning was refused: {why}"))
        .unwrap_or_default();
    format!("{standing}{running}{refused}")
}

/// What waits for a person about this member: its own provisioning items
/// that start with a person, and an Xcode only a person can get the master,
/// when this member's apply needed it.
fn person_steps<'a>(member: &'a MemberRow, attention: &'a [Item], setup: &Setup) -> Vec<&'a Item> {
    let needs_xcode: Vec<String> = setup
        .apply
        .as_ref()
        .and_then(|apply| apply.as_ref().ok())
        .and_then(Option::as_ref)
        .into_iter()
        .flat_map(|apply| &apply.steps)
        .filter(|step| {
            matches!(&step.state, AppliedState::Failed { code, .. } if code == "xcode_not_on_master")
        })
        .filter_map(|step| step.tool.version.clone())
        .collect();
    needs_person(&member.name, attention)
        .chain(attention.iter().filter(|item| {
            item.category == Category::XcodeAppleIdRequired
                && item.member != member.name
                && item
                    .subject
                    .as_ref()
                    .is_some_and(|version| needs_xcode.contains(version))
        }))
        .collect()
}

fn person_step(item: &Item, kit: &Kit) -> El {
    let mut step = col()
        .w_full()
        .gap(4.0)
        .border_sides([1.0, 0.0, 0.0, 0.0], kit.c.border_base)
        .pt(6.0)
        .child(
            text(item.message.clone())
                .wrap()
                .font(theme::scale().h5)
                .color(kit.c.text_base),
        )
        .child(
            text(item.hint.clone())
                .wrap()
                .font(theme::scale().small)
                .color(kit.c.text_muted),
        );
    for action in item.actions.iter().filter(|action| action.person_only) {
        step = step.child(command_line(&action.run_on, &action.command, kit));
    }
    step.key(format!("person-step:{}", item.id))
}

/// `On mac-mini: elsecall tools apply`, for a person to run there.
pub(crate) fn command_line(run_on: &str, command: &[String], kit: &Kit) -> El {
    row()
        .w_full()
        .gap(theme::SPACE_2)
        .child(
            text(format!("On {run_on}:"))
                .font(theme::scale().small)
                .color(kit.c.text_muted),
        )
        .child(
            text(command.join(" "))
                .wrap()
                .grow(1.0)
                .min_w(0.0)
                .font(theme::scale().mono)
                .color(kit.c.text_base),
        )
}

fn plan_section(plan: Option<&Result<Plan, String>>, kit: &Kit) -> El {
    let section = section("To install", kit.c.info.icon, kit);
    match plan {
        None => section.child(reading("Reading the plan…", kit)),
        Some(Err(why)) => section.child(note(why, kit)),
        Some(Ok(plan)) if plan.steps.is_empty() && plan.unresolved.is_empty() && !plan.partial => {
            section.child(note(
                "Nothing: everything the fleet's pins ask of it is installed.",
                kit,
            ))
        }
        Some(Ok(plan)) => {
            let mut list = section;
            if plan.partial {
                list = list.child(note(
                    "Some pins or tools could not be read, so this may not be all: the member stays awaiting provisioning until they are.",
                    kit,
                ));
            }
            for step in &plan.steps {
                list = list.child(line(&step.label(), "To install", kit));
            }
            for unresolved in &plan.unresolved {
                list = list.child(
                    col()
                        .w_full()
                        .gap(2.0)
                        .child(line(&unresolved.tool.label(), "Not installable yet", kit))
                        .child(note(&unresolved.hint, kit)),
                );
            }
            list
        }
    }
}

fn apply_section(
    apply: Option<&Result<Option<Apply>, String>>,
    member: &MemberRow,
    attention: &[Item],
    kit: &Kit,
) -> El {
    let section = section("Latest apply", kit.c.success.icon, kit);
    match apply {
        None => section.child(reading("Reading what it installed…", kit)),
        Some(Err(why)) => section.child(note(why, kit)),
        Some(Ok(None)) => section.child(note("Nothing applied yet.", kit)),
        Some(Ok(Some(apply))) if apply.steps.is_empty() => section.child(note(
            if apply.finished {
                "The last apply had nothing to install."
            } else {
                "Starting…"
            },
            kit,
        )),
        Some(Ok(Some(apply))) => {
            let mut list = section;
            for step in &apply.steps {
                list = list.child(applied(step, member, attention, kit));
            }
            list
        }
    }
}

fn applied(step: &Applied, member: &MemberRow, attention: &[Item], kit: &Kit) -> El {
    let label = step.tool.label();
    match &step.state {
        AppliedState::Queued => line(&label, "Queued", kit),
        AppliedState::Running => row()
            .w_full()
            .gap(theme::SPACE_2)
            .center_y()
            .child(Spinner::new().size(12.0).build(kit))
            .child(line(&label, "Installing", kit).grow(1.0)),
        AppliedState::Installed => line(&label, "Installed", kit),
        AppliedState::Skipped => line(&label, "Skipped: not installed this way yet", kit),
        AppliedState::Failed { stage, code } => {
            let subject = format!(
                "{}:{}@{}",
                step.tool.manager,
                step.tool.tool,
                step.tool.version.as_deref().unwrap_or_default()
            );
            let hint = attention
                .iter()
                .find(|item| {
                    item.member == member.name
                        && item.category == Category::ProvisioningStepFailed
                        && item.subject.as_deref() == Some(subject.as_str())
                })
                .map(|item| item.hint.clone());
            let mut failed = col().w_full().gap(2.0).child(line(
                &label,
                &format!("Failed at {stage}: {code}"),
                kit,
            ));
            if let Some(hint) = hint {
                failed = failed.child(note(&hint, kit));
            }
            failed
        }
    }
}

fn section(name: &str, accent: rune::Color, kit: &Kit) -> El {
    col().w_full().gap(8.0).child(
        row()
            .gap(8.0)
            .center_y()
            .child(el().w(4.0).h(16.0).radius(2.0).fill(accent))
            .child(text(name).font(theme::scale().h4).color(kit.c.text_base)),
    )
}

fn line(label: &str, state: &str, kit: &Kit) -> El {
    row()
        .w_full()
        .gap(theme::SPACE_3)
        .border_sides([1.0, 0.0, 0.0, 0.0], kit.c.border_base)
        .pt(5.0)
        .child(
            text(label.to_owned())
                .grow(1.0)
                .min_w(0.0)
                .font(theme::scale().h5)
                .color(kit.c.text_base),
        )
        .child(
            text(state.to_owned())
                .font(theme::scale().small)
                .color(kit.c.text_muted),
        )
}

fn note(message: &str, kit: &Kit) -> El {
    text(message.to_owned())
        .wrap()
        .w_full()
        .font(theme::scale().small)
        .color(kit.c.text_muted)
}

fn reading(message: &str, kit: &Kit) -> El {
    row()
        .gap(theme::SPACE_2)
        .center_y()
        .child(Spinner::new().size(12.0).build(kit))
        .child(note(message, kit))
}
