//! The fleet window, as a pure function of its state.

use std::borrow::Cow;

use rune::tok::TextStyle;
use rune::{Align, Color, El, col, row, text};
use rune_kit::{Cell, Table, TableColumn};

use crate::app::AppState;
use crate::fleet_source::{Gate, LoadAverages, MemberRow, MemoryUse, Reachability, Role};
use crate::theme;

const DASH: &str = "—";
const STALE_OPACITY: f32 = 0.45;
const PILL_HEIGHT: f32 = 22.0;

#[derive(Clone, Copy)]
struct Paints {
    text: Color,
    secondary: Color,
    subtle: Color,
    page: Color,
    card: Color,
    border: Color,
    brand_fill: Color,
    brand_ink: Color,
    online_fill: Color,
    online_ink: Color,
    offline_fill: Color,
    offline_ink: Color,
    blocked_fill: Color,
    blocked_ink: Color,
    h3: TextStyle,
    body: TextStyle,
    small: TextStyle,
    label: TextStyle,
}

pub fn fleet(state: &AppState, cx: &mut rune::Cx<AppState>) -> El {
    let dark = cx.system_dark();
    let kit = theme::kit(dark);
    let paints = Paints {
        text: kit.c.text_base,
        secondary: kit.c.text_muted,
        subtle: kit.c.text_faint,
        page: kit.c.page,
        card: kit.c.layer_01,
        border: kit.c.border_base,
        brand_fill: kit.c.info.bg,
        brand_ink: kit.c.info.fg,
        online_fill: kit.c.success.bg,
        online_ink: kit.c.success.fg,
        offline_fill: kit.c.danger.bg,
        offline_ink: kit.c.danger.fg,
        blocked_fill: kit.c.warning.bg,
        blocked_ink: kit.c.warning.fg,
        h3: theme::scale().h3,
        body: theme::scale().body,
        small: theme::scale().small,
        label: theme::scale().label,
    };
    let page = col()
        .w_full()
        .h_full()
        .fill(paints.page)
        .padding(theme::SPACE_8)
        .gap(theme::SPACE_6);
    if matches!(state.phase(), crate::app::Phase::Loading) {
        return page.center().child(loading(&paints));
    }
    let shown = state.shown();
    let mut page = page.child(header(state, &paints));
    if let Some(error) = state.update_error() {
        page = page.child(text(error).font(paints.small).color(paints.secondary));
    }
    if let Some(failure) = state.failure() {
        page = page.child(banner(failure.message(), failure.hint(), &paints));
    }
    if shown.is_some_and(|snapshot| !snapshot.enrolled) {
        page = page.child(solo_line(&paints));
    }
    if shown.is_some_and(|snapshot| snapshot.members.is_empty()) {
        page = page.child(
            text("No members reported.")
                .font(paints.small)
                .color(paints.subtle),
        );
    }
    if shown.is_some_and(|snapshot| !snapshot.members.is_empty()) {
        page = page.child(member_card(state, dark, paints));
    }
    page
}

fn loading(paints: &Paints) -> El {
    col()
        .gap(theme::SPACE_2)
        .center()
        .child(text("Fleet").font(paints.h3).color(paints.text))
        .child(
            text("Reading the local service…")
                .font(paints.body)
                .color(paints.secondary),
        )
}

fn header(state: &AppState, paints: &Paints) -> El {
    let mut line = row()
        .w_full()
        .center_y()
        .gap(theme::SPACE_3)
        .child(text("Fleet").font(paints.h3).color(paints.text));
    if let Some(snapshot) = state.shown() {
        line = line.child(
            text(count_label(snapshot.members.len()))
                .font(paints.small)
                .color(paints.subtle),
        );
    }
    line = line.child(rune::el().grow(1.0));
    if state.update_available() {
        line = line.child(update_button(state, paints));
    }
    if let Some(at) = state.refreshed() {
        line = line.child(
            text(format!("Updated {at}"))
                .font(paints.small)
                .color(paints.subtle),
        );
    }
    line
}

fn update_button(state: &AppState, paints: &Paints) -> El {
    let label = if state.updating() {
        "Updating…"
    } else {
        "Update"
    };
    row()
        .center_y()
        .h(PILL_HEIGHT)
        .px(theme::SPACE_2)
        .radius(theme::RADIUS_PILL)
        .fill(paints.brand_fill)
        .child(text(label).font(paints.label).color(paints.brand_ink))
        .on_press(|state: &mut AppState| state.press_update())
}

fn banner(message: &str, hint: &str, paints: &Paints) -> El {
    col()
        .w_full()
        .padding(theme::SPACE_4)
        .gap(theme::SPACE_1)
        .radius(theme::RADIUS_SMALL)
        .fill(paints.card)
        .border(1.0, paints.border)
        .child(text(message).font(paints.body).color(paints.text))
        .child(text(hint).font(paints.small).color(paints.secondary))
}

fn solo_line(paints: &Paints) -> El {
    row()
        .center_y()
        .child(
            text("Not in a fleet — ")
                .font(paints.small)
                .color(paints.subtle),
        )
        .child(
            text("elsecall fleet enroll")
                .font(theme::scale().mono)
                .color(paints.secondary),
        )
        .child(
            text(" adds members")
                .font(paints.small)
                .color(paints.subtle),
        )
}

fn member_card(state: &AppState, dark: bool, paints: Paints) -> El {
    let table = members_table(state, dark, paints);
    let mut card = col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .fill(paints.card)
        .radius(theme::RADIUS_SMALL)
        .border(1.0, paints.border)
        .clip_rounded(theme::RADIUS_SMALL)
        .child(table.h_full().w_full().grow(1.0).min_h(0.0));
    if state.stale() {
        card = card.opacity(STALE_OPACITY);
    }
    card
}

fn members_table(state: &AppState, dark: bool, paints: Paints) -> El {
    Table::new(AppState::table, AppState::table_mut, row_count, row_id)
        .name("Fleet members")
        .style(theme::table_style(dark))
        .column(
            TableColumn::new("member", "Member")
                .flex(1.0)
                .min(160.0)
                .resizable(false),
        )
        .column(
            TableColumn::new("role", "Role")
                .width(96.0)
                .resizable(false),
        )
        .column(
            TableColumn::new("status", "Status")
                .width(210.0)
                .resizable(false),
        )
        .column(
            TableColumn::new("cpu", "CPU")
                .width(72.0)
                .align(Align::End)
                .resizable(false),
        )
        .column(
            TableColumn::new("memory", "Memory")
                .width(196.0)
                .align(Align::End)
                .resizable(false),
        )
        .column(
            TableColumn::new("load", "Load")
                .width(212.0)
                .align(Align::End)
                .resizable(false),
        )
        .cell(move |state, index, column| cell(&state.rows()[index], column, paints))
        .build(state, &theme::kit(dark))
}

fn cell(member: &MemberRow, column: &str, paints: Paints) -> Cell {
    match column {
        "member" => Cell::text(member.name.clone()).mono(),
        "role" => role_cell(member, paints),
        "status" => status_cell(member, paints),
        "cpu" => Cell::text(cpu_label(member.cpu)).mono(),
        "memory" => Cell::text(memory_label(member.memory.as_ref())).mono(),
        "load" => Cell::text(load_label(member.load.as_ref())).mono(),
        _ => Cell::text(""),
    }
}

fn role_cell(member: &MemberRow, paints: Paints) -> Cell {
    match member.role {
        Role::Master => Cell::el(pill(
            "master",
            paints.brand_fill,
            paints.brand_ink,
            paints.label,
        )),
        Role::Member => Cell::text("member").muted(),
    }
}

fn status_cell(member: &MemberRow, paints: Paints) -> Cell {
    let (label, fill, ink, detail) = match (&member.reach, &member.gate) {
        (Reachability::Unreachable, _) => {
            ("offline", paints.offline_fill, paints.offline_ink, None)
        }
        (Reachability::Reachable, Gate::Blocked { leading_process }) => (
            "blocked",
            paints.blocked_fill,
            paints.blocked_ink,
            leading_process.as_deref(),
        ),
        (Reachability::Reachable, Gate::Open) => {
            ("online", paints.online_fill, paints.online_ink, None)
        }
    };
    let mut line = row()
        .center_y()
        .gap(theme::SPACE_2)
        .child(pill(label, fill, ink, paints.label));
    if let Some(detail) = detail {
        line = line.child(
            text(detail)
                .font(paints.small)
                .color(paints.secondary)
                .truncate(),
        );
    }
    Cell::el(line)
}

fn pill(label: &str, fill: Color, ink: Color, font: TextStyle) -> El {
    row()
        .center_y()
        .h(PILL_HEIGHT)
        .px(theme::SPACE_2)
        .radius(theme::RADIUS_PILL)
        .fill(fill)
        .child(text(label).font(font).color(ink))
}

fn row_count(state: &AppState) -> usize {
    state.rows().len()
}

fn row_id(state: &AppState, index: usize) -> Cow<'_, str> {
    Cow::Borrowed(state.rows()[index].name.as_str())
}

pub(crate) fn update_available(installed: Option<&str>, running: Option<&str>) -> bool {
    match (installed, running) {
        (Some(installed), Some(running)) => installed != running,
        _ => false,
    }
}

pub(crate) fn count_label(count: usize) -> String {
    match count {
        1 => "1 member".into(),
        count => format!("{count} members"),
    }
}

pub(crate) fn cpu_label(cpu: Option<f64>) -> String {
    match cpu {
        Some(value) if value.is_finite() => format!("{}%", (value * 100.0).round() as i64),
        _ => DASH.into(),
    }
}

pub(crate) fn memory_label(memory: Option<&MemoryUse>) -> String {
    match memory {
        Some(memory) => format!(
            "{} / {}",
            compact_bytes(memory.used_bytes),
            compact_bytes(memory.total_bytes)
        ),
        None => DASH.into(),
    }
}

pub(crate) fn load_label(load: Option<&LoadAverages>) -> String {
    match load {
        Some(load)
            if [load.one, load.five, load.fifteen]
                .iter()
                .all(|value| value.is_finite()) =>
        {
            format!("{:.1}  {:.1}  {:.1}", load.one, load.five, load.fifteen)
        }
        _ => DASH.into(),
    }
}

pub(crate) fn compact_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    let (scaled, suffix) = if value >= KIB.powi(3) {
        (value / KIB.powi(3), "GB")
    } else if value >= KIB.powi(2) {
        (value / KIB.powi(2), "MB")
    } else if value >= KIB {
        (value / KIB, "KB")
    } else {
        return format!("{bytes} B");
    };
    if (scaled - scaled.round()).abs() < 0.05 {
        format!("{} {suffix}", scaled.round())
    } else {
        format!("{scaled:.1} {suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_update_is_offered_only_when_the_installed_window_differs() {
        assert!(!update_available(None, Some("aaaa")));
        assert!(!update_available(Some("aaaa"), None));
        assert!(!update_available(Some("aaaa"), Some("aaaa")));
        assert!(update_available(Some("bbbb"), Some("aaaa")));
    }

    #[test]
    fn labels_counts_cpu_memory_and_load() {
        assert_eq!(count_label(1), "1 member");
        assert_eq!(count_label(3), "3 members");
        assert_eq!(cpu_label(Some(0.434)), "43%");
        assert_eq!(cpu_label(Some(f64::NAN)), "—");
        assert_eq!(cpu_label(None), "—");
        assert_eq!(compact_bytes(500), "500 B");
        assert_eq!(compact_bytes(1536), "1.5 KB");
        assert_eq!(compact_bytes(10 * 1024 * 1024), "10 MB");
        assert_eq!(compact_bytes(36 << 30), "36 GB");
        assert_eq!(
            memory_label(Some(&MemoryUse {
                used_bytes: 36 << 30,
                total_bytes: 64 << 30,
            })),
            "36 GB / 64 GB"
        );
        assert_eq!(memory_label(None), "—");
        assert_eq!(
            load_label(Some(&LoadAverages {
                one: 1.25,
                five: 1.0,
                fifteen: 0.94,
            })),
            "1.2  1.0  0.9"
        );
    }
}
