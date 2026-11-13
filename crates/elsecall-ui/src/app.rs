//! Window state, scenes and the rune app spec.

use std::sync::Arc;

use rune::{App, AppSpec, Chrome, Latest, Tx, Waker};
use rune_kit::TableState;

use crate::fleet_source::{
    FleetSnapshot, Gate, LoadAverages, MemberRow, MemoryUse, Reachability, Role, SourceFailure,
};
use crate::refresh::{self, Update};
use crate::socket_source;
use crate::theme;
use crate::view;

const CHROME: Chrome = Chrome {
    full_size_content: false,
    traffic_lights: None,
    drag_regions: false,
};

type Scene = fn(&mut AppState);

const SCENES: &[(&str, Scene)] = &[
    ("loading", scene_loading),
    ("unavailable-first", scene_unavailable_first),
    ("unavailable", scene_unavailable),
    ("solo", scene_solo),
    ("members", scene_members),
];

pub(crate) enum Phase {
    Loading,
    Unavailable {
        failure: SourceFailure,
        last: Option<FleetSnapshot>,
    },
    Ready(FleetSnapshot),
}

pub struct AppState {
    phase: Phase,
    refreshed: Option<String>,
    table: TableState,
    follow_service: bool,
    feed: Arc<Latest<Update>>,
    installed_window: Option<String>,
    self_build: Option<String>,
    updating: bool,
    update_error: Option<String>,
}

impl AppState {
    pub(crate) fn phase(&self) -> &Phase {
        &self.phase
    }

    pub(crate) fn shown(&self) -> Option<&FleetSnapshot> {
        match &self.phase {
            Phase::Ready(snapshot) => Some(snapshot),
            Phase::Unavailable { last, .. } => last.as_ref(),
            Phase::Loading => None,
        }
    }

    pub(crate) fn rows(&self) -> &[MemberRow] {
        self.shown()
            .map(|snapshot| snapshot.members.as_slice())
            .unwrap_or(&[])
    }

    pub(crate) fn failure(&self) -> Option<&SourceFailure> {
        match &self.phase {
            Phase::Unavailable { failure, .. } => Some(failure),
            _ => None,
        }
    }

    pub(crate) fn refreshed(&self) -> Option<&str> {
        self.refreshed.as_deref()
    }

    pub(crate) fn stale(&self) -> bool {
        matches!(self.phase, Phase::Unavailable { last: Some(_), .. })
    }

    pub(crate) fn update_available(&self) -> bool {
        view::update_available(self.installed_window.as_deref(), self.self_build.as_deref())
    }

    pub(crate) fn updating(&self) -> bool {
        self.updating
    }

    pub(crate) fn update_error(&self) -> Option<&str> {
        self.update_error.as_deref()
    }

    pub(crate) fn press_update(&mut self) {
        if self.updating || !self.update_available() {
            return;
        }
        self.updating = true;
        self.update_error = None;
        let feed = Arc::clone(&self.feed);
        std::thread::spawn(move || {
            let result = crate::socket_source::replace_window();
            if result.is_ok() {
                std::process::exit(0);
            }
            let message = result.unwrap_err();
            feed.request(message, |message| Update {
                result: Err(crate::fleet_source::SourceFailure::Rejected {
                    message: message.clone(),
                    hint: String::new(),
                }),
                refreshed_at: String::new(),
                installed_window: None,
                update_result: Some(Err(message)),
            });
        });
    }

    pub(crate) fn table(state: &Self) -> &TableState {
        &state.table
    }

    pub(crate) fn table_mut(state: &mut Self) -> &mut TableState {
        &mut state.table
    }

    fn apply_refresh(&mut self, update: Update) {
        if let Some(result) = update.update_result {
            self.updating = false;
            self.update_error = result.err();
            return;
        }
        self.installed_window = update.installed_window;
        match update.result {
            Ok(snapshot) => {
                self.refreshed = Some(update.refreshed_at);
                self.phase = Phase::Ready(snapshot);
            }
            Err(failure) => {
                let last = self.shown().cloned();
                self.phase = Phase::Unavailable { failure, last };
            }
        }
    }
}

pub fn run() {
    if let Err(error) = App::new(spec()).chrome(CHROME).try_run() {
        eprintln!("elsecall-ui: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
pub(crate) fn render_scene(argv: Vec<String>) -> Result<(), String> {
    App::new(spec()).chrome(CHROME).args(argv).try_run()
}

fn spec() -> AppSpec<AppState> {
    AppSpec {
        min_size: Some((1040.0, 480.0)),
        scenes: SCENES,
        fonts: theme::FONTS,
        families: theme::FAMILIES,
        setup: Some(setup),
        poll: Some(poll),
        ..AppSpec::new("Elsecall", (1120.0, 560.0), init, view::fleet)
    }
}

fn init() -> AppState {
    AppState {
        phase: Phase::Loading,
        refreshed: None,
        table: TableState::new(),
        follow_service: true,
        feed: Arc::new(Latest::default()),
        installed_window: None,
        self_build: own_build(),
        updating: false,
        update_error: None,
    }
}

fn own_build() -> Option<String> {
    let path = std::env::current_exe().ok()?;
    let digest = elsecall::build_identity::Digest::of_file(&path).ok()?;
    Some(digest.build_id().to_string())
}

fn setup(state: &mut AppState, waker: Waker) {
    state.feed.set_waker(waker);
    if state.follow_service {
        refresh::start(Arc::clone(&state.feed), socket_source::shared());
    }
}

fn poll(state: &mut AppState, _: f64, tx: &mut Tx) -> bool {
    let Some(update) = state.feed.take() else {
        return false;
    };
    state.apply_refresh(update);
    tx.instant();
    true
}

fn scene_loading(state: &mut AppState) {
    state.follow_service = false;
    state.phase = Phase::Loading;
    state.refreshed = None;
}

fn scene_unavailable_first(state: &mut AppState) {
    state.follow_service = false;
    state.refreshed = None;
    state.phase = Phase::Unavailable {
        failure: service_down(),
        last: None,
    };
}

fn scene_unavailable(state: &mut AppState) {
    state.follow_service = false;
    state.refreshed = Some("14:32:05".into());
    state.phase = Phase::Unavailable {
        failure: service_down(),
        last: Some(members_snapshot()),
    };
}

fn service_down() -> SourceFailure {
    SourceFailure::ServiceUnavailable {
        message: "The local service could not be reached.".into(),
        hint: "Run elsecall doctor; use elsecall install if absent, or elsecall service restart if installed.".into(),
    }
}

fn scene_solo(state: &mut AppState) {
    state.follow_service = false;
    state.refreshed = Some("14:32:05".into());
    state.phase = Phase::Ready(solo_snapshot());
}

fn scene_members(state: &mut AppState) {
    state.follow_service = false;
    state.refreshed = Some("14:32:05".into());
    state.phase = Phase::Ready(members_snapshot());
}

fn solo_snapshot() -> FleetSnapshot {
    FleetSnapshot {
        enrolled: false,
        members: vec![member(
            "mac-mini",
            Role::Master,
            Reachability::Reachable,
            Gate::Open,
            Some(0.18),
            Some(gigabytes(18, 64)),
            Some(LoadAverages {
                one: 1.4,
                five: 1.6,
                fifteen: 1.5,
            }),
        )],
    }
}

fn members_snapshot() -> FleetSnapshot {
    FleetSnapshot {
        enrolled: true,
        members: vec![
            member(
                "mac-mini",
                Role::Master,
                Reachability::Reachable,
                Gate::Open,
                Some(0.42),
                Some(gigabytes(36, 64)),
                Some(LoadAverages {
                    one: 8.2,
                    five: 7.4,
                    fifteen: 6.1,
                }),
            ),
            member(
                "macbook",
                Role::Member,
                Reachability::Unreachable,
                Gate::Open,
                None,
                None,
                None,
            ),
            member(
                "builder",
                Role::Member,
                Reachability::Reachable,
                Gate::Blocked {
                    leading_process: Some("Xcode".into()),
                },
                Some(0.91),
                Some(gigabytes(52, 64)),
                Some(LoadAverages {
                    one: 14.2,
                    five: 11.8,
                    fifteen: 9.4,
                }),
            ),
        ],
    }
}

fn member(
    name: &str,
    role: Role,
    reach: Reachability,
    gate: Gate,
    cpu: Option<f64>,
    memory: Option<MemoryUse>,
    load: Option<LoadAverages>,
) -> MemberRow {
    MemberRow {
        name: name.into(),
        role,
        reach,
        gate,
        cpu,
        memory,
        load,
    }
}

fn gigabytes(used: u64, total: u64) -> MemoryUse {
    MemoryUse {
        used_bytes: used << 30,
        total_bytes: total << 30,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_keeps_the_last_snapshot_and_its_refresh_time() {
        let mut state = init();
        state.apply_refresh(Update {
            result: Ok(solo_snapshot()),
            refreshed_at: "14:32:05".into(),
            installed_window: None,
            update_result: None,
        });
        state.apply_refresh(Update {
            result: Err(SourceFailure::ServiceUnavailable {
                message: "The local service could not be reached.".into(),
                hint: "Run elsecall install if absent, or elsecall service restart if installed."
                    .into(),
            }),
            refreshed_at: "14:32:07".into(),
            installed_window: None,
            update_result: None,
        });
        assert!(state.stale());
        assert_eq!(state.rows().len(), 1);
        assert_eq!(state.rows()[0].name, "mac-mini");
        assert_eq!(state.refreshed(), Some("14:32:05"));
        let failure = state.failure().unwrap();
        assert!(failure.hint().contains("elsecall install"));
        assert!(failure.hint().contains("elsecall service restart"));
    }

    #[test]
    fn the_first_error_has_nothing_to_grey() {
        let mut state = init();
        state.apply_refresh(Update {
            result: Err(SourceFailure::Rejected {
                message: "The fleet response could not be read.".into(),
                hint: "Reinstall matching CLI and service versions with elsecall install.".into(),
            }),
            refreshed_at: "14:32:05".into(),
            installed_window: None,
            update_result: None,
        });
        assert!(state.shown().is_none());
        assert!(!state.stale());
        assert!(state.refreshed().is_none());
    }
}
