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
use crate::window_upgrade::{self, UpgradeState, WindowUpgrade};

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
    ("upgrade", scene_upgrade),
    ("upgrading", scene_upgrading),
    ("upgrade-failed", scene_upgrade_failed),
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
    upgrade: UpgradeState,
    upgrade_feed: Latest<Result<(), String>>,
    window_upgrade: Arc<dyn WindowUpgrade>,
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
        window_upgrade::available(self.installed_window.as_deref(), self.self_build.as_deref())
    }

    pub(crate) fn updating(&self) -> bool {
        matches!(self.upgrade, UpgradeState::Installing)
    }

    pub(crate) fn update_error(&self) -> Option<&str> {
        match &self.upgrade {
            UpgradeState::Failed(message) => Some(message),
            _ => None,
        }
    }

    pub(crate) fn press_update(&mut self) {
        if self.updating() || !self.update_available() {
            return;
        }
        self.upgrade = UpgradeState::Installing;
        self.upgrade_feed
            .request(Arc::clone(&self.window_upgrade), |upgrade| {
                upgrade.replace_and_reopen()
            });
    }

    pub(crate) fn table(state: &Self) -> &TableState {
        &state.table
    }

    pub(crate) fn table_mut(state: &mut Self) -> &mut TableState {
        &mut state.table
    }

    fn apply_refresh(&mut self, update: Update) {
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
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let mut ready = if args
        .first()
        .is_some_and(|arg| arg == elsecall::window_startup::READY_ARGUMENT)
    {
        if args.len() < 2 {
            eprintln!("elsecall-ui: missing startup socket");
            std::process::exit(1);
        }
        let socket = std::path::PathBuf::from(&args[1]);
        args.drain(..2);
        Some(socket)
    } else {
        None
    };
    let app = App::new(spec())
        .chrome(CHROME)
        .args(args)
        .on_ready(move |_| {
            if let Some(socket) = ready.take()
                && elsecall::window_startup::acknowledge(&socket).is_err()
            {
                // The launcher timed out and restored the previous bundle.
                std::process::exit(1);
            }
        });
    if let Err(error) = app.try_run() {
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
        upgrade: UpgradeState::Idle,
        upgrade_feed: Latest::default(),
        window_upgrade: Arc::new(socket_source::SocketWindowUpgrade),
    }
}

fn own_build() -> Option<String> {
    let path = std::env::current_exe().ok()?;
    elsecall::window_app::running_build(&path)
        .ok()
        .map(|build| build.to_string())
}

fn setup(state: &mut AppState, waker: Waker) {
    state.feed.set_waker(waker.clone());
    state.upgrade_feed.set_waker(waker);
    if state.follow_service {
        refresh::start(Arc::clone(&state.feed), socket_source::shared());
    }
}

fn poll(state: &mut AppState, _: f64, tx: &mut Tx) -> bool {
    let mut changed = false;
    if let Some(update) = state.feed.take() {
        state.apply_refresh(update);
        changed = true;
    }
    if let Some(result) = state.upgrade_feed.take() {
        match result {
            Ok(()) => state.window_upgrade.close_old_window(),
            Err(message) => state.upgrade = UpgradeState::Failed(message),
        }
        changed = true;
    }
    if changed {
        tx.instant();
    }
    changed
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

fn scene_upgrade(state: &mut AppState) {
    scene_members(state);
    state.self_build = Some("1111111111111111".into());
    state.installed_window = Some("2222222222222222".into());
}

fn scene_upgrading(state: &mut AppState) {
    scene_upgrade(state);
    state.upgrade = UpgradeState::Installing;
}

fn scene_upgrade_failed(state: &mut AppState) {
    scene_upgrade(state);
    state.upgrade = UpgradeState::Failed(
        "The fleet window could not be opened. Retry from a macOS desktop session.".into(),
    );
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

    struct Desktop {
        reply: std::sync::Mutex<std::sync::mpsc::Receiver<Result<(), String>>>,
        requests: std::sync::atomic::AtomicUsize,
        closed: std::sync::atomic::AtomicBool,
    }
    impl WindowUpgrade for Desktop {
        fn replace_and_reopen(&self) -> Result<(), String> {
            self.requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.reply.lock().unwrap().recv().unwrap()
        }
        fn close_old_window(&self) {
            self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn click_upgrade(rt: &mut rune::Runtime<AppState>) {
        let rect = rt.presented("upgrade-window").expect("upgrade button").0;
        let (x, y) = (rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0);
        rt.pointer_move(x, y);
        rt.frame(1.0 / 60.0);
        rt.pointer_down(x, y);
        rt.frame(1.0 / 60.0);
        rt.pointer_up(x, y);
        rt.frame(0.0);
    }

    #[test]
    fn clicking_upgrade_retries_errors_and_closes_only_after_a_successful_relaunch() {
        let _guard = theme::serial_render();
        let (reply, receiver) = std::sync::mpsc::channel();
        let desktop = Arc::new(Desktop {
            reply: std::sync::Mutex::new(receiver),
            requests: std::sync::atomic::AtomicUsize::new(0),
            closed: std::sync::atomic::AtomicBool::new(false),
        });
        let mut state = init();
        scene_members(&mut state);
        state.self_build = Some("1111111111111111".into());
        state.window_upgrade = desktop.clone();
        let mut rt = rune::Runtime::new(
            state,
            view::fleet,
            (1120.0, 560.0),
            rune::Env::new(false, 1.0),
        );
        rt.frame(0.0);
        assert!(rt.presented("upgrade-window").is_none());
        rt.state.apply_refresh(Update {
            result: Ok(members_snapshot()),
            refreshed_at: "14:32:07".into(),
            installed_window: Some("2222222222222222".into()),
        });
        rt.invalidate();
        rt.frame(0.0);
        click_upgrade(&mut rt);
        assert!(rt.state.updating());
        click_upgrade(&mut rt); // Repeated clicks cannot queue another installation.
        assert!(!desktop.closed.load(std::sync::atomic::Ordering::SeqCst));
        let (wake, ready) = std::sync::mpsc::channel();
        rt.state.upgrade_feed.set_waker(Arc::new(move || {
            wake.send(()).unwrap();
        }));
        reply
            .send(Err(
                "The fleet window could not be opened. Retry the upgrade.".into(),
            ))
            .unwrap();
        ready
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        // A fleet refresh arriving at the same time must not erase the error.
        rt.state.apply_refresh(Update {
            result: Ok(members_snapshot()),
            refreshed_at: "14:32:09".into(),
            installed_window: Some("2222222222222222".into()),
        });
        poll(&mut rt.state, 0.0, &mut Tx::default());
        assert!(!rt.state.updating());
        assert!(rt.state.update_error().unwrap().contains("Retry"));
        assert_eq!(rt.state.rows().len(), 3);
        assert!(!desktop.closed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            desktop.requests.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        rt.invalidate();
        rt.frame(0.0);
        click_upgrade(&mut rt);
        assert!(rt.state.update_error().is_none());
        reply.send(Ok(())).unwrap();
        ready
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        poll(&mut rt.state, 0.0, &mut Tx::default());
        assert!(desktop.closed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            desktop.requests.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
    }

    #[test]
    fn an_error_keeps_the_last_snapshot_and_its_refresh_time() {
        let mut state = init();
        state.apply_refresh(Update {
            result: Ok(solo_snapshot()),
            refreshed_at: "14:32:05".into(),
            installed_window: None,
        });
        state.apply_refresh(Update {
            result: Err(SourceFailure::ServiceUnavailable {
                message: "The local service could not be reached.".into(),
                hint: "Run elsecall install if absent, or elsecall service restart if installed."
                    .into(),
            }),
            refreshed_at: "14:32:07".into(),
            installed_window: None,
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
        });
        assert!(state.shown().is_none());
        assert!(!state.stale());
        assert!(state.refreshed().is_none());
    }
}
