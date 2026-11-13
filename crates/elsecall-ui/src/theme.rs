//! Mergegate colour and type, mapped onto a rune [`Kit`](rune_kit::Kit).
//!
//! Hex values are the colours `appearance/tokens.json` resolves to in
//! `ui/design-system/appearance.css` (`:root` for light, `prefers-color-scheme: dark`
//! for dark). Spacing and radii are from `ui/design-system/tokens.css`
//! (`--space-*` 4/8/12/16/24/32, `--radius-small` 4, `--radius-pill` 999,
//! `--control-height` 32). `--text-on-fill` is `var(--background-page)`, so dark
//! mode resolves it to the dark page colour.
//!
//! | Kit field | Mergegate token | Light | Dark |
//! | --- | --- | --- | --- |
//! | `page`, `bg_base`, `bg_deep` | `--background-page` | `#f7fcff` | `#222526` |
//! | `layer_01`, `surface_base`, `surface_weak`, `surface_raised_base` | `--background-subtle` | `#ebeff2` | `#3a3d40` |
//! | `layer_02` | `--background-secondary` | `#d2d6d9` | `#525659` |
//! | `hover`, `surface_hover` | `--background-hover` | `#e6ebef` | `#2f3235` |
//! | `bg_accent`, `info.icon` | `--background-brand` | `#003b66` | `#66bfff` |
//! | `border_focus`, `border_active`, `info.border` | `--border-brand` | `#178fe5` | `#66bfff` |
//! | `info.bg` | `--background-tint-brand` | `#e5f4ff` | `#052033` |
//! | `info.fg` | `--text-tint-brand` | `#003b66` | `#99d4ff` |
//! | `border_base`, `border_weak`, `border_muted`, `std_border` | `--border-container` | `#9da2a6` | `#84898c` |
//! | `border_strong` | `--border-strong` | `#84898c` | `#84898c` |
//! | `text_base`, `text_strong`, `std_text`, `icon_base`, `std_icon` | `--text-primary` | `#3a3d40` | `#ebeff2` |
//! | `text_muted`, `text_weak`, `icon_muted` | `--text-secondary` | `#525659` | `#b8bcbf` |
//! | `text_faint`, `icon_faint`, `icon_weaker` | `--text-subtle` | `#6b6f73` | `#9da2a6` |
//! | `text_accent`, `icon_accent` | `--text-brand` | `#003b66` | `#e5f4ff` |
//! | `text_contrast`, `text_inverse`, `text_invert_strong`, `submit_fg`, `submit_icon` | `--text-on-fill` | `#f7fcff` | `#222526` |
//! | `success.bg` | `--background-tint-success` | `#d8e5cf` | `#102600` |
//! | `success.fg` | `--text-tint-success` | `#264013` | `#aecc99` |
//! | `success.border` | `--border-success` | `#93bf73` | `#384d29` |
//! | `success.icon` | `--background-success` | `#384d29` | `#aecc99` |
//! | `warning.bg` | `--background-tint-warning` | `#fbf1dc` | `#3a2f14` |
//! | `warning.fg` | `--text-tint-warning` | `#4d390b` | `#f2c055` |
//! | `warning.border` | `--border-warning` | `#d9a11e` | `#664700` |
//! | `warning.icon` | `--background-warning` | `#805f12` | `#f2c055` |
//! | `danger.bg` | `--background-tint-error` | `#ffe5e5` | `#330808` |
//! | `danger.fg` | `--text-tint-error` | `#732222` | `#ffb3b3` |
//! | `danger.border` | `--border-error` | `#ff6666` | `#732222` |
//! | `danger.icon` | `--background-error` | `#732222` | `#ff6666` |
//! | table selection | `--background-selected` | `#cceaff` | `#16374e` |
//!
//! The master badge uses `info` (brand tint). Status badges use `success`, `warning`
//! and `danger` tint fills with their tint text. Other `Kit` fields stay on
//! [`Kit::neutral`](rune_kit::Kit::neutral).
//!
//! Type, from `appearance/tokens.json`: h3 600 19/24, h4 600 16/20, h5 600 14/16,
//! p3 400 14/20, p4 400 12/16, label 500 14/14, mono 400 13/20. Heading tracking
//! is −0.03em and label tracking is +0.03em. Rune's `TextStyle::tracking` is in
//! points, so each style stores `size * em`. Mono is [`Family::SystemMono`](rune::tok::Family::SystemMono).

use rune::Color;
use rune::tok::{Family, TextStyle};
use rune_kit::{Kit, StateColors, TableStyle};

/// DM Sans variable font (mergegate `ios/Resources/DMSans.ttf`) and its family name.
pub const DM_SANS: &[u8] = include_bytes!("../assets/fonts/DMSans.ttf");
pub const FONTS: &[&[u8]] = &[DM_SANS];
pub const FAMILIES: (&str, &str) = ("DM Sans", "ui-monospace");

/// `--space-1` in `ui/design-system/tokens.css`.
pub const SPACE_1: f32 = 4.0;
/// `--space-2`.
pub const SPACE_2: f32 = 8.0;
/// `--space-3`.
pub const SPACE_3: f32 = 12.0;
/// `--space-4`.
pub const SPACE_4: f32 = 16.0;
/// `--space-6`.
pub const SPACE_6: f32 = 24.0;
/// `--space-8`.
pub const SPACE_8: f32 = 32.0;
/// `--radius-small` in `appearance.css`.
pub const RADIUS_SMALL: f32 = 4.0;
/// `--radius-pill` in `tokens.css`.
pub const RADIUS_PILL: f32 = 999.0;
/// `--control-height` in `tokens.css`.
pub const CONTROL_HEIGHT: f32 = 32.0;

const HEADING_TRACKING: f32 = -0.03;
const LABEL_TRACKING: f32 = 0.03;

#[derive(Clone, Copy, Debug)]
pub struct TypeScale {
    pub h3: TextStyle,
    pub h4: TextStyle,
    pub h5: TextStyle,
    pub body: TextStyle,
    pub small: TextStyle,
    pub label: TextStyle,
    pub mono: TextStyle,
}

struct Ink {
    page: &'static str,
    subtle: &'static str,
    secondary: &'static str,
    hover: &'static str,
    selected: &'static str,
    brand: &'static str,
    brand_border: &'static str,
    brand_tint: &'static str,
    brand_tint_text: &'static str,
    border: &'static str,
    border_strong: &'static str,
    text: &'static str,
    text_secondary: &'static str,
    text_subtle: &'static str,
    text_brand: &'static str,
    on_fill: &'static str,
    success: &'static str,
    success_tint: &'static str,
    success_text: &'static str,
    success_border: &'static str,
    warning: &'static str,
    warning_tint: &'static str,
    warning_text: &'static str,
    warning_border: &'static str,
    danger: &'static str,
    danger_tint: &'static str,
    danger_text: &'static str,
    danger_border: &'static str,
}

/// Light column of the mapping table. Source: `appearance/tokens.json` via `appearance.css`.
const LIGHT: Ink = Ink {
    page: "#f7fcff",
    subtle: "#ebeff2",
    secondary: "#d2d6d9",
    hover: "#e6ebef",
    selected: "#cceaff",
    brand: "#003b66",
    brand_border: "#178fe5",
    brand_tint: "#e5f4ff",
    brand_tint_text: "#003b66",
    border: "#9da2a6",
    border_strong: "#84898c",
    text: "#3a3d40",
    text_secondary: "#525659",
    text_subtle: "#6b6f73",
    text_brand: "#003b66",
    on_fill: "#f7fcff",
    success: "#384d29",
    success_tint: "#d8e5cf",
    success_text: "#264013",
    success_border: "#93bf73",
    warning: "#805f12",
    warning_tint: "#fbf1dc",
    warning_text: "#4d390b",
    warning_border: "#d9a11e",
    danger: "#732222",
    danger_tint: "#ffe5e5",
    danger_text: "#732222",
    danger_border: "#ff6666",
};

/// Dark column of the mapping table. Source: `appearance/tokens.json` via `appearance.css`.
const DARK: Ink = Ink {
    page: "#222526",
    subtle: "#3a3d40",
    secondary: "#525659",
    hover: "#2f3235",
    selected: "#16374e",
    brand: "#66bfff",
    brand_border: "#66bfff",
    brand_tint: "#052033",
    brand_tint_text: "#99d4ff",
    border: "#84898c",
    border_strong: "#84898c",
    text: "#ebeff2",
    text_secondary: "#b8bcbf",
    text_subtle: "#9da2a6",
    text_brand: "#e5f4ff",
    on_fill: "#222526",
    success: "#aecc99",
    success_tint: "#102600",
    success_text: "#aecc99",
    success_border: "#384d29",
    warning: "#f2c055",
    warning_tint: "#3a2f14",
    warning_text: "#f2c055",
    warning_border: "#664700",
    danger: "#ff6666",
    danger_tint: "#330808",
    danger_text: "#ffb3b3",
    danger_border: "#732222",
};

pub fn scale() -> TypeScale {
    let sans = |size, weight, line, tracking_em| {
        TextStyle::new(size, weight)
            .lh(line)
            .tracking(size * tracking_em)
            .family(Family::Sans)
    };
    TypeScale {
        h3: sans(19.0, 600.0, 24.0, HEADING_TRACKING),
        h4: sans(16.0, 600.0, 20.0, HEADING_TRACKING),
        h5: sans(14.0, 600.0, 16.0, HEADING_TRACKING),
        body: sans(14.0, 400.0, 20.0, 0.0),
        small: sans(12.0, 400.0, 16.0, 0.0),
        label: sans(14.0, 500.0, 14.0, LABEL_TRACKING),
        mono: TextStyle::new(13.0, 400.0)
            .lh(20.0)
            .family(Family::SystemMono)
            .features("'tnum' 1"),
    }
}

pub fn kit(dark: bool) -> Kit {
    let mut kit = Kit::neutral(dark);
    apply(&mut kit, if dark { &DARK } else { &LIGHT });
    let fonts = scale();
    kit.ty.ui = fonts.body;
    kit.ty.ui_medium = fonts.h4;
    kit.ty.ui_compact = fonts.body;
    kit.ty.small = fonts.small;
    kit.ty.small_medium = fonts.label;
    kit.ty.body = fonts.body;
    kit.ty.body_medium = fonts.h5;
    kit.ty.text_small = fonts.small;
    kit.ty.text_small_medium = fonts.label;
    kit.ty.text_xsmall = fonts.small;
    kit.ty.text_xsmall_medium = fonts.label;
    kit.ty.mono = fonts.mono;
    kit.radius.xs = RADIUS_SMALL;
    kit.radius.sm = RADIUS_SMALL;
    kit
}

pub fn table_style(dark: bool) -> TableStyle {
    let ink = if dark { &DARK } else { &LIGHT };
    let fonts = scale();
    TableStyle {
        row_height: 44.0,
        header_height: CONTROL_HEIGHT,
        cell_pad: SPACE_3,
        font: fonts.body,
        mono: fonts.mono,
        header_font: fonts.label,
        text: paint(ink.text),
        muted: paint(ink.text_secondary),
        header_text: paint(ink.text_subtle),
        rule: paint(ink.border),
        selected: paint(ink.selected),
        stripe: paint(ink.hover),
    }
}

fn apply(kit: &mut Kit, ink: &Ink) {
    let page = paint(ink.page);
    let subtle = paint(ink.subtle);
    let text = paint(ink.text);
    let secondary = paint(ink.text_secondary);
    let subtle_text = paint(ink.text_subtle);
    let on_fill = paint(ink.on_fill);
    let border = paint(ink.border);
    let brand = paint(ink.brand);
    kit.c.page = page;
    kit.c.bg_base = page;
    kit.c.bg_deep = page;
    kit.c.layer_01 = subtle;
    kit.c.surface_base = subtle;
    kit.c.surface_weak = subtle;
    kit.c.surface_raised_base = subtle;
    kit.c.layer_02 = paint(ink.secondary);
    kit.c.hover = paint(ink.hover);
    kit.c.surface_hover = paint(ink.hover);
    kit.c.bg_accent = brand;
    kit.c.border_focus = paint(ink.brand_border);
    kit.c.border_active = paint(ink.brand_border);
    kit.c.border_base = border;
    kit.c.border_weak = border;
    kit.c.border_muted = border;
    kit.c.std_border = border;
    kit.c.border_strong = paint(ink.border_strong);
    kit.c.text_base = text;
    kit.c.text_strong = text;
    kit.c.std_text = text;
    kit.c.icon_base = text;
    kit.c.std_icon = text;
    kit.c.text_muted = secondary;
    kit.c.text_weak = secondary;
    kit.c.icon_muted = secondary;
    kit.c.text_faint = subtle_text;
    kit.c.icon_faint = subtle_text;
    kit.c.icon_weaker = subtle_text;
    kit.c.text_accent = paint(ink.text_brand);
    kit.c.icon_accent = paint(ink.text_brand);
    kit.c.text_contrast = on_fill;
    kit.c.text_inverse = on_fill;
    kit.c.text_invert_strong = on_fill;
    kit.c.submit_fg = on_fill;
    kit.c.submit_icon = on_fill;
    kit.c.info = StateColors {
        bg: paint(ink.brand_tint),
        fg: paint(ink.brand_tint_text),
        border: paint(ink.brand_border),
        icon: brand,
    };
    kit.c.success = StateColors {
        bg: paint(ink.success_tint),
        fg: paint(ink.success_text),
        border: paint(ink.success_border),
        icon: paint(ink.success),
    };
    kit.c.warning = StateColors {
        bg: paint(ink.warning_tint),
        fg: paint(ink.warning_text),
        border: paint(ink.warning_border),
        icon: paint(ink.warning),
    };
    kit.c.danger = StateColors {
        bg: paint(ink.danger_tint),
        fg: paint(ink.danger_text),
        border: paint(ink.danger_border),
        icon: paint(ink.danger),
    };
}

fn paint(hex: &str) -> Color {
    rune::color::hex(hex).unwrap_or_else(|| panic!("mergegate token {hex} is not a colour"))
}

#[cfg(test)]
pub(crate) fn serial_render() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_scale_matches_mergegate() {
        let fonts = scale();
        assert_eq!(
            (fonts.h3.size, fonts.h3.weight, fonts.h3.line_height),
            (19.0, 600.0, 24.0)
        );
        assert_eq!(
            (fonts.h4.size, fonts.h4.weight, fonts.h4.line_height),
            (16.0, 600.0, 20.0)
        );
        assert_eq!(
            (fonts.h5.size, fonts.h5.weight, fonts.h5.line_height),
            (14.0, 600.0, 16.0)
        );
        assert_eq!(
            (fonts.body.size, fonts.body.weight, fonts.body.line_height),
            (14.0, 400.0, 20.0)
        );
        assert_eq!(
            (
                fonts.small.size,
                fonts.small.weight,
                fonts.small.line_height
            ),
            (12.0, 400.0, 16.0)
        );
        assert_eq!(
            (
                fonts.label.size,
                fonts.label.weight,
                fonts.label.line_height
            ),
            (14.0, 500.0, 14.0)
        );
        assert_eq!(
            (fonts.mono.size, fonts.mono.weight, fonts.mono.line_height),
            (13.0, 400.0, 20.0)
        );
        assert!((fonts.h3.tracking - 19.0 * -0.03).abs() < 0.001);
        assert!((fonts.label.tracking - 14.0 * 0.03).abs() < 0.001);
    }

    #[test]
    fn dm_sans_variable_weights_differ() {
        let _guard = serial_render();
        let mut fonts = rune::text::TextSystem::new(FONTS, FAMILIES);
        let width = |fonts: &mut rune::text::TextSystem, weight: f32| {
            fonts
                .measure(
                    "Hamburgefonstiv",
                    TextStyle::new(32.0, weight).family(Family::Sans),
                    1.0,
                )
                .0
        };
        let regular = width(&mut fonts, 400.0);
        let middle = width(&mut fonts, 450.0);
        let bold = width(&mut fonts, 700.0);
        assert!(
            middle > regular + 0.5 && bold > middle + 0.5,
            "DM Sans did not follow the weight axis: 400 {regular}, 450 {middle}, 700 {bold}"
        );
    }
}
