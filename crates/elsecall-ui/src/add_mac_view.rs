//! The Add Mac sheet: what to do on the new Mac, which Mac, its account, and the
//! add as it happens, with looping illustrations of the operator's steps.

use elsecall_fleet::add_mac::Approval;
use rune::tok::TextStyle;
use rune::{Color, El, Icon, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, Dialog, Kit, Spinner, TextInput};

mod illustrations;

use self::illustrations::{browser_art, settings_art, wifi_art};
use crate::add_mac_flow::{
    MacStatus, Nearby, NearbyMacExt, Page, STEPS, StepState, Target, needs_new_target, step_label,
    step_state,
};
use crate::app::AppState;
use crate::theme;
use elsecall_contract::failure::Failure;
use elsecall_fleet::add_mac::AddReport;
use elsecall_fleet::nearby_macs::NearbyMac;
use elsecall_fleet::remote_mac::LoginLink;
use elsecall_peers::roster::Readiness;

const SHEET_WIDTH: f32 = 680.0;
static CHECK: Icon = Icon {
    name: "elsecall-check",
    view: 16.0,
    body: r#"<path d="M3.5 8.5L6.5 11.5L12.5 4.5" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};
static MAC: Icon = Icon {
    name: "elsecall-mac",
    view: 16.0,
    body: r#"<rect x="1.5" y="2.5" width="13" height="8.5" rx="1.2" stroke="currentColor" stroke-width="1.3" fill="none"/><path d="M5.5 14H10.5M8 11V14" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/>"#,
};

#[derive(Clone, Copy)]
struct Look {
    text: Color,
    secondary: Color,
    subtle: Color,
    card: Color,
    raised: Color,
    border: Color,
    accent: Color,
    accent_ink: Color,
    good: Color,
    good_fill: Color,
    warn_fill: Color,
    warn_ink: Color,
    track: Color,
    knob: Color,
    title: TextStyle,
    body: TextStyle,
    small: TextStyle,
    label: TextStyle,
    mono: TextStyle,
}

fn look(k: &Kit) -> Look {
    Look {
        text: k.c.text_base,
        secondary: k.c.text_muted,
        subtle: k.c.text_faint,
        card: k.c.layer_01,
        raised: k.c.layer_02,
        border: k.c.border_base,
        accent: k.c.info.icon,
        accent_ink: k.c.info.fg,
        good: k.c.success.icon,
        good_fill: k.c.success.bg,
        warn_fill: k.c.warning.bg,
        warn_ink: k.c.warning.fg,
        track: k.c.border_base,
        knob: Color::WHITE,
        title: theme::scale().h3,
        body: theme::scale().body,
        small: theme::scale().small,
        label: theme::scale().label,
        mono: theme::scale().mono,
    }
}

/// Adds the sheet to `layer` while it is open.
pub(crate) fn sheet(state: &AppState, k: &Kit, reduced_motion: bool, layer: &mut Vec<El>) {
    let flow = state.add_mac();
    if !flow.is_open() {
        return;
    }
    let l = look(k);
    // Under Reduce Motion the illustrations hold their last frame.
    let clock = if reduced_motion {
        f64::MAX
    } else {
        state.clock()
    };
    let (title, body, buttons) = match &flow.page {
        Page::Closed => return,
        Page::Prepare => ("Add a Mac", prepare(clock, &l), prepare_buttons(k)),
        Page::Network => (
            "Connect to the same network",
            network(clock, &l),
            network_buttons(k),
        ),
        Page::Choose { selected } => (
            "Choose the new Mac",
            choose(state, selected.as_deref(), k, &l),
            choose_buttons(state, k),
        ),
        Page::SignIn { target } => (
            "Sign in to the new Mac",
            sign_in(state, target, k, &l),
            sign_in_buttons(state, k),
        ),
        Page::Adding {
            target,
            current,
            approval,
        } => (
            "Adding the Mac",
            adding(&target.name, *current, approval.as_ref(), clock, k, &l),
            adding_buttons(k),
        ),
        Page::Done { report } => (
            "The Mac joined the fleet",
            done(report, &l),
            done_buttons(k),
        ),
        Page::Failed { failure, .. } => (
            "The Mac was not added",
            failed(failure, flow.has_credentials(), k, &l),
            failed_buttons(failure, flow.has_credentials(), k),
        ),
    };
    let content = col()
        .w_full()
        .padding(theme::SPACE_6)
        .gap(theme::SPACE_6)
        .child(stepper(&flow.page, &l))
        .child(body)
        .child(
            row()
                .w_full()
                .justify_end()
                .gap(theme::SPACE_2)
                .children(buttons),
        );
    Dialog::new("add-mac", title)
        .width(SHEET_WIDTH)
        .fit()
        .click_outside(false)
        .open(true)
        .on_close(|state: &mut AppState| state.close_add_mac())
        .child(content)
        .build(k, layer);
}

/// The current stage of the sheet.
fn stepper(page: &Page, l: &Look) -> El {
    let at = match page {
        Page::Closed | Page::Prepare => 0,
        Page::Network => 1,
        Page::Choose { .. } => 2,
        Page::SignIn { .. } => 3,
        Page::Adding { .. } | Page::Done { .. } | Page::Failed { .. } => 4,
    };
    let mut line = row().center_y().gap(theme::SPACE_2);
    for (index, label) in [
        "On the new Mac",
        "Connect to Wi-Fi",
        "Choose it",
        "Sign in",
        "Add",
    ]
    .into_iter()
    .enumerate()
    {
        if index > 0 {
            line = line.child(rune::el().size(18.0, 1.0).fill(l.border));
        }
        let (dot, ink) = if index < at {
            (l.good, l.secondary)
        } else if index == at {
            (l.accent, l.text)
        } else {
            (l.border, l.subtle)
        };
        line = line.child(
            row()
                .center_y()
                .gap(theme::SPACE_1)
                .child(rune::el().size(8.0, 8.0).radius(4.0).fill(dot))
                .child(text(label).font(l.small).color(ink)),
        );
    }
    line.key("add-mac-stepper")
}

fn paragraph(line: &str, l: &Look) -> El {
    text(line).wrap().w_full().font(l.body).color(l.secondary)
}

fn numbered(number: usize, line: El, l: &Look) -> El {
    row()
        .w_full()
        .gap(theme::SPACE_2)
        .child(
            col()
                .size(20.0, 20.0)
                .radius(10.0)
                .fill(l.raised)
                .center()
                .child(text(number.to_string()).font(l.label).color(l.text)),
        )
        .child(col().grow(1.0).child(line))
}

fn prepare(clock: f64, l: &Look) -> El {
    let steps = col()
        .grow(1.0)
        .gap(theme::SPACE_3)
        .child(numbered(
            1,
            paragraph("On the new Mac, open System Settings, then General › Sharing.", l),
            l,
        ))
        .child(numbered(
            2,
            paragraph(
                "Turn on Remote Login. Its \"Allow access for\" must include the account you will sign in with (all users, the default, does).",
                l,
            ),
            l,
        ));
    row()
        .w_full()
        .gap(theme::SPACE_6)
        .child(settings_art(clock, l))
        .child(steps)
}

fn network(clock: f64, l: &Look) -> El {
    let steps = col()
        .grow(1.0)
        .gap(theme::SPACE_3)
        .child(numbered(
            1,
            paragraph("Check the Wi-Fi network this Mac is using.", l),
            l,
        ))
        .child(numbered(
            2,
            paragraph(
                "On the new Mac, open System Settings › Wi-Fi and join that same network.",
                l,
            ),
            l,
        ))
        .child(paragraph(
            "Ethernet also works when both Macs are on the same local network.",
            l,
        ));
    row()
        .w_full()
        .gap(theme::SPACE_6)
        .child(wifi_art(clock, l))
        .child(steps)
}

fn choose(state: &AppState, selected: Option<&str>, k: &Kit, l: &Look) -> El {
    let flow = state.add_mac();
    let typing = !flow.manual.text.trim().is_empty();
    let mut list = col().w_full().gap(theme::SPACE_1).key("add-mac-nearby");
    let mut trouble = None;
    match &flow.nearby {
        Nearby::Looking => {
            list = list.child(
                row()
                    .gap(theme::SPACE_2)
                    .center_y()
                    .child(Spinner::new().size(14.0).build(k))
                    .child(
                        text("Looking for Macs on this network…")
                            .font(l.body)
                            .color(l.secondary),
                    ),
            );
        }
        Nearby::Failed(failure) => trouble = Some(failure),
        Nearby::Found { macs, trouble: now } => {
            trouble = now.as_ref();
            if macs.is_empty() {
                list = list.child(paragraph(
                    "No new Macs found yet. Check the new Mac's network and Remote Login, or type its address below.",
                    l,
                ));
            }
            for mac in macs {
                let picked = !typing && selected == Some(mac.host.as_str());
                list = list.child(mac_row(mac, picked, l));
            }
        }
    }
    let mut page = col().w_full().gap(theme::SPACE_4).child(paragraph(
        "Macs on this network. Pick the new one; this list keeps looking while it is open.",
        l,
    ));
    if let Some(failure) = trouble {
        page = page.child(problem(failure, k, l));
    }
    page.child(list).child(field(
        "Not listed? Its host name or address",
        TextInput::new(&flow.manual)
            .label("Host name or address")
            .placeholder("macbook-pro.local or 192.168.1.20")
            .on_edit(|state: &mut AppState| &mut state.add_mac_mut().manual)
            .build(k)
            .key("add-mac-manual"),
        l,
    ))
}

/// Why looking for Macs failed, with what to do; for Local Network privacy, the
/// way to the setting.
fn problem(failure: &Failure, k: &Kit, l: &Look) -> El {
    let mut card = col()
        .w_full()
        .gap(theme::SPACE_1)
        .padding(theme::SPACE_3)
        .radius(theme::RADIUS_SMALL)
        .fill(l.warn_fill)
        .child(
            text(failure.message.clone())
                .wrap()
                .w_full()
                .font(l.body)
                .color(l.text),
        )
        .child(
            text(failure.hint.clone())
                .wrap()
                .w_full()
                .font(l.small)
                .color(l.secondary),
        )
        .key("add-mac-problem");
    if failure.code == "local_network_denied" {
        card = card.child(
            row().child(
                Button::new("Open Local Network settings")
                    .variant(ButtonVariant::Outline)
                    .on_press(|_: &mut AppState| open_local_network_settings())
                    .build(k)
                    .key("add-mac-local-network"),
            ),
        );
    }
    card
}

/// System Settings › Privacy & Security › Local Network.
fn open_local_network_settings() {
    let _ = std::process::Command::new("/usr/bin/open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_LocalNetwork")
        .spawn();
}

fn mac_row(mac: &NearbyMac, selected: bool, l: &Look) -> El {
    let status = mac.status();
    let (label, fill, ink) = match &status {
        MacStatus::Ready => ("Ready to add", l.good_fill, l.good),
        MacStatus::RemoteLoginOff => ("Turn on Remote Login", l.warn_fill, l.warn_ink),
        MacStatus::Unknown => ("Not sure it is a Mac", l.raised, l.secondary),
        MacStatus::PossibleMember => ("May be a member", l.raised, l.secondary),
    };
    let detail = describe(mac.model.as_deref(), &mac.host, mac.addresses.first());
    let mut line = row()
        .w_full()
        .h(48.0)
        .px(theme::SPACE_3)
        .gap(theme::SPACE_3)
        .center_y()
        .radius(theme::RADIUS_SMALL)
        .fill(if selected {
            l.accent.alpha(0.14)
        } else {
            l.card
        })
        .border(1.0, if selected { l.accent } else { l.border })
        .child(icon(&MAC, 20.0).color(if status.selectable() {
            l.text
        } else {
            l.subtle
        }))
        .child(
            col()
                .grow(1.0)
                .gap(2.0)
                .child(text(mac.name.clone()).font(l.body).color(l.text))
                .child(text(detail).font(l.small).color(l.subtle)),
        )
        .child(
            row()
                .h(22.0)
                .px(theme::SPACE_2)
                .center_y()
                .radius(theme::RADIUS_PILL)
                .fill(fill)
                .child(text(label).font(l.label).color(ink)),
        )
        .key(format!("add-mac-host-{}", mac.host));
    if !status.selectable() {
        line = line.opacity(0.6);
    } else {
        let host = mac.host.clone();
        line = line.on_press(move |state: &mut AppState| state.add_mac_mut().select(&host));
    }
    line
}

/// A Mac as the operator can check it: model, host name and address.
fn describe(model: Option<&str>, host: &str, address: Option<&std::net::IpAddr>) -> String {
    let address = address.map(ToString::to_string);
    [model, Some(host), address.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")
}

fn field(caption: &str, input: El, l: &Look) -> El {
    col()
        .gap(theme::SPACE_1)
        .child(text(caption).font(l.label).color(l.secondary))
        .child(input)
}

fn sign_in(state: &AppState, target: &Target, k: &Kit, l: &Look) -> El {
    let flow = state.add_mac();
    let address = flow
        .found()
        .iter()
        .find(|mac| mac.host == target.host)
        .and_then(|mac| mac.addresses.first());
    col()
        .w_full()
        .gap(theme::SPACE_4)
        .child(
            row()
                .w_full()
                .gap(theme::SPACE_3)
                .center_y()
                .padding(theme::SPACE_3)
                .radius(theme::RADIUS_SMALL)
                .fill(l.card)
                .border(1.0, l.border)
                .child(icon(&MAC, 20.0).color(l.text))
                .child(
                    col()
                        .gap(2.0)
                        .child(text(target.name.clone()).font(l.body).color(l.text))
                        .child(
                            text(describe(target.model.as_deref(), &target.host, address))
                                .font(l.small)
                                .color(l.subtle),
                        ),
                )
                .key("add-mac-target"),
        )
        .child(paragraph(
            "Use an administrator account on that Mac. Elsecall keeps the password in memory for setup and retries.",
            l,
        ))
        .child(field(
            "Account",
            TextInput::new(&flow.account)
                .label("Account")
                .placeholder("Its short name, such as devbox")
                .on_edit(|state: &mut AppState| &mut state.add_mac_mut().account)
                .build(k)
                .key("add-mac-account"),
            l,
        ))
        .child(field(
            "Password",
            TextInput::new(&flow.password)
                .label("Password")
                .secure()
                .on_edit(|state: &mut AppState| &mut state.add_mac_mut().password)
                .on_submit(|state: &mut AppState, _| state.start_add_mac())
                .build(k)
                .key("add-mac-password"),
            l,
        ))
}

fn adding(
    name: &str,
    current: Option<elsecall_fleet::add_mac::Step>,
    approval: Option<&(elsecall_fleet::remote_mac::LoginLink, Approval)>,
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
    let side = match approval {
        Some((link, approval)) => approve(name, link, *approval, clock, k, l),
        None if current.is_none() => col()
            .grow(1.0)
            .child(paragraph(&format!("Connecting to {name} over SSH…"), l)),
        None => col()
            .grow(1.0)
            .gap(theme::SPACE_3)
            .child(paragraph(
                &format!("Elsecall is setting up {name} over SSH. This takes a minute or two."),
                l,
            ))
            .child(paragraph(
                "If Tailscale needs your approval, its page opens in your browser and this sheet shows what to click.",
                l,
            )),
    };
    row().w_full().gap(theme::SPACE_6).child(steps).child(side)
}

fn approve(
    name: &str,
    link: &elsecall_fleet::remote_mac::LoginLink,
    approval: Approval,
    clock: f64,
    k: &Kit,
    l: &Look,
) -> El {
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

fn done(report: &AddReport, l: &Look) -> El {
    let model = report.mac.model.as_deref().unwrap_or("Mac");
    let standing = match report.readiness {
        Readiness::AwaitingProvisioning => {
            "It is a member awaiting provisioning: you can run work on it by name, and it takes no other work until its tools are installed."
        }
        Readiness::Ready => "It is a member and takes work; adding it again repaired it.",
    };
    let mut facts = vec![format!("SSH host key {}", report.mac.ssh_host_key)];
    if let Some(serial) = &report.mac.serial_number {
        facts.push(format!(
            "Serial number {serial}: compare it with About This Mac there."
        ));
    }
    facts.push(match &report.tailscale_key_expiry {
        Some(expiry) => format!(
            "Its Tailscale key expires {expiry}; to keep it on the tailnet, disable key expiry for it in the Tailscale admin console."
        ),
        None => "Its Tailscale key does not expire.".into(),
    });
    let mut page = col()
        .w_full()
        .gap(theme::SPACE_3)
        .child(
            row()
                .gap(theme::SPACE_3)
                .center_y()
                .child(
                    col()
                        .size(36.0, 36.0)
                        .radius(18.0)
                        .fill(l.good_fill)
                        .center()
                        .child(icon(&CHECK, 20.0).color(l.good)),
                )
                .child(
                    col()
                        .gap(2.0)
                        .child(text(report.member.clone()).font(l.title).color(l.text))
                        .child(
                            text(format!("{model} · macOS {}", report.mac.macos_version))
                                .font(l.small)
                                .color(l.subtle),
                        ),
                ),
        )
        .child(paragraph(standing, l));
    for fact in facts {
        page = page.child(text(fact).wrap().w_full().font(l.small).color(l.subtle));
    }
    page.key("add-mac-done")
}

fn failed(failure: &Failure, has_credentials: bool, k: &Kit, l: &Look) -> El {
    let mut card = col()
        .w_full()
        .gap(theme::SPACE_2)
        .padding(theme::SPACE_4)
        .radius(theme::RADIUS_SMALL)
        .fill(l.warn_fill)
        .child(
            text(failure.message.clone())
                .wrap()
                .w_full()
                .font(l.body)
                .color(l.text),
        )
        .child(
            text(failure.hint.clone())
                .wrap()
                .w_full()
                .font(l.small)
                .color(l.secondary),
        );
    // Still waiting for the approval: the page can be opened again from here.
    let pending = failure
        .details
        .as_ref()
        .and_then(|details| details["url"].as_str())
        .and_then(LoginLink::parse)
        .filter(|_| failure.code == "tailscale_login_pending");
    if let Some(link) = pending {
        let link = link.as_str().to_owned();
        card = card.child(
            row().child(
                Button::new("Open the page")
                    .variant(ButtonVariant::Outline)
                    .on_press_tx(move |_: &mut AppState, tx| {
                        let _ = tx.open_url(&link);
                    })
                    .build(k)
                    .key("add-mac-open-pending"),
            ),
        );
    }
    if has_credentials {
        card = card.child(
            text("Trying again continues where it stopped.")
                .wrap()
                .w_full()
                .font(l.small)
                .color(l.subtle),
        );
    }
    card.key("add-mac-failed")
}

fn prepare_buttons(k: &Kit) -> Vec<El> {
    vec![
        Button::new("Cancel")
            .variant(ButtonVariant::Ghost)
            .on_press(|state: &mut AppState| state.close_add_mac())
            .build(k)
            .key("add-mac-cancel"),
        Button::new("Remote Login is on")
            .variant(ButtonVariant::Contrast)
            .on_press(|state: &mut AppState| state.add_mac_mut().show_network())
            .build(k)
            .key("add-mac-continue"),
    ]
}

fn network_buttons(k: &Kit) -> Vec<El> {
    vec![
        Button::new("Back")
            .variant(ButtonVariant::Ghost)
            .on_press(|state: &mut AppState| state.add_mac_mut().back())
            .build(k)
            .key("add-mac-back"),
        Button::new("On the same network")
            .variant(ButtonVariant::Contrast)
            .on_press(|state: &mut AppState| state.add_mac_mut().show_choose())
            .build(k)
            .key("add-mac-continue"),
    ]
}

fn choose_buttons(state: &AppState, k: &Kit) -> Vec<El> {
    vec![
        Button::new("Back")
            .variant(ButtonVariant::Ghost)
            .on_press(|state: &mut AppState| state.add_mac_mut().back())
            .build(k)
            .key("add-mac-back"),
        Button::new("Continue")
            .variant(ButtonVariant::Contrast)
            .disabled(state.add_mac().chosen().is_none())
            .on_press(|state: &mut AppState| state.add_mac_mut().show_sign_in())
            .build(k)
            .key("add-mac-continue"),
    ]
}

fn sign_in_buttons(state: &AppState, k: &Kit) -> Vec<El> {
    vec![
        Button::new("Back")
            .variant(ButtonVariant::Ghost)
            .on_press(|state: &mut AppState| state.add_mac_mut().back())
            .build(k)
            .key("add-mac-back"),
        Button::new("Add Mac")
            .variant(ButtonVariant::Contrast)
            .disabled(!state.add_mac().can_add())
            .on_press(|state: &mut AppState| state.start_add_mac())
            .build(k)
            .key("add-mac-start"),
    ]
}

fn adding_buttons(k: &Kit) -> Vec<El> {
    vec![
        Button::new("Stop")
            .variant(ButtonVariant::Ghost)
            .on_press(|state: &mut AppState| state.stop_add_mac())
            .build(k)
            .key("add-mac-stop"),
    ]
}

fn done_buttons(k: &Kit) -> Vec<El> {
    vec![
        Button::new("Done")
            .variant(ButtonVariant::Contrast)
            .on_press(|state: &mut AppState| state.close_add_mac())
            .build(k)
            .key("add-mac-close"),
    ]
}

fn failed_buttons(failure: &Failure, has_credentials: bool, k: &Kit) -> Vec<El> {
    let mut buttons = vec![
        Button::new("Back")
            .variant(ButtonVariant::Ghost)
            .on_press(|state: &mut AppState| state.add_mac_mut().back())
            .build(k)
            .key("add-mac-back"),
    ];
    if !needs_new_target(failure) {
        buttons.push(
            Button::new(if has_credentials {
                "Try again"
            } else {
                "Edit sign-in"
            })
            .variant(ButtonVariant::Contrast)
            .on_press(|state: &mut AppState| state.retry_add_mac())
            .build(k)
            .key("add-mac-retry"),
        );
    }
    buttons
}
