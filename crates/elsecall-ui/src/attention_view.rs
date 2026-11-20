//! What in the fleet needs attention, as plain items: each one's message, its
//! hint and the commands that resolve it, the ones only a person can run
//! marked as theirs. Nothing here runs them: an agent reads the same items
//! from `elsecall fleet`, and a person runs theirs where the line says.
//!
//! A secret consent item keeps its "from now on" choice as a checkbox, which
//! picks which of its two commands to show; both are a person's.
use crate::{app::AppState, setup_dialog::command_line, theme};
use elsecall_contract::attention::{Category, Item};
use rune::{El, ScrollSpec, col, row, text};
use rune_kit::{Checkbox, Kit};

/// The list, or none when nothing needs attention.
pub(crate) fn card(state: &AppState, kit: &Kit) -> Option<El> {
    let items = &state.shown()?.attention;
    if items.is_empty() {
        return None;
    }
    let mut list = col().w_full().gap(theme::SPACE_3).padding(theme::SPACE_3);
    for item in items {
        list = list.child(entry(item, state, kit));
    }
    let heading = text(match items.len() {
        1 => "1 item needs attention".to_owned(),
        count => format!("{count} items need attention"),
    })
    .font(theme::scale().h5)
    .color(kit.c.text_base);
    Some(
        col()
            .w_full()
            .gap(theme::SPACE_1)
            .child(heading)
            .child(
                col()
                    .w_full()
                    .max_h(176.0)
                    .fill(kit.c.layer_01)
                    .radius(theme::RADIUS_SMALL)
                    .border(1.0, kit.c.border_base)
                    .scroll(ScrollSpec {
                        stick_bottom: false,
                    })
                    .child(list),
            )
            .key("attention"),
    )
}

fn entry(item: &Item, state: &AppState, kit: &Kit) -> El {
    let mut entry = col()
        .w_full()
        .gap(2.0)
        .child(
            text(item.message.clone())
                .wrap()
                .w_full()
                .font(theme::scale().body)
                .color(kit.c.text_base),
        )
        .child(
            text(item.hint.clone())
                .wrap()
                .w_full()
                .font(theme::scale().small)
                .color(kit.c.text_muted),
        );
    if item.category == Category::SecretConsent && item.actions.len() == 2 {
        let id = item.id.clone();
        let always = state.provisioning.always(&item.id);
        let action = &item.actions[usize::from(always)];
        entry = entry
            .child(
                Checkbox::new(always)
                    .label("Sync changes to these automatically from now on")
                    .on_toggle(move |state: &mut AppState| state.provisioning.toggle_always(&id))
                    .build(kit)
                    .key(format!("always:{}", item.id)),
            )
            .child(yours(&action.run_on, &action.command, kit));
    } else {
        for action in &item.actions {
            entry = entry.child(if action.person_only {
                yours(&action.run_on, &action.command, kit)
            } else {
                command_line(&action.run_on, &action.command, kit)
            });
        }
    }
    entry.key(format!("attention:{}", item.id))
}

/// A command only a person can run, marked as theirs.
fn yours(run_on: &str, command: &[String], kit: &Kit) -> El {
    row()
        .w_full()
        .gap(theme::SPACE_2)
        .child(
            text("For you")
                .font(theme::scale().label)
                .color(kit.c.warning.fg),
        )
        .child(command_line(run_on, command, kit).grow(1.0))
}
