//! The sheet's looping illustrations of actions the operator takes.

use super::{CHECK, Look};
use crate::theme;
use rune::{Color, El, Icon, col, icon, row, text};

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
pub(super) fn settings_art(clock: f64, l: &Look) -> El {
    let t = phase(clock, TOGGLE_LOOP);
    let on = between(t, 0.48, 0.56);
    let reach = between(t, 0.22, 0.46);
    let leave = between(t, 0.84, 0.98);
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
    settings_window("General", pane, x, y, leave, l).key("add-mac-settings-art")
}

/// The Wi-Fi pane showing an example network becoming connected.
pub(super) fn wifi_art(clock: f64, l: &Look) -> El {
    let t = phase(clock, TOGGLE_LOOP);
    let reach = between(t, 0.18, 0.46);
    let connected = between(t, 0.51, 0.64);
    let leave = between(t, 0.84, 0.98);
    let network = row()
        .w_full()
        .h(32.0)
        .px(6.0)
        .gap(4.0)
        .center_y()
        .radius(5.0)
        .fill(l.accent.alpha(0.08 + 0.12 * reach))
        .border(1.0, l.accent.alpha(0.35 + 0.4 * reach))
        .child(text("Home Wi-Fi").font(l.small).color(l.text))
        .child(rune::el().grow(1.0))
        .child(if connected > 0.5 {
            row()
                .center_y()
                .child(icon(&CHECK, 14.0).color(l.good))
                .child(text("Connected").font(l.small).color(l.good))
        } else {
            row()
                .px(6.0)
                .h(20.0)
                .center_y()
                .radius(4.0)
                .fill(l.card)
                .child(text("Join").font(l.small).color(l.accent_ink))
        });
    let pane = col()
        .grow(1.0)
        .h_full()
        .padding(theme::SPACE_2)
        .gap(theme::SPACE_2)
        .child(text("Wi-Fi").font(l.label).color(l.text))
        .child(text("Known Networks").font(l.small).color(l.secondary))
        .child(network)
        .child(
            row()
                .w_full()
                .h(28.0)
                .px(6.0)
                .center_y()
                .child(text("Guest Wi-Fi").font(l.small).color(l.subtle)),
        );
    let (x, y) = (mix(145.0, 267.0, reach), mix(152.0, 83.0, reach));
    settings_window("Wi‑Fi", pane, x, y, leave, l).key("add-mac-wifi-art")
}

fn settings_window(section: &str, pane: El, x: f32, y: f32, leave: f32, l: &Look) -> El {
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
    for label in ["Wi‑Fi", "Bluetooth", "Network", "General", "Appearance"] {
        let selected = label == section;
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

/// A browser page whose button a pointer clicks, which then shows a check.
pub(super) fn browser_art(name: &str, button: &str, clock: f64, l: &Look) -> El {
    let t = phase(clock, APPROVE_LOOP);
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
