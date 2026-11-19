//! Native resource meters and their separately anchored contributor popovers.
use crate::{
    app::AppState,
    fleet_source::{MemberRow, Reachability},
    resource_details::{Metric, Source},
    theme,
};
use rune::{El, Tx, col, el, row, text};
use rune_kit::{Cell, Kit, Placement, popover};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    member: String,
    metric: Metric,
    source: Source,
}

impl Target {
    fn key(&self) -> String {
        format!(
            "usage-{}-{}-{}",
            self.member,
            self.metric.name(),
            self.source.name()
        )
    }
}

#[derive(Default)]
pub(crate) struct Hover {
    shown: Option<Target>,
    close_at: Option<f64>,
    focused: Option<String>,
}

impl Hover {
    pub(crate) fn dismiss(&mut self) -> bool {
        self.close_at = None;
        self.shown.take().is_some()
    }

    fn enter(&mut self, target: &Target) {
        self.close_at = None;
        self.shown = Some(target.clone());
    }

    fn pointer(&mut self, target: &Target, inside: bool, tx: &mut Tx) {
        if inside {
            self.enter(target);
        } else if self.shown.as_ref() == Some(target) {
            let grace = Duration::from_millis(180);
            self.close_at = Some(tx.clock() + grace.as_secs_f64());
            tx.wake_after(grace);
        }
        tx.instant();
    }

    pub(crate) fn poll(&mut self, now: f64, tx: &Tx) -> bool {
        // Rune reports the leaf key for focus. The final two components are the
        // metric and source; member names may themselves contain hyphens.
        let focused = tx
            .focused()
            .filter(|_| tx.focus_visible())
            .and_then(|key| key.strip_prefix("usage-"));
        if self.focused.as_deref() != focused {
            let was_focused = self.focused.is_some();
            self.focused = focused.map(str::to_owned);
            if let Some(target) = focused.and_then(target_from_focus) {
                self.enter(&target);
                return true;
            }
            if was_focused {
                return self.dismiss();
            }
        }
        self.close_at.is_some_and(|until| now >= until) && self.dismiss()
    }
}

fn target_from_focus(key: &str) -> Option<Target> {
    let (prefix, source) = key.rsplit_once('-')?;
    let (member, metric) = prefix.rsplit_once('-')?;
    let source = [Source::Managed, Source::Outside]
        .into_iter()
        .find(|v| v.name() == source)?;
    let metric = [Metric::Cpu, Metric::Memory]
        .into_iter()
        .find(|v| v.name() == metric)?;
    Some(Target {
        member: member.into(),
        metric,
        source,
    })
}

pub(crate) fn cell(member: &MemberRow, metric: Metric, kit: &Kit) -> Cell {
    if member.reach == Reachability::Unreachable {
        return Cell::text("—").mono();
    }
    let (label, ratio) = match metric {
        Metric::Cpu => {
            let mut label = crate::view::cpu_label(member.cpu);
            if let Some(count) = member.logical_cpus {
                label.push_str(&format!(" · {count} CPUs"));
            }
            (label, member.cpu)
        }
        Metric::Memory => {
            let label = crate::view::memory_label(member.memory.as_ref());
            (
                label,
                member
                    .memory
                    .as_ref()
                    .filter(|m| m.total_bytes > 0)
                    .map(|m| m.used_bytes as f64 / m.total_bytes as f64),
            )
        }
    };
    let mut meter = row().w_full().h(3.0).radius(1.5);
    if let Some(ratio) = ratio {
        meter = meter.fill(kit.c.layer_02).child(
            el().w_frac(ratio.clamp(0.0, 1.0) as f32)
                .h_full()
                .radius(1.5)
                .fill(kit.c.info.icon),
        );
    }
    let mut triggers = row().w_full().gap(4.0);
    for source in [Source::Managed, Source::Outside] {
        let target = Target {
            member: member.name.clone(),
            metric,
            source,
        };
        let press = target.clone();
        triggers = triggers.child(
            row()
                .key(target.key())
                .described_by(format!("{}-details", target.key()))
                .fill(match source {
                    Source::Managed => kit.c.info.bg,
                    Source::Outside => kit.c.warning.bg,
                })
                .role("button")
                .focusable(true)
                .label(format!(
                    "{} {} on {}",
                    source.name(),
                    metric.name(),
                    member.name
                ))
                .center()
                .grow(1.0)
                .h(18.0)
                .radius(3.0)
                .hover(kit.c.surface_hover)
                .on_hover_change(move |state: &mut AppState, inside, tx| {
                    state.resource_hover.pointer(&target, inside, tx)
                })
                .on_press(move |state: &mut AppState| state.resource_hover.enter(&press))
                .child(
                    text(source.name())
                        .font(theme::scale().small)
                        .color(match source {
                            Source::Managed => kit.c.info.fg,
                            Source::Outside => kit.c.warning.fg,
                        }),
                ),
        );
    }
    Cell::el(
        col()
            .w_full()
            .center_y()
            .gap(1.0)
            .child(
                text(label)
                    .font(theme::scale().small)
                    .color(kit.c.text_base),
            )
            .child(meter)
            .child(triggers),
    )
}

pub(crate) fn overlay(state: &AppState, kit: &Kit, overlays: &mut Vec<El>) {
    let Some(target) = &state.resource_hover.shown else {
        return;
    };
    let Some(member) = state
        .rows()
        .iter()
        .find(|m| m.name == target.member && m.reach == Reachability::Reachable)
    else {
        return;
    };
    let details = member
        .contributors
        .details(target.metric, target.source, state.snapshot_at);
    let mut rows = col().w_full().gap(8.0);
    let capacity = match target.metric {
        Metric::Cpu => member.logical_cpus.map(|n| f64::from(n.get())),
        Metric::Memory => member.memory.as_ref().map(|m| m.total_bytes as f64),
    };
    for reading in details.readings {
        let bar = reading
            .measured
            .as_ref()
            .zip(capacity.filter(|n| *n > 0.0))
            .map(|(m, capacity)| {
                row().w_full().h(3.0).fill(kit.c.layer_02).child(
                    el().h_full()
                        .w_frac((m.amount / capacity).clamp(0.0, 1.0) as f32)
                        .fill(kit.c.warning.icon),
                )
            });
        rows = rows.child(
            col()
                .w_full()
                .gap(2.0)
                .child(
                    row()
                        .w_full()
                        .gap(6.0)
                        .child(
                            text(reading.label)
                                .grow(1.0)
                                .min_w(0.0)
                                .wrap()
                                .font(theme::scale().h5)
                                .color(kit.c.text_base),
                        )
                        .children(reading.measured.map(|m| {
                            text(m.value)
                                .font(theme::scale().small)
                                .color(kit.c.text_base)
                        })),
                )
                .children(bar)
                .child(
                    text(reading.detail)
                        .wrap()
                        .font(theme::scale().small)
                        .color(kit.c.text_muted),
                ),
        );
    }
    let owner = target.clone();
    let mut content = col()
        .key(format!("{}-details", target.key()))
        .role("group")
        .label(format!(
            "{} {} details",
            target.source.name(),
            target.metric.name()
        ))
        .w(318.0)
        .min_h(0.0)
        .gap(8.0)
        .padding(12.0)
        .child(
            text(format!(
                "{} {} · {}",
                target.source.name(),
                target.metric.name(),
                member.name
            ))
            .font(theme::scale().h4)
            .color(kit.c.text_base),
        )
        .child(
            text(details.caption)
                .wrap()
                .font(theme::scale().small)
                .color(kit.c.text_muted),
        );
    if state.stale() {
        content = content.child(
            text("Last available snapshot")
                .font(theme::scale().small)
                .color(kit.c.text_muted),
        );
    }
    content = content.child(popover::fit_scroll(rows)).on_hover_change(
        move |s: &mut AppState, inside, tx| s.resource_hover.pointer(&owner, inside, tx),
    );
    overlays.push(popover::anchored(
        kit,
        popover::raised(kit, content, 6.0),
        &target.key(),
        Placement::Bottom,
        2.0,
    ));
}
