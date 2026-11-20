//! Progress steps and the actions the operator can take while Add Mac runs.

use super::illustrations::{browser_art, firewall_art};
use super::{CHECK, Look, paragraph};
use crate::add_mac_flow::{Action, STEPS, StepState, step_label, step_state};
use crate::app::AppState;
use crate::theme;
use elsecall_fleet::add_mac::{Approval, Step};
use elsecall_fleet::remote_mac::LoginLink;
use rune::{El, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, Kit, Spinner};

pub(super) fn adding(
    name: &str,
    current: Option<Step>,
    action: Option<&Action>,
    clock: f64,
    k: &Kit,
    l: &Look,
) -> El {
    let mut steps = col().w(250.0).gap(theme::SPACE_2).key("add-mac-steps");
    for step in STEPS {
        let state = step_state(step, current);
        let mark = match state {
            StepState::Done => icon(&CHECK, 16.0).color(l.good),
            StepState::Running => Spinner::new().size(14.0).build(k),
            StepState::Pending => rune::el().size(6.0, 6.0).radius(3.0).fill(l.border),
        };
        steps = steps.child(
            row()
                .h(22.0)
                .gap(theme::SPACE_2)
                .center_y()
                .child(row().size(16.0, 16.0).center().child(mark))
                .child(text(step_label(step)).font(l.body).color(match state {
                    StepState::Done => l.secondary,
                    StepState::Running => l.text,
                    StepState::Pending => l.subtle,
                })),
        );
    }
    let side = match action {
        Some(Action::Tailnet { link, approval }) => approve(name, link, *approval, clock, k, l),
        Some(Action::Firewall) => firewall(name, true, l),
        None if matches!(current, Some(Step::Firewall | Step::Verify)) => firewall(name, false, l),
        None if current.is_none() => col()
            .grow(1.0)
            .child(paragraph(&format!("Connecting to {name} over SSH…"), l)),
        None => col()
            .grow(1.0)
            .gap(theme::SPACE_3)
            .child(paragraph(
                &format!("Elsecall is setting up {name} over SSH."),
                l,
            ))
            .child(paragraph(
                if current == Some(Step::Install) {
                    "Keep the new Mac unlocked. If macOS asks to allow incoming connections for Elsecall or tailscaled, click Allow there."
                } else if current == Some(Step::Tools) {
                    "If Git is missing, Elsecall installs Apple's Command Line Tools. The download may take several minutes."
                } else if current == Some(Step::Configuration) {
                    "Elsecall is applying the fleet's configuration. The new Mac may restart its Elsecall service."
                } else {
                    "If Tailscale needs your approval, its page opens in your browser and this sheet shows what to click."
                },
                l,
            )),
    };
    row().w_full().gap(theme::SPACE_6).child(steps).child(side)
}

fn firewall(name: &str, waiting: bool, l: &Look) -> El {
    let mut help = col()
        .grow(1.0)
        .gap(theme::SPACE_3)
        .child(firewall_art(l))
        .child(paragraph(
            &format!("If {name} asks to allow incoming connections for Elsecall or tailscaled, click Allow on that Mac."),
            l,
        ));
    if waiting {
        help = help.child(paragraph(
            "Add Mac will continue when the firewall allows the connection. If you see no prompt, check System Settings › Network › Firewall › Options on the new Mac.",
            l,
        ));
    }
    help.key("add-mac-firewall-help")
}

fn approve(name: &str, link: &LoginLink, approval: Approval, clock: f64, k: &Kit, l: &Look) -> El {
    let (lead, button) = match approval {
        Approval::Login => (
            format!(
                "Approve {name} in your browser: Tailscale's page is open there. Click Connect to let it join your tailnet."
            ),
            "Connect",
        ),
        Approval::Device => (
            format!(
                "Your tailnet approves new devices: in the admin console that opened, approve {name}."
            ),
            "Approve",
        ),
    };
    let link = link.as_str().to_owned();
    col()
        .grow(1.0)
        .gap(theme::SPACE_3)
        .child(browser_art(name, button, clock, l))
        .child(text(lead).wrap().w_full().font(l.body).color(l.text))
        .child(
            row().child(
                Button::new("Open the page again")
                    .variant(ButtonVariant::Outline)
                    .on_press_tx(move |_: &mut AppState, tx| {
                        let _ = tx.open_url(&link);
                    })
                    .build(k)
                    .key("add-mac-open-link"),
            ),
        )
        .key("add-mac-approve")
}
