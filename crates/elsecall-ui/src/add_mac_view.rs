//! The Add Mac sheet: what to do on the new Mac, which Mac, its account, and the
//! add as it happens, with looping illustrations of the two things a person does:
//! turning on Remote Login there, and approving it in the browser here.

use elsecall_add_mac::add_mac::Approval;
use rune::tok::TextStyle;
use rune::{Color, El, Icon, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, Dialog, Kit, Spinner, TextInput};

use crate::add_mac_flow::{
    MacStatus, Nearby, NearbyMacExt, Page, STEPS, StepState, Target, step_label, step_state,
};
use crate::app::AppState;
use crate::theme;
use elsecall_add_mac::add_mac::AddReport;
use elsecall_add_mac::nearby_macs::NearbyMac;
use elsecall_add_mac::remote_mac::LoginLink;
use elsecall_failure::Failure;
use elsecall_membership::roster::Readiness;

const SHEET_WIDTH: f32 = 680.0;
const ART_WIDTH: f32 = 300.0;
const ART_HEIGHT: f32 = 188.0;
/// One loop of the Remote Login illustration, in seconds.
const TOGGLE_LOOP: f64 = 4.5;
/// One loop of the browser approval illustration, in seconds.
const APPROVE_LOOP: f64 = 3.6;

static POINTER: Icon = Icon {
    name: "elsecall-pointer",
    view: 16.0,
    body: r#"<path d="M3 1.5V13.2L6.1 10.4L8.2 15L10.3 14L8.2 9.5H12.4Z" fill="currentColor" stroke="white" stroke-width="1" stroke-linejoin="round"/>"#,
};
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
            failed(failure, k, &l),
            failed_buttons(k),
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

/// Where the sheet is: four stages, the current one marked.
fn stepper(page: &Page, l: &Look) -> El {
    let at = match page {
        Page::Closed | Page::Prepare => 0,
        Page::Choose { .. } => 1,
        Page::SignIn { .. } => 2,
        Page::Adding { .. } | Page::Done { .. } | Page::Failed { .. } => 3,
    };
    let mut line = row().center_y().gap(theme::SPACE_2);
    for (index, label) in ["On the new Mac", "Choose it", "Sign in", "Add"]
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
        ))
        .child(numbered(
            3,
            paragraph(
                "Keep it on the same network as this Mac. Everything else happens from here: Elsecall installs itself and Tailscale there, and you approve it once in your browser.",
                l,
            ),
            l,
        ));
    row()
        .w_full()
        .gap(theme::SPACE_6)
        .child(settings_art(phase(clock, TOGGLE_LOOP), l))
        .child(steps)
}

/// Where a loop of `length` seconds is, from 0 to 1.
fn phase(clock: f64, length: f64) -> f32 {
    if clock == f64::MAX {
        return 1.0;
    }
    ((clock.rem_euclid(length)) / length) as f32
}

/// `t` eased between `from` and `to` (both 0–1 within the loop).
fn between(t: f32, from: f32, to: f32) -> f32 {
    let x = ((t - from) / (to - from)).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

fn mix(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// A small System Settings window whose Remote Login switch a pointer turns on.
fn settings_art(t: f32, l: &Look) -> El {
    let on = between(t, 0.48, 0.56);
    let reach = between(t, 0.22, 0.46);
    let leave = between(t, 0.84, 0.98);
    let titlebar = row()
        .h(22.0)
        .w_full()
        .px(theme::SPACE_2)
        .center_y()
        .gap(5.0)
        .children(
            [
                rune::oklch(0.68, 0.19, 27.0),
                rune::oklch(0.83, 0.16, 80.0),
                rune::oklch(0.73, 0.19, 145.0),
            ]
            .map(|c| rune::el().size(8.0, 8.0).radius(4.0).fill(c)),
        );
    let mut sidebar = col()
        .w(92.0)
        .h_full()
        .padding(theme::SPACE_2)
        .gap(3.0)
        .fill(l.raised);
    for (label, selected) in [
        ("Wi‑Fi", false),
        ("Bluetooth", false),
        ("Network", false),
        ("General", true),
        ("Appearance", false),
    ] {
        let item = row().w_full().h(17.0).px(5.0).center_y().radius(4.0).child(
            text(label)
                .font(l.small)
                .color(if selected { l.accent_ink } else { l.secondary }),
        );
        sidebar = sidebar.child(if selected {
            item.fill(l.accent.alpha(0.22))
        } else {
            item
        });
    }
    let mut pane = col()
        .grow(1.0)
        .h_full()
        .padding(theme::SPACE_2)
        .gap(4.0)
        .child(text("General › Sharing").font(l.label).color(l.text));
    for (label, value) in [
        ("Screen Sharing", 0.0),
        ("File Sharing", 0.0),
        ("Remote Login", on),
        ("Remote Management", 0.0),
    ] {
        let focus = label == "Remote Login";
        let line = row()
            .w_full()
            .h(22.0)
            .px(6.0)
            .center_y()
            .radius(5.0)
            .fill(l.card)
            .child(
                text(label)
                    .font(l.small)
                    .color(if focus { l.text } else { l.secondary }),
            )
            .child(rune::el().grow(1.0))
            .child(switch(value, l));
        pane = pane.child(if focus {
            line.border(1.0, l.accent.alpha(0.35 + 0.4 * reach))
        } else {
            line
        });
    }
    // The pointer comes from the lower left to the switch, clicks, and fades.
    let (x, y) = (mix(120.0, 262.0, reach), mix(176.0, 112.0, reach));
    let pointer = icon(&POINTER, 16.0)
        .color(l.text)
        .opacity(1.0 - leave)
        .absolute(x, y)
        .key("add-mac-pointer");
    rune::el()
        .size(ART_WIDTH, ART_HEIGHT)
        .child(
            col()
                .size(ART_WIDTH, ART_HEIGHT)
                .radius(10.0)
                .fill(l.card)
                .border(1.0, l.border)
                .clip_rounded(10.0)
                .shadow(6.0, 18.0, Color::BLACK.alpha(0.16))
                .child(titlebar)
                .child(row().w_full().grow(1.0).child(sidebar).child(pane)),
        )
        .child(pointer)
        .key("add-mac-settings-art")
}

/// A switch, `on` from 0 (off) to 1 (on).
fn switch(on: f32, l: &Look) -> El {
    let track = if on > 0.5 { l.good } else { l.track };
    rune::el().size(26.0, 15.0).radius(7.5).fill(track).child(
        rune::el()
            .size(11.0, 11.0)
            .radius(5.5)
            .fill(l.knob)
            .absolute(2.0 + 11.0 * on, 2.0),
    )
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
                    "No Mac on this network advertises itself yet. When Remote Login is on, the new Mac shows up here within a few seconds; you can also type its address below.",
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
        MacStatus::Member(_) => ("Member · repair", l.raised, l.secondary),
        MacStatus::PossibleMember(_) => ("May be a member", l.raised, l.secondary),
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
            "An administrator account on that Mac. The password is used only while Elsecall sets it up, and is never stored.",
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
    current: Option<elsecall_add_mac::add_mac::Step>,
    approval: Option<&(elsecall_add_mac::remote_mac::LoginLink, Approval)>,
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
    link: &elsecall_add_mac::remote_mac::LoginLink,
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
        .child(browser_art(name, button, phase(clock, APPROVE_LOOP), l))
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

/// A browser page whose button a pointer clicks, which then shows a check.
fn browser_art(name: &str, button: &str, t: f32, l: &Look) -> El {
    let reach = between(t, 0.12, 0.42);
    let pressed = between(t, 0.44, 0.5) * (1.0 - between(t, 0.5, 0.56));
    let joined = between(t, 0.55, 0.65);
    let leave = between(t, 0.85, 0.98);
    let address = row()
        .h(26.0)
        .w_full()
        .px(theme::SPACE_2)
        .gap(theme::SPACE_2)
        .center_y()
        .child(
            row()
                .grow(1.0)
                .h(16.0)
                .px(theme::SPACE_2)
                .center_y()
                .radius(8.0)
                .fill(l.raised)
                .child(text("login.tailscale.com").font(l.small).color(l.secondary)),
        );
    let action: El = if joined > 0.5 {
        row()
            .gap(theme::SPACE_1)
            .center_y()
            .opacity(joined)
            .child(icon(&CHECK, 16.0).color(l.good))
            .child(text("Connected").font(l.label).color(l.good))
    } else {
        row()
            .h(26.0)
            .px(theme::SPACE_4)
            .center()
            .radius(6.0)
            .fill(l.accent.alpha(1.0 - 0.25 * pressed))
            .child(text(button).font(l.label).color(Color::WHITE))
    };
    let page = col()
        .w_full()
        .grow(1.0)
        .center()
        .gap(theme::SPACE_2)
        .child(text("Connect device").font(l.label).color(l.secondary))
        .child(text(name.to_owned()).font(l.mono).color(l.text))
        .child(action);
    let (x, y) = (mix(40.0, 160.0, reach), mix(138.0, 112.0, reach));
    rune::el()
        .size(ART_WIDTH, ART_HEIGHT - 28.0)
        .child(
            col()
                .size(ART_WIDTH, ART_HEIGHT - 28.0)
                .radius(10.0)
                .fill(l.card)
                .border(1.0, l.border)
                .clip_rounded(10.0)
                .child(address)
                .child(rune::el().h(1.0).w_full().fill(l.border))
                .child(page),
        )
        .child(
            icon(&POINTER, 16.0)
                .color(l.text)
                .opacity(1.0 - leave)
                .absolute(x, y),
        )
        .key("add-mac-browser-art")
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

fn failed(failure: &Failure, k: &Kit, l: &Look) -> El {
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
    card.child(
        text("Trying again continues where it stopped.")
            .wrap()
            .w_full()
            .font(l.small)
            .color(l.subtle),
    )
    .key("add-mac-failed")
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

fn failed_buttons(k: &Kit) -> Vec<El> {
    vec![
        Button::new("Back")
            .variant(ButtonVariant::Ghost)
            .on_press(|state: &mut AppState| state.add_mac_mut().back())
            .build(k)
            .key("add-mac-back"),
        Button::new("Try again")
            .variant(ButtonVariant::Contrast)
            .on_press(|state: &mut AppState| state.add_mac_mut().retry())
            .build(k)
            .key("add-mac-retry"),
    ]
}
