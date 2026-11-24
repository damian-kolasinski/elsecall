//! Provisioning in the members table: the status cell's badge while a member's
//! tools are installed or wait for a person, the Provision button that starts
//! it, and the Setup button that opens a member's setup.
use crate::{
    app::AppState,
    fleet_source::{Gate, MemberRow, Reachability},
    provisioning_badge::{Badge, badge},
    theme,
};
use rune::{row, text};
use rune_kit::{Button, ButtonVariant, Cell, ControlSize, Kit, Spinner};

/// The status cell when provisioning has something to say; none for the
/// usual status.
pub(crate) fn status_cell(member: &MemberRow, state: &AppState, kit: &Kit) -> Option<Cell> {
    let attention = state.shown().map_or(&[][..], |fleet| &fleet.attention[..]);
    let requested = state.provisioning.requested(&member.name);
    let name = member.name.clone();
    let cell = match badge(member, attention, requested)? {
        Badge::Installing { step, of } => row()
            .center_y()
            .gap(theme::SPACE_2)
            .child(Spinner::new().size(12.0).build(kit))
            .child(
                text(format!("Installing {step} of {of}"))
                    .font(theme::scale().label)
                    .color(kit.c.text_base),
            )
            .label(format!("Installing step {step} of {of} on {}", member.name))
            .help("Installing tools")
            .help_row(format!(
                "The master is installing the tools the fleet's work pins on this Mac: step {step} of {of}, each as admitted work there."
            ))
            .key(format!("installing-{}", member.name)),
        Badge::NeedsYou { message } => Button::new("Needs you")
            .variant(ButtonVariant::Warning)
            .control_size(ControlSize::Small)
            .on_press(move |state: &mut AppState| state.provisioning.open_setup(&name))
            .build(kit)
            .px(theme::SPACE_2)
            .description(message.clone())
            .help("A step waits for you")
            .help_row(message)
            .help_row("Open it to see what to do and where.")
            .key(format!("needs-you-{}", member.name)),
        Badge::InstallFailed { message } => Button::new("Install failed")
            .variant(ButtonVariant::Warning)
            .control_size(ControlSize::Small)
            .on_press(move |state: &mut AppState| state.provisioning.open_setup(&name))
            .build(kit)
            .px(theme::SPACE_2)
            .description(message.clone())
            .help("Tool installation failed")
            .help_row(message)
            .help_row("Open Setup to inspect the failed step and recent placement decisions.")
            .key(format!("install-failed-{}", member.name)),
        Badge::Starting => Button::new("Starting…")
            .variant(ButtonVariant::Loading)
            .control_size(ControlSize::Small)
            .disabled(true)
            .build(kit)
            .px(theme::SPACE_2)
            .help("Provisioning")
            .help_row("The master is planning this Mac's tools; installing starts next.")
            .key(format!("provision-{}", member.name)),
        Badge::Provision { failed } => {
            const EXPLANATION: &str =
                "Automatic job placement skips this Mac until provisioning is complete.";
            let connection = match (&member.reach, &member.gate) {
                (Reachability::Unreachable, _) => "This Mac is offline.".to_owned(),
                (Reachability::Reachable, Gate::Blocked { leading_process }) => {
                    match leading_process {
                        Some(process) => {
                            format!("Online. Outside work by {process} holds admission.")
                        }
                        None => "Online. Outside work holds admission.".to_owned(),
                    }
                }
                (Reachability::Reachable, Gate::Open) => "This Mac is online.".to_owned(),
            };
            let retry = (failed > 0).then(|| {
                format!(
                    "{failed} step{} failed last time; provisioning tries {} again.",
                    if failed == 1 { "" } else { "s" },
                    if failed == 1 { "it" } else { "them" }
                )
            });
            // Only the master provisions; another member's window says where.
            let master = state.shown().and_then(|fleet| {
                fleet
                    .members
                    .iter()
                    .find(|row| row.role == crate::fleet_source::Role::Master)
                    .map(|row| (row.name.clone(), row.this_member))
            });
            let here = master.as_ref().is_some_and(|(_, this)| *this);
            let mut button = Button::new("Provision")
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Small)
                .disabled(member.reach == Reachability::Unreachable || !here)
                .on_press(move |state: &mut AppState| {
                    let now = state.clock();
                    let fleet = state.shown().cloned();
                    state.provisioning.provision(&name, now, fleet.as_ref());
                })
                .build(kit)
                .px(theme::SPACE_2)
                .description(EXPLANATION)
                .help("Awaiting provisioning")
                .help_row(EXPLANATION)
                .help_row("Provision installs what the fleet's work pins now; the master also does it by itself.")
                .help_row("You can still target this Mac by name.")
                .help_row(connection);
            if !here && let Some((master, _)) = &master {
                button = button.help_row(format!(
                    "Only the master provisions: open Elsecall on {master} to start it now."
                ));
            }
            if let Some(retry) = retry {
                button = button.help_row(retry);
            }
            if let Some(why) = state.provisioning.refusal(&member.name) {
                button = button.help_row(format!("Asking failed: {why}"));
            }
            button.key(format!("provision-{}", member.name))
        }
    };
    Some(Cell::el(cell))
}

/// The member cell's Setup button, beside Load details.
pub(crate) fn setup_button(member: &MemberRow, kit: &Kit) -> rune::El {
    let name = member.name.clone();
    Button::new("Setup")
        .variant(ButtonVariant::Ghost)
        .control_size(ControlSize::Mini)
        .on_press(move |state: &mut AppState| state.provisioning.open_setup(&name))
        .build(kit)
        .key(format!("setup:{}", member.name))
        .label(format!("Setup of {}", member.name))
}
