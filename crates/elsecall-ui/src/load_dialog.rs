//! One machine's allocations and reported process measurements, viewed together.
use crate::{
    app::AppState,
    fleet_source::MemberRow,
    resource_details::{Details, Metric},
    theme,
};
use rune::{Color, El, ScrollSpec, col, el, row, text};
use rune_kit::{Dialog, Kit};

pub(crate) fn overlay(state: &AppState, kit: &Kit, layers: &mut Vec<El>) {
    let Some(member) = state
        .load_member
        .as_ref()
        .and_then(|name| state.rows().iter().find(|m| &m.name == name))
    else {
        return;
    };
    let data = &member.contributors;
    let managed = group("Elsecall-managed work", kit.c.info.icon, kit).child(readings(
        data.managed(),
        None,
        kit,
    ));
    let mut outside = group("Unmanaged processes", kit.c.warning.icon, kit);
    for (label, metric, capacity) in [
        (
            "CPU",
            Metric::Cpu,
            member.logical_cpus.map(|n| f64::from(n.get())),
        ),
        (
            "Memory",
            Metric::Memory,
            member.memory.as_ref().map(|m| m.total_bytes as f64),
        ),
    ] {
        outside = outside.child(
            col()
                .w_full()
                .gap(4.0)
                .child(text(label).font(theme::scale().h5).color(kit.c.text_base))
                .child(readings(
                    data.outside(metric, state.snapshot_at),
                    capacity.map(|n| (n, metric)),
                    kit,
                )),
        );
    }
    let body = row()
        .w_full()
        .items_start()
        .gap(16.0)
        .padding(12.0)
        .child(managed)
        .child(outside);
    let scroll = col()
        .key("load-breakdown")
        .focusable(true)
        .label("Resource breakdown")
        .focus_return(format!("load-details:{}", member.name))
        .w_full()
        .h(344.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(body);
    Dialog::new("machine-load", format!("Load details · {}", member.name))
        .description(summary(member, state.stale()))
        .width(900.0)
        .fit()
        .open(true)
        .on_close(|state: &mut AppState| state.load_member = None)
        .child(scroll)
        .build(kit, layers);
}

fn summary(member: &MemberRow, stale: bool) -> String {
    let cpu = crate::view::cpu_label(member.cpu);
    let ram = crate::view::memory_label(member.memory.as_ref());
    let freshness = if stale {
        " · Last available snapshot"
    } else {
        ""
    };
    format!("CPU {cpu} · Memory {ram}{freshness}")
}

fn group(name: &str, accent: Color, kit: &Kit) -> El {
    col()
        .w(0.0)
        .grow(1.0)
        .min_w(0.0)
        .gap(12.0)
        .padding(12.0)
        .border(1.0, kit.c.border_base)
        .radius(theme::RADIUS_SMALL)
        .child(
            row()
                .gap(8.0)
                .center_y()
                .child(el().w(4.0).h(16.0).radius(2.0).fill(accent))
                .child(text(name).font(theme::scale().h4).color(kit.c.text_base)),
        )
}

fn readings(details: Details, scale: Option<(f64, Metric)>, kit: &Kit) -> El {
    let scale = scale.filter(|(n, _)| *n > 0.0);
    let mut list = col().w_full().gap(8.0);
    let mut caption = details.caption;
    if !details.readings.is_empty()
        && let Some((_, metric)) = scale
    {
        if !caption.is_empty() {
            caption.push(' ');
        }
        caption.push_str(match metric {
            Metric::Cpu => "Bars show share of machine CPU.",
            Metric::Memory => "Bars show share of physical memory.",
        });
    }
    if !caption.is_empty() {
        list = list.child(
            text(caption)
                .wrap()
                .font(theme::scale().small)
                .color(kit.c.text_muted),
        );
    }
    for reading in details.readings {
        let mut line = col()
            .w_full()
            .gap(2.0)
            .border_sides([1.0, 0.0, 0.0, 0.0], kit.c.border_base)
            .pt(5.0);
        let mut heading = row().w_full().gap(6.0).child(
            text(reading.label)
                .grow(1.0)
                .min_w(0.0)
                .wrap()
                .font(theme::scale().h5)
                .color(kit.c.text_base),
        );
        let m = reading.quantity;
        heading = heading.child(
            text(m.value)
                .font(theme::scale().small)
                .color(kit.c.text_base),
        );
        line = line.child(heading);
        if let Some((total, _)) = scale {
            line = line.child(
                row().w_full().h(3.0).fill(kit.c.layer_02).child(
                    el().h_full()
                        .w_frac((m.amount / total).clamp(0.0, 1.0) as f32)
                        .fill(kit.c.warning.icon),
                ),
            );
        }
        list = list.child(
            line.child(
                text(reading.detail)
                    .wrap()
                    .font(theme::scale().small)
                    .color(kit.c.text_muted),
            ),
        );
    }
    list
}
