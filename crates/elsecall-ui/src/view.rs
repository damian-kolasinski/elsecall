//! The fleet window, as a pure function of its state.

use std::borrow::Cow;
use std::num::NonZeroU32;

use rune::tok::TextStyle;
use rune::{Align, Color, El, HelpLook, Span, col, rich, row, text};
use rune_kit::{Button, Cell, Table, TableColumn};

use crate::app::AppState;
use crate::fleet_source::{
    FleetSnapshot, Gate, LoadAverages, MemberRow, MemoryUse, Reachability, Release, Role, Standing,
};
use crate::load_trend::LoadTrend;
use crate::theme;

const DASH: &str = "—";
const STALE_OPACITY: f32 = 0.45;
const PILL_HEIGHT: f32 = 22.0;
const BORDER_WIDTH: f32 = 1.0;

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
    role_fill: Color,
    role_ink: Color,
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

pub(crate) fn fleet(state: &AppState, cx: &mut rune::Cx<AppState>) -> El {
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
        role_fill: kit.c.layer_02,
        role_ink: kit.c.text_base,
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
    cx.help_look(
        HelpLook::system(dark)
            .fill(paints.page)
            .ink(paints.text)
            .ring(BORDER_WIDTH, paints.border)
            .font(paints.small)
            .padding(theme::SPACE_3, theme::SPACE_2)
            .max_width(Some(264.0)),
    );
    let page = col()
        .w_full()
        .h_full()
        .fill(paints.page)
        .padding(theme::SPACE_8)
        .gap(theme::SPACE_6);
    if matches!(state.phase(), crate::app::Phase::Loading) && !state.update_available() {
        return page.center().child(loading(&paints));
    }
    let shown = state.shown();
    let mut page = page.child(header(state, &paints, dark));
    if matches!(state.phase(), crate::app::Phase::Loading) {
        page = page.child(
            col().w_full().grow(1.0).center().child(
                text("Reading the local service…")
                    .font(paints.body)
                    .color(paints.secondary),
            ),
        );
    }
    if let Some(error) = state.update_error() {
        page = page.child(text(error).font(paints.small).color(paints.secondary));
    }
    if let Some(failure) = state.failure() {
        page = page.child(banner(failure.message(), failure.hint(), &paints));
    }
    // Enrolment and an empty member list mean nothing until the Mac has a roster.
    match shown.map(|snapshot| &snapshot.standing) {
        Some(Standing::OffTailnet { hint }) => {
            return page.child(banner(
                "This Mac is not on a tailnet yet",
                &format!(
                    "Elsecall finds and reaches your Macs over Tailscale; on a tailnet, this Mac is the master of its own fleet. {hint}"
                ),
                &paints,
            ));
        }
        Some(Standing::Unrostered { message, hint }) => {
            return page.child(banner(message, hint, &paints));
        }
        Some(Standing::Rostered) | None => {}
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
    if let Some(line) = shown.and_then(|snapshot| release_summary(snapshot, state.fleet_updating()))
    {
        page = page.child(text(line).wrap().font(paints.small).color(paints.secondary));
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

fn header(state: &AppState, paints: &Paints, dark: bool) -> El {
    let mut line = row()
        .w_full()
        .center_y()
        .gap(theme::SPACE_3)
        .child(text("Fleet").font(paints.h3).color(paints.text));
    if let Some(snapshot) = state
        .shown()
        .filter(|snapshot| snapshot.standing == Standing::Rostered)
    {
        line = line.child(
            text(count_label(snapshot.members.len()))
                .font(paints.small)
                .color(paints.subtle),
        );
    }
    line = line.child(rune::el().grow(1.0));
    if let Some(notice) = state.release_notice() {
        line = line.child(text(notice).font(paints.small).color(paints.secondary));
    }
    if state.update_available() {
        line = line.child(update_button(state, dark));
    }
    if state
        .shown()
        .is_some_and(|snapshot| snapshot.this_is_master())
    {
        line = line.child(
            Button::new("Add Mac")
                .disabled(state.stale())
                .on_press(|state: &mut AppState| state.open_add_mac())
                .build(&theme::kit(dark))
                .key("add-mac"),
        );
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

fn update_button(state: &AppState, dark: bool) -> El {
    Button::new(state.upgrade_label())
        .disabled(state.updating())
        .on_press(|state: &mut AppState| state.press_update())
        .build(&theme::kit(dark))
        .key("upgrade-window")
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
        .child(
            text(hint)
                .wrap()
                .w_full()
                .font(paints.small)
                .color(paints.secondary),
        )
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
        .border(BORDER_WIDTH, paints.border)
        .clip_rounded(theme::RADIUS_SMALL)
        // Row highlights stay inside the border: the table sits within its 1pt width
        // and is clipped to the border's inner curve at the corners.
        .padding(BORDER_WIDTH)
        .child(
            col()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .clip_rounded(theme::RADIUS_SMALL - BORDER_WIDTH)
                .child(table.h_full().w_full().grow(1.0).min_h(0.0)),
        );
    if state.stale() {
        card = card.opacity(STALE_OPACITY);
    }
    card
}

fn members_table(state: &AppState, dark: bool, paints: Paints) -> El {
    let kit = theme::kit(dark);
    let cell_kit = kit.clone();
    Table::new(AppState::table, AppState::table_mut, row_count, row_id)
        .name("Fleet members")
        .style(theme::table_style(dark))
        .column(
            TableColumn::new("member", "Member")
                .flex(1.0)
                .min(120.0)
                .resizable(false),
        )
        .column(
            TableColumn::new("role", "Role")
                .width(104.0)
                .resizable(false),
        )
        .column(
            TableColumn::new("status", "Status")
                .width(152.0)
                .resizable(false),
        )
        .column(
            TableColumn::new("release", "Release")
                .width(148.0)
                .resizable(false),
        )
        .column(
            TableColumn::new("cpu", "CPU")
                .width(160.0)
                .align(Align::End)
                .resizable(false),
        )
        .column(
            TableColumn::new("memory", "Memory")
                .width(160.0)
                .align(Align::End)
                .resizable(false),
        )
        .column(
            TableColumn::new("cpu-demand", "CPU demand")
                .width(124.0)
                .align(Align::End)
                .resizable(false),
        )
        .cell(move |state, index, column| cell(&state.rows()[index], column, paints, &cell_kit))
        .build(state, &kit)
}

fn cell(member: &MemberRow, column: &str, paints: Paints, kit: &rune_kit::Kit) -> Cell {
    match column {
        "member" => crate::resource_display::member_cell(member, kit),
        "role" => role_cell(member, paints),
        "status" => status_cell(member, paints),
        "release" => release_cell(&member.release, paints),
        "cpu" => crate::resource_display::cell(member, crate::resource_details::Metric::Cpu, kit),
        "memory" => {
            crate::resource_display::cell(member, crate::resource_details::Metric::Memory, kit)
        }
        "cpu-demand" => cpu_demand_cell(member.load, member.logical_cpus, paints),
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
        Role::Member => Cell::el(pill(
            "member",
            paints.role_fill,
            paints.role_ink,
            paints.label,
        )),
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

fn cpu_demand_cell(load: Option<LoadAverages>, count: Option<NonZeroU32>, paints: Paints) -> Cell {
    let Some(load) = load else {
        return Cell::text(DASH).mono();
    };
    let raw_load = load_summary(&load, LoadTrend::of(&load));
    let line = row().w_full().h_full().justify_end().center_y();
    let Some(count) = count else {
        return Cell::el(
            line.label("CPU demand unavailable")
                .help("This Mac did not report its logical CPU count.")
                .help_row(raw_load)
                .child(
                    text(DASH)
                        .font(theme::scale().mono)
                        .color(paints.text)
                        .a11y_hidden(),
                ),
        );
    };
    let demand = load.demand(count);
    let label = format!("{demand:.1}×");
    let capacity = format!("{label} CPU capacity");
    let explanation = cpu_demand_explanation(demand, count);
    let baseline = "At 1.0×, demand matches CPU capacity.";
    let underline = Span::new(0, label.len()).underline(paints.subtle);
    Cell::el(
        line.label(capacity.clone())
            .description(format!("{explanation} {baseline} {raw_load}"))
            .help(capacity)
            .help_row(explanation)
            .help_row(baseline)
            .help_row(raw_load)
            .child(
                rich(label, vec![underline])
                    .font(theme::scale().mono)
                    .color(paints.text)
                    .a11y_hidden(),
            ),
    )
}

fn release_cell(release: &Release, paints: Paints) -> Cell {
    let (version, mark) = match release {
        Release::Matching { version } => (version.as_str(), None),
        Release::Behind { version } => (version.as_str(), Some("outdated")),
        Release::Ahead { version } => (version.as_str(), Some("newer")),
        Release::OtherBuild { version } => (version.as_str(), Some("differs")),
        Release::Unknown {
            version: Some(version),
        } => return Cell::text(version.clone()).mono().muted(),
        Release::Unknown { version: None } => return Cell::text("—").muted(),
    };
    let mut line = row().center_y().gap(theme::SPACE_2).child(
        text(version)
            .font(theme::scale().mono)
            .color(paints.text)
            .truncate(),
    );
    if let Some(mark) = mark {
        line = line.child(pill(
            mark,
            paints.blocked_fill,
            paints.blocked_ink,
            paints.label,
        ));
    }
    Cell::el(line)
}

/// One line naming the members on another build than the master, which placement
/// skips until they match, and the repair.
pub(crate) fn release_summary(snapshot: &FleetSnapshot, updating: bool) -> Option<String> {
    let names: Vec<&str> = snapshot
        .members
        .iter()
        .filter(|member| member.release.differs())
        .map(|member| member.name.as_str())
        .collect();
    let (subject, verb, object) = match names.as_slice() {
        [] => return None,
        [only] => (only.to_string(), "runs", "it"),
        [rest @ .., last] => (format!("{} and {last}", rest.join(", ")), "run", "them"),
    };
    let repair = if updating {
        format!("The master is giving {object} its build now.")
    } else {
        format!("Run elsecall fleet upgrade on the master to give {object} the master's build.")
    };
    Some(format!(
        "{subject} {verb} another build than the master, so no work is placed on {object}. {repair}"
    ))
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

fn logical_cpu_label(count: NonZeroU32) -> String {
    let suffix = if count.get() == 1 { "" } else { "s" };
    format!("{count} logical CPU{suffix}")
}

fn cpu_demand_explanation(demand: f64, count: NonZeroU32) -> String {
    let cpus = logical_cpu_label(count);
    if (demand - 1.0).abs() < 0.05 {
        format!(
            "Over the past minute, roughly as much work was ready to run as this Mac’s {cpus} could handle at once."
        )
    } else if demand > 1.0 {
        let excess = (demand - 1.0) * 100.0;
        format!(
            "Over the past minute, about {excess:.0}% more work was ready to run than this Mac’s {cpus} could handle at once."
        )
    } else {
        let percent = demand * 100.0;
        format!(
            "Over the past minute, the work ready to run would fill about {percent:.0}% of this Mac’s CPU capacity."
        )
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

/// The raw averages and their trend, shown in the demand tooltip.
pub(crate) fn load_summary(load: &LoadAverages, trend: LoadTrend) -> String {
    format!(
        "Load (1/5/15 min) {:.1} / {:.1} / {:.1} · {}",
        load.one,
        load.five,
        load.fifteen,
        trend.word()
    )
}

pub(crate) fn compact_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    let (scaled, suffix) = if value >= KIB.powi(3) {
        (value / KIB.powi(3), "GiB")
    } else if value >= KIB.powi(2) {
        (value / KIB.powi(2), "MiB")
    } else if value >= KIB {
        (value / KIB, "KiB")
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

    fn fleet(releases: &[(&str, Release)]) -> FleetSnapshot {
        FleetSnapshot {
            enrolled: true,
            members: releases
                .iter()
                .map(|(name, release)| MemberRow {
                    name: (*name).into(),
                    this_member: false,
                    role: Role::Member,
                    reach: Reachability::Reachable,
                    gate: Gate::Open,
                    cpu: None,
                    logical_cpus: None,
                    memory: None,
                    load: None,
                    release: release.clone(),
                    contributors: Default::default(),
                })
                .collect(),
            standing: Standing::Rostered,
        }
    }

    #[test]
    fn while_the_master_updates_the_fleet_the_summary_says_so() {
        let behind = Release::Behind {
            version: "0.2.1".into(),
        };
        let line = release_summary(&fleet(&[("macbook-pro", behind)]), true).unwrap();
        assert!(
            line.ends_with("The master is giving it its build now."),
            "{line}"
        );
    }

    #[test]
    fn the_summary_names_members_on_another_release_and_the_repair() {
        let matching = |version: &str| Release::Matching {
            version: version.into(),
        };
        let behind = Release::Behind {
            version: "0.2.1".into(),
        };
        assert_eq!(
            release_summary(&fleet(&[("mac-mini", matching("0.2.2"))]), false),
            None
        );
        // An unreachable member's release is unknown, not outdated.
        assert_eq!(
            release_summary(
                &fleet(&[("macbook", Release::Unknown { version: None })]),
                false
            ),
            None
        );
        assert_eq!(
            release_summary(
                &fleet(&[
                    ("mac-mini", matching("0.2.2")),
                    ("macbook-pro", behind.clone())
                ]),
                false
            )
            .unwrap(),
            "macbook-pro runs another build than the master, so no work is placed on it. Run elsecall fleet upgrade on the master to give it the master's build."
        );
        assert!(
            release_summary(
                &fleet(&[
                    ("a", behind.clone()),
                    (
                        "b",
                        Release::OtherBuild {
                            version: "0.2.2".into()
                        }
                    ),
                    (
                        "c",
                        Release::Ahead {
                            version: "0.3.0".into()
                        }
                    )
                ]),
                false
            )
            .unwrap()
            .starts_with(
                "a, b and c run another build than the master, so no work is placed on them."
            )
        );
    }

    #[test]
    fn labels_counts_cpu_memory_and_load() {
        assert_eq!(count_label(1), "1 member");
        assert_eq!(count_label(3), "3 members");
        assert_eq!(cpu_label(Some(0.434)), "43%");
        assert_eq!(cpu_label(Some(f64::NAN)), "—");
        assert_eq!(cpu_label(None), "—");
        assert_eq!(compact_bytes(500), "500 B");
        assert_eq!(compact_bytes(1536), "1.5 KiB");
        assert_eq!(compact_bytes(10 * 1024 * 1024), "10 MiB");
        assert_eq!(compact_bytes(36 << 30), "36 GiB");
        assert_eq!(
            memory_label(Some(&MemoryUse {
                used_bytes: 36 << 30,
                total_bytes: 64 << 30,
            })),
            "36 GiB / 64 GiB"
        );
        assert_eq!(memory_label(None), "—");
        let load = LoadAverages {
            one: 207.4,
            five: 187.0,
            fifteen: 141.3,
        };
        assert_eq!(
            load_summary(&load, LoadTrend::of(&load)),
            "Load (1/5/15 min) 207.4 / 187.0 / 141.3 · rising"
        );
    }

    #[test]
    fn demand_uses_the_one_minute_average_and_explains_capacity() {
        let count = NonZeroU32::new(8).unwrap();
        let load = LoadAverages {
            one: 12.0,
            five: 9.4,
            fifteen: 7.2,
        };
        assert_eq!(logical_cpu_label(NonZeroU32::MIN), "1 logical CPU");
        assert_eq!(logical_cpu_label(count), "8 logical CPUs");
        assert_eq!(load.demand(count), 1.5);
        assert_eq!(
            cpu_demand_explanation(load.demand(count), count),
            "Over the past minute, about 50% more work was ready to run than this Mac’s 8 logical CPUs could handle at once."
        );
        assert!(cpu_demand_explanation(0.4, count).contains("about 40%"));
        assert!(cpu_demand_explanation(1.0, count).contains("roughly as much work"));
    }
}
