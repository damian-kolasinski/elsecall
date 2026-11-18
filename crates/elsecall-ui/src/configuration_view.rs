//! The native workload configuration screen and its local read state.

use std::time::Duration;

use elsecall::scheduling::request::AtDeadline;
use elsecall::workload_configuration::FILE_NAME;
use elsecall::workload_settings::{LabelRule, RulePlacement, RuleUrgency, WhenUnavailable};
use rune::{El, Latest, ScrollSpec, TextEdit, Waker, col, el, row, text};
use rune_kit::{Button, ButtonVariant, Kit, SegmentedControl, TextInput};

use crate::{
    app::AppState,
    theme,
    workload_policy::{Policy, ReadError},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tab {
    Rules,
    Toml,
}

impl Tab {
    const ALL: [Self; 2] = [Self::Rules, Self::Toml];
}

#[derive(Default)]
pub(crate) struct ConfigurationView {
    tab: Option<Tab>,
    search: TextEdit,
    result: Option<Result<Policy, ReadError>>,
    reads: Latest<Result<Policy, ReadError>>,
}

impl ConfigurationView {
    pub(crate) fn visible(&self) -> bool {
        self.tab.is_some()
    }

    pub(crate) fn open(&mut self) {
        if self.tab.is_none() {
            self.tab = Some(Tab::Rules);
            self.reload();
        }
    }

    pub(crate) fn close(&mut self) {
        self.tab = None;
    }

    pub(crate) fn set_waker(&self, waker: Waker) {
        self.reads.set_waker(waker);
    }

    pub(crate) fn poll(&mut self) -> bool {
        if let Some(result) = self.reads.take() {
            self.result = Some(result);
            true
        } else {
            false
        }
    }

    fn reload(&mut self) {
        self.result = None;
        self.reads.request((), |_| Policy::personal());
    }

    pub(crate) fn scene(&mut self, tab: Tab, result: Result<Policy, ReadError>, query: &str) {
        self.tab = Some(tab);
        self.result = Some(result);
        self.search = TextEdit::new(query);
    }
}

pub(crate) fn render(state: &AppState, kit: &Kit) -> El {
    let config = &state.configuration;
    let tab = config
        .tab
        .expect("rendered only while configuration is visible");
    let toolbar = row()
        .w_full()
        .center_y()
        .gap(theme::SPACE_3)
        .child(
            text(FILE_NAME)
                .font(theme::scale().mono)
                .color(kit.c.text_base),
        )
        .child(el().grow(1.0))
        .child(
            Button::new("Reload")
                .variant(ButtonVariant::Ghost)
                .disabled(config.result.is_none())
                .on_press(|state: &mut AppState| state.configuration.reload())
                .build(kit)
                .key("configuration-reload"),
        );
    let tabs = SegmentedControl::new(&["Rules", "TOML"], Some(tab as usize))
        .label("Configuration view")
        .width(Some(172.0))
        .on_pick(|state: &mut AppState, index, _| state.configuration.tab = Some(Tab::ALL[index]))
        .build(kit)
        .key("configuration-tabs");
    let mut controls = row().w_full().center_y().gap(theme::SPACE_3).child(tabs);
    if tab == Tab::Rules
        && config.result.as_ref().is_some_and(|result| {
            result
                .as_ref()
                .is_ok_and(|policy| !policy.global.labels.is_empty())
        })
    {
        controls = controls.child(el().grow(1.0)).child(
            TextInput::new(&config.search)
                .label("Search rule patterns")
                .placeholder("Search rule patterns")
                .leading_icon(kit.glyph.search)
                .width(Some(260.0))
                .clear_button()
                .on_edit(|state: &mut AppState| &mut state.configuration.search)
                .build(kit)
                .key("configuration-search"),
        );
    }
    let content = match config.result.as_ref() {
        None => message("Reading workloads.toml…", kit),
        Some(Err(ReadError::Unreadable)) => message(
            "Couldn’t read workloads.toml. Check access and reload.",
            kit,
        ),
        Some(Err(ReadError::Invalid)) => {
            message("Invalid workloads.toml. Fix the file and reload.", kit)
        }
        Some(Ok(policy)) => match tab {
            Tab::Rules => rules(policy, &config.search.text, kit),
            Tab::Toml => source(policy, kit),
        },
    };
    let mut body = col().w_full().gap(theme::SPACE_3).child(content);
    if tab != Tab::Toml
        && config
            .result
            .as_ref()
            .and_then(|p| p.as_ref().ok())
            .is_some_and(|p| p.has_directory_policies)
    {
        body = body.child(small(
            "Global policy. See TOML for directory overrides.",
            kit,
        ));
    }
    col()
        .w_full()
        .h_full()
        .fill(kit.c.page)
        .padding(theme::SPACE_8)
        .child(
            col()
                .key("workload-configuration")
                .w_full()
                .max_w(760.0)
                .grow(1.0)
                .min_h(0.0)
                .gap(theme::SPACE_3)
                .child(toolbar)
                .child(controls)
                .child(
                    col()
                        .key(format!("configuration-scroll-{}", tab as usize))
                        .w_full()
                        .grow(1.0)
                        .min_h(0.0)
                        .scroll(ScrollSpec {
                            stick_bottom: false,
                        })
                        .child(body),
                ),
        )
}

fn panel(kit: &Kit) -> El {
    col()
        .w_full()
        .radius(theme::RADIUS_SMALL)
        .border(1.0, kit.c.border_base)
}

fn small(value: impl Into<String>, kit: &Kit) -> El {
    text(value)
        .font(theme::scale().small)
        .color(kit.c.text_muted)
}

fn message(value: &'static str, kit: &Kit) -> El {
    panel(kit).padding(theme::SPACE_3).child(small(value, kit))
}

fn matching_rules<'a>(rules: &'a [LabelRule], query: &str) -> impl Iterator<Item = &'a LabelRule> {
    let needle = query.trim().to_lowercase();
    rules
        .iter()
        .filter(move |rule| needle.is_empty() || rule.pattern.to_lowercase().contains(&needle))
}

fn rules(policy: &Policy, query: &str, kit: &Kit) -> El {
    let labels = &policy.global.labels;
    let mut matches = matching_rules(labels, query).peekable();
    let timed = labels
        .iter()
        .any(|rule| matches!(rule.urgency, Some(RuleUrgency::Deferrable { .. })));
    let placement = labels.iter().any(|rule| rule.placement.is_some());
    let mut view = panel(kit);
    if matches.peek().is_none() {
        let note = if policy.source.is_none() {
            "No workloads.toml. Using built-in defaults."
        } else if labels.is_empty() {
            "No global label rules."
        } else {
            "No rules match your search."
        };
        view = view.child(row().padding(theme::SPACE_3).child(small(note, kit)));
    } else {
        let heading = if timed {
            "Max wait / deadline"
        } else {
            "Max wait"
        };
        let mut header = table_row(kit, kit.c.layer_01)
            .child(small("Label", kit).role("columnheader").grow(1.0))
            .child(small("Weighted cores", kit).role("columnheader").w(112.0))
            .child(small(heading, kit).role("columnheader").w(146.0));
        if placement {
            header = header.child(small("Placement", kit).role("columnheader").w(90.0));
        }
        let mut table = col()
            .role("table")
            .label("Global label rules, in file order")
            .w_full()
            .child(header);
        for rule in matches {
            let timing = match rule.urgency {
                Some(RuleUrgency::Interactive {
                    hold_limit: Some(limit),
                }) => duration(limit),
                Some(RuleUrgency::Deferrable {
                    deadline,
                    at_deadline,
                }) => deadline_label(deadline, at_deadline),
                _ => "—".into(),
            };
            let mono = |value: String| {
                text(value)
                    .font(theme::scale().mono)
                    .color(kit.c.text_base)
                    .role("cell")
            };
            let mut line = table_row(kit, kit.c.page)
                .child(
                    mono(rule.pattern.clone())
                        .wrap_anywhere()
                        .grow(1.0)
                        .min_w(0.0),
                )
                .child(
                    mono(
                        rule.size
                            .map_or_else(|| "—".into(), |size| size.to_string()),
                    )
                    .w(112.0),
                )
                .child(mono(timing).wrap().w(146.0));
            if placement {
                line = line.child(
                    mono(
                        match rule.placement {
                            Some(RulePlacement::Pinned) => "Pinned",
                            Some(RulePlacement::Placeable) => "Placeable",
                            None => "—",
                        }
                        .into(),
                    )
                    .w(90.0),
                );
            }
            table = table.child(line);
        }
        view = view.child(table);
    }
    view.child(
        row()
            .w_full()
            .center_y()
            .padding(theme::SPACE_3)
            .gap(theme::SPACE_2)
            .child(small("If service is unavailable", kit))
            .child(
                text(match policy.global.when_unavailable.unwrap_or_default() {
                    WhenUnavailable::Fail => "Fail",
                    WhenUnavailable::Run => "Run unmanaged",
                })
                .font(theme::scale().small)
                .color(kit.c.text_base),
            ),
    )
}

fn table_row(kit: &Kit, fill: rune::Color) -> El {
    row()
        .role("row")
        .w_full()
        .center_y()
        .padding(theme::SPACE_3)
        .gap(theme::SPACE_3)
        .fill(fill)
        .border_sides([0.0, 0.0, 1.0, 0.0], kit.c.border_base)
}

fn source(policy: &Policy, kit: &Kit) -> El {
    match policy.source.as_deref() {
        None => message("No workloads.toml. Using built-in defaults.", kit),
        Some("") => message("workloads.toml is empty.", kit),
        Some(source) => {
            let preview = source_preview(source);
            let code = panel(kit)
                .fill(kit.c.layer_01)
                .padding(theme::SPACE_3)
                .child(
                    text(preview)
                        .font(theme::scale().mono)
                        .color(kit.c.text_base)
                        .w_full()
                        .wrap_anywhere()
                        .selectable(true),
                );
            if preview.len() == source.len() {
                code
            } else {
                col().w_full().gap(theme::SPACE_2).child(
                    small("Partial preview. Full file at ~/Library/Application Support/Elsecall/workloads.toml", kit)
                        .wrap().selectable(true),
                ).child(code)
            }
        }
    }
}

fn source_preview(source: &str) -> &str {
    // The policy parser accepts files much larger than should be shaped in one frame.
    const MAX_BYTES: usize = 8 * 1024;
    const MAX_LINES: usize = 240;
    let prefix = &source[..source.floor_char_boundary(MAX_BYTES)];
    let end = prefix
        .match_indices('\n')
        .nth(MAX_LINES - 1)
        .map_or(prefix.len(), |(end, _)| end + 1);
    &prefix[..end]
}

fn duration(value: Duration) -> String {
    let seconds = value.as_secs();
    if seconds >= 60 && seconds.is_multiple_of(60) && value.subsec_nanos() == 0 {
        format!("{}m", seconds / 60)
    } else {
        format!("{value:?}")
    }
}

fn deadline_label(deadline: Duration, action: AtDeadline) -> String {
    let action = match action {
        AtDeadline::Start => "start",
        AtDeadline::Skip => "skip",
    };
    format!("{} · {action}", duration(deadline))
}

#[cfg(test)]
mod tests {
    use super::{matching_rules, source_preview};
    use elsecall::workload_settings::WorkloadSettings;

    #[test]
    fn search_matches_literal_pattern_text_in_file_order() {
        let settings = WorkloadSettings::parse(
            r#"
[[labels]]
match = "acme test Checkout"
[[labels]]
match = "acme test *"
[[labels]]
match = "elsecall build"
"#,
        )
        .unwrap();
        let matches = |query| {
            matching_rules(&settings.labels, query)
                .map(|rule| rule.pattern.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(matches(" TEST "), ["acme test Checkout", "acme test *"]);
        assert_eq!(matches("*"), ["acme test *"]);
        assert_eq!(
            matches(""),
            ["acme test Checkout", "acme test *", "elsecall build"]
        );
        assert!(matches("unlisted").is_empty());
    }

    #[test]
    fn preview_bounds_shaping_even_for_wide_unicode_and_many_empty_lines() {
        assert_eq!(source_preview("[admission]\n"), "[admission]\n");
        let wide = "éö界".repeat(1600);
        let preview = source_preview(&wide);
        assert!(wide.starts_with(preview));
        assert!(preview.len() <= 8 * 1024);
        assert_eq!(source_preview(&"\n".repeat(1000)).len(), 240);
    }
}
