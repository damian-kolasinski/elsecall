//! Review a declared path's fleet-wide changes before enabling it.

use crate::{app::AppState, config_sync_source as source, theme};
use elsecall_fleet::config_sync_model::status::Change;
use rune::{El, Latest, TextEdit, Waker, col, el, row, text};
use rune_kit::{Button, ButtonVariant, Kit, TextInput};

#[derive(Default)]
pub(crate) struct SyncView {
    input: TextEdit,
    status: Option<source::Status>,
    plan: Option<source::Plan>,
    message: Option<String>,
    busy: bool,
    work: Latest<Result<Update, String>>,
}

enum Update {
    Status(source::Status),
    Review(source::Plan),
    Declared {
        status: source::Status,
        plan: Result<source::Plan, String>,
    },
    Enabled {
        complete: bool,
        status: Result<source::Status, String>,
    },
}

impl SyncView {
    pub(crate) fn scene(&mut self) {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/file-sync.json")).unwrap();
        self.status = Some(serde_json::from_value(fixture["status"].clone()).unwrap());
        self.plan = Some(serde_json::from_value(fixture["plan"].clone()).unwrap());
    }

    pub(crate) fn set_waker(&self, waker: Waker) {
        self.work.set_waker(waker);
    }

    pub(crate) fn poll(&mut self) -> bool {
        let Some(update) = self.work.take() else {
            return false;
        };
        self.busy = false;
        match update {
            Ok(Update::Status(status)) => self.status = Some(status),
            Ok(Update::Review(plan)) => self.plan = Some(plan),
            Ok(Update::Declared { status, plan }) => {
                self.status = Some(status);
                self.input = TextEdit::default();
                match plan {
                    Ok(plan) => self.plan = Some(plan),
                    Err(error) => self.message = Some(error),
                }
            }
            Ok(Update::Enabled { complete, status }) => {
                let mut message = if complete {
                    "Configuration delivered to all members.".to_owned()
                } else {
                    "Configuration sync is enabled. Delivery is pending on some members.".into()
                };
                match status {
                    Ok(status) => self.status = Some(status),
                    Err(error) => {
                        self.status = None;
                        message.push_str(&format!(" {error}"));
                    }
                }
                self.message = Some(message);
            }
            Err(error) => self.message = Some(error),
        }
        true
    }

    fn request(&mut self, load: impl FnOnce() -> Result<Update, String> + Send + 'static) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.plan = None;
        self.message = None;
        self.work.request((), move |_| load());
    }

    pub(crate) fn refresh(&mut self) {
        self.request(|| source::status().map(Update::Status));
    }

    fn review(&mut self, path: String) {
        self.request(move || source::review(&path).map(Update::Review));
    }

    fn declare(&mut self) {
        let path = self.input.text.trim().to_owned();
        if path.is_empty() {
            return;
        }
        self.request(move || {
            source::declare(&path)?;
            Ok(Update::Declared {
                status: source::status()?,
                plan: source::review(&path),
            })
        });
    }

    fn enable(&mut self) {
        let Some(plan) = &self.plan else { return };
        let Some(path) = plan
            .paths
            .first()
            .filter(|path| path.present && !path.enabled)
        else {
            return;
        };
        let (path, id) = (path.path.clone(), plan.plan_id.clone());
        self.request(move || {
            let complete = source::enable(&path, &id)?;
            Ok(Update::Enabled {
                complete,
                status: source::status(),
            })
        });
    }
}

pub(crate) fn busy(state: &AppState) -> bool {
    state.config_sync.busy
}

pub(crate) fn render(state: &AppState, kit: &Kit) -> El {
    let sync = &state.config_sync;
    let mut body = col().w_full().gap(theme::SPACE_3);
    if let Some(message) = &sync.message {
        body = body.child(note(message, kit));
    }
    if sync.busy {
        body = body.child(note("Working…", kit));
    }
    let Some(status) = &sync.status else {
        return body;
    };
    let Some(local) = status
        .members
        .iter()
        .find(|member| member.member == status.local_member)
    else {
        return body;
    };
    if local.member != status.master {
        return body.child(note(
            format!("Review configuration sync on {}.", status.master),
            kit,
        ));
    }
    if let Some(plan) = &sync.plan {
        return body
            .child(
                Button::new("Back to paths")
                    .variant(ButtonVariant::Ghost)
                    .on_press(|state: &mut AppState| state.config_sync.plan = None)
                    .build(kit),
            )
            .child(review(plan, kit));
    }
    body = body.child(
        row()
            .w_full()
            .gap(theme::SPACE_2)
            .center_y()
            .child(
                TextInput::new(&sync.input)
                    .label("Home-relative configuration path")
                    .placeholder("~/.config/")
                    .width(Some(320.0))
                    .disabled(sync.busy)
                    .on_edit(|state: &mut AppState| &mut state.config_sync.input)
                    .build(kit)
                    .key("file-sync-path"),
            )
            .child(
                Button::new("Declare and review")
                    .disabled(sync.busy || sync.input.text.trim().is_empty())
                    .on_press(|state: &mut AppState| state.config_sync.declare())
                    .build(kit)
                    .key("file-sync-declare"),
            ),
    );
    for (index, path) in status.declared_here.iter().enumerate() {
        let path_to_review = path.clone();
        let enabled = local
            .sync
            .as_ref()
            .is_some_and(|summary| summary.paths.iter().any(|entry| entry.path == *path));
        body = body.child(
            row()
                .w_full()
                .gap(theme::SPACE_2)
                .center_y()
                .child(text(path.clone()).font(theme::scale().mono).grow(1.0))
                .child(note(if enabled { "Enabled" } else { "Declared" }, kit))
                .child(
                    Button::new("Review")
                        .disabled(sync.busy)
                        .variant(ButtonVariant::Ghost)
                        .on_press(move |state: &mut AppState| {
                            state.config_sync.review(path_to_review.clone())
                        })
                        .build(kit)
                        .key(format!("file-sync-review-{index}")),
                ),
        );
    }
    if local
        .sync
        .as_ref()
        .is_some_and(|summary| !summary.paths.is_empty())
    {
        body = body.child(delivery(status, kit));
    }
    body
}

fn review(plan: &source::Plan, kit: &Kit) -> El {
    let Some(path) = plan.paths.first() else {
        return el();
    };
    let mut panel = col()
        .w_full()
        .gap(theme::SPACE_2)
        .padding(theme::SPACE_3)
        .radius(theme::RADIUS_SMALL)
        .border(1.0, kit.c.border_base)
        .child(text(format!("Review {}", path.path)).font(theme::scale().h5))
        .child(note(
            format!(
                "{} {} on the master",
                path.files,
                if path.files == 1 { "file" } else { "files" }
            ),
            kit,
        ));
    if !path.present {
        panel = panel.child(note(
            "Create the path on the master before enabling sync.",
            kit,
        ));
    }
    if !path.secret_looking.is_empty() {
        panel = panel.child(note(
            "Enabling this plan shares these secret-looking files with the listed members.",
            kit,
        ));
        for secret in &path.secret_looking {
            panel = panel.child(note(secret.file.clone(), kit));
        }
    }
    for skipped in &path.skipped {
        panel = panel.child(note(format!("Skipped on master · {skipped}"), kit));
    }
    for member in &plan.members {
        panel = panel.child(text(member.member.clone()).font(theme::scale().label));
        if let Some(error) = &member.error {
            panel = panel.child(note(error.message.clone(), kit));
        }
        for path in &member.paths {
            if let Some(error) = &path.error {
                panel = panel.child(note(error.message.clone(), kit));
            }
            panel = panel.child(note(format!("{} unchanged", path.unchanged), kit));
            for file in &path.files {
                let action = match file.change {
                    Change::Added => "Add",
                    Change::Changed | Change::LocallyEdited => "Replace",
                    Change::Removed => "Remove",
                    Change::ModeChanged => "Change permissions",
                    Change::Overridden => "Keep override",
                };
                let backup = if file.backup { " · Back up first" } else { "" };
                panel = panel.child(note(format!("{action} · {}{backup}", file.file), kit));
            }
        }
    }
    if !path.enabled {
        panel = panel.child(
            Button::new("Approve and enable sync")
                .disabled(!path.present)
                .on_press(|state: &mut AppState| state.config_sync.enable())
                .build(kit)
                .key("file-sync-enable"),
        );
    }
    panel
}

fn delivery(status: &source::Status, kit: &Kit) -> El {
    let mut view = col()
        .w_full()
        .gap(theme::SPACE_2)
        .child(text("File delivery").font(theme::scale().h5));
    for member in &status.members {
        view = view.child(text(member.member.clone()).font(theme::scale().label));
        if let Some(summary) = &member.sync {
            if summary.applying {
                view = view.child(note("Applying files…", kit));
            }
            for path in &summary.paths {
                let detail = if summary.paused.contains(&path.path) {
                    "Paused".to_owned()
                } else {
                    format!("{} differences", path.differences.len())
                };
                view = view.child(note(format!("{} · {detail}", path.path), kit));
            }
            if summary.service.pending {
                view = view.child(note("Service settings reload pending", kit));
            }
        } else {
            view = view.child(note("Status unavailable", kit));
        }
    }
    view
}

fn note(value: impl Into<String>, kit: &Kit) -> El {
    text(value)
        .font(theme::scale().small)
        .color(kit.c.text_muted)
        .wrap()
        .w_full()
}
