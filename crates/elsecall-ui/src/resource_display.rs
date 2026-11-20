//! Machine CPU and memory meters and the row action for their breakdown.
use crate::{
    app::AppState,
    fleet_source::{MemberRow, Reachability},
    resource_details::Metric,
    theme,
};
use rune::{col, el, row, text};
use rune_kit::{Button, ButtonVariant, Cell, ControlSize, Kit};

pub(crate) fn member_cell(member: &MemberRow, kit: &Kit) -> Cell {
    if member.reach == Reachability::Unreachable {
        return Cell::text(member.name.clone()).mono();
    }
    let member_name = member.name.clone();
    let action = Button::new("Load details")
        .variant(ButtonVariant::Ghost)
        .control_size(ControlSize::Mini)
        .on_press(move |state: &mut AppState| state.load_member = Some(member_name.clone()))
        .build(kit)
        .key(format!("load-details:{}", member.name))
        .label(format!("Load details for {}", member.name));
    Cell::el(
        col()
            .items_start()
            .justify_center()
            .child(
                text(member.name.clone())
                    .font(theme::scale().mono)
                    .color(kit.c.text_base),
            )
            .child(
                row()
                    .gap(2.0)
                    .child(action)
                    .child(crate::provisioning_view::setup_button(member, kit)),
            ),
    )
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
    Cell::el(
        col()
            .w_full()
            .center_y()
            .gap(4.0)
            .child(
                text(label)
                    .font(theme::scale().small)
                    .color(kit.c.text_base),
            )
            .child(meter),
    )
}
