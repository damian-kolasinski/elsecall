//! Window state, scenes and the rune app spec.

use std::sync::Arc;

use rune::{App, AppSpec, Chrome, Latest, Tx, Waker};
use rune_kit::TableState;

use crate::fleet_source::{
    FleetSnapshot, Gate, LoadAverages, MemberRow, MemoryUse, Reachability, Role, SourceFailure,
    Standing,
};
use crate::refresh::{self, Update};
use crate::release_source::{self, ReleaseSnapshot};
use crate::socket_source;
use crate::theme;
use crate::view;
use crate::window_upgrade::{self, UpgradeState, WindowUpgrade};
use elsecall::release_update::{Job, Phase as ReleasePhase, Status as ReleaseStatus};

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
    ("no-tailnet", scene_no_tailnet),
    ("members", scene_members),
    ("members-selected", scene_members_selected),
    ("upgrade", scene_upgrade),
    ("upgrading", scene_upgrading),
    ("upgrade-failed", scene_upgrade_failed),
    ("release-available", scene_release_available),
    ("release-loading", scene_release_loading),
    ("release-downloading", scene_release_downloading),
    ("release-waiting", scene_release_waiting),
    ("release-installing", scene_release_installing),
    ("release-reopening", scene_release_reopening),
    ("release-failed", scene_release_failed),
    ("release-offline", scene_release_offline),
    ("release-future", scene_release_future),
    ("demo-airdrop", scene_demo_airdrop),
    ("add-mac-preparing", scene_add_mac_preparing),
    ("add-mac-picker", scene_add_mac_picker),
    ("add-mac-blocked", scene_add_mac_blocked),
    ("add-mac-cancelled", scene_add_mac_cancelled),
    ("add-mac-failed", scene_add_mac_failed),
    ("add-mac-unconfirmed", scene_add_mac_unconfirmed),
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
    release_status: Option<ReleaseStatus>,
    release_feed: Arc<Latest<Result<ReleaseSnapshot, String>>>,
    release_start: Latest<Result<Job, String>>,
    opened_at_ms: u64,
    release_disconnected: bool,
    pub(crate) onboarding: crate::onboarding::Onboarding,
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
            || self.release_status.as_ref().is_some_and(|status| {
                status.available.is_some()
                    || status.job.as_ref().is_some_and(|job| !job.phase.terminal())
            })
            || self.updating()
    }

    pub(crate) fn updating(&self) -> bool {
        matches!(
            self.upgrade,
            UpgradeState::Installing
                | UpgradeState::StartingRelease
                | UpgradeState::Release(_)
                | UpgradeState::Retiring
        ) || self
            .release_status
            .as_ref()
            .and_then(|status| status.job.as_ref())
            .is_some_and(|job| !job.phase.terminal())
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
        if self
            .release_status
            .as_ref()
            .is_some_and(|status| status.available.is_some())
        {
            self.upgrade = UpgradeState::StartingRelease;
            self.release_start
                .request(Arc::clone(&self.window_upgrade), |upgrade| {
                    upgrade.start_release()
                });
        } else {
            self.upgrade = UpgradeState::Installing;
            self.upgrade_feed
                .request(Arc::clone(&self.window_upgrade), |upgrade| {
                    upgrade.replace_and_reopen()
                });
        }
    }

    pub(crate) fn release_notice(&self) -> Option<String> {
        let status = self.release_status.as_ref()?;
        if let Some(job) = &status.job
            && !job.phase.terminal()
        {
            return Some(format!("Updating to {}", job.release.version));
        }
        status
            .available
            .as_ref()
            .map(|release| format!("Version {} available", release.version))
    }
    pub(crate) fn upgrade_label(&self) -> &'static str {
        if self.release_disconnected && self.updating() {
            return "Reconnecting…";
        }
        if let Some(job) = self
            .release_status
            .as_ref()
            .and_then(|status| status.job.as_ref())
            && !job.phase.terminal()
        {
            return match job.phase {
                ReleasePhase::Starting | ReleasePhase::Downloading => "Downloading…",
                ReleasePhase::WaitingForWork => "Waiting for work…",
                ReleasePhase::Installing => "Installing…",
                ReleasePhase::SelectingCli => "Finishing update…",
                ReleasePhase::Reopening => "Reopening…",
                ReleasePhase::Restoring => "Restoring previous version…",
                ReleasePhase::Unknown => "Updating…",
                _ => unreachable!("active update phase"),
            };
        }
        if self.updating() {
            "Upgrading…"
        } else {
            "Upgrade and reopen"
        }
    }
    fn apply_release(&mut self, snapshot: Result<ReleaseSnapshot, String>) {
        let Ok(snapshot) = snapshot else {
            self.release_disconnected = true;
            return;
        };
        self.release_disconnected = false;
        self.installed_window = snapshot.installed_window;
        let mut incoming = snapshot.release;
        if let Some(previous) = self
            .release_status
            .as_ref()
            .and_then(|status| status.job.as_ref())
            && let Some(status) = &mut incoming
            && status
                .job
                .as_ref()
                .is_some_and(|job| job.started_at_ms < previous.started_at_ms)
        {
            status.job = Some(previous.clone());
        }
        self.release_status = incoming;
        if matches!(self.upgrade, UpgradeState::Retiring) {
            return;
        }
        if let Some(job) = self
            .release_status
            .as_ref()
            .and_then(|status| status.job.as_ref())
        {
            if !job.phase.terminal() && matches!(self.upgrade, UpgradeState::Failed(_)) {
                self.upgrade = UpgradeState::Idle;
            }
            let initiated = matches!(&self.upgrade,UpgradeState::Release(id) if id==&job.id);
            let in_this_window =
                initiated || (job.started_at_ms > 0 && job.started_at_ms >= self.opened_at_ms);
            match &job.phase {
                ReleasePhase::Succeeded if in_this_window && job.reopen => {
                    self.upgrade = UpgradeState::Retiring;
                    self.window_upgrade.close_old_window();
                }
                ReleasePhase::Succeeded if initiated => self.upgrade = UpgradeState::Idle,
                ReleasePhase::Failed { error, .. }
                    if in_this_window && !matches!(self.upgrade, UpgradeState::StartingRelease) =>
                {
                    self.upgrade = UpgradeState::Failed(format!("{} {}", error.message, error.hint))
                }
                _ => (),
            }
        }
    }

    pub(crate) fn table(state: &Self) -> &TableState {
        &state.table
    }

    pub(crate) fn table_mut(state: &mut Self) -> &mut TableState {
        &mut state.table
    }

    fn apply_refresh(&mut self, update: Update) {
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
        release_status: None,
        release_feed: Arc::new(Latest::default()),
        release_start: Latest::default(),
        opened_at_ms: elsecall::release_update::now_millis(),
        release_disconnected: false,
        onboarding: crate::onboarding::Onboarding::new(
            Arc::new(crate::onboarding_socket::SocketPreparation),
            Box::new(crate::airdrop::AirDrop::default()),
        ),
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
    state.upgrade_feed.set_waker(waker.clone());
    state.release_feed.set_waker(waker.clone());
    state.release_start.set_waker(waker.clone());
    state.onboarding.set_waker(waker);
    if state.follow_service {
        refresh::start(Arc::clone(&state.feed), socket_source::shared());
        release_source::start(
            Arc::clone(&state.release_feed),
            release_source::production(),
        );
    }
}

fn poll(state: &mut AppState, _: f64, tx: &mut Tx) -> bool {
    let mut changed = state.onboarding.poll();
    if state.onboarding.busy() {
        tx.wake_after(std::time::Duration::from_millis(100));
    }
    if let Some(update) = state.feed.take() {
        state.apply_refresh(update);
        changed = true;
    }
    if let Some(release) = state.release_feed.take() {
        state.apply_release(release);
        changed = true;
    }
    if let Some(result) = state.release_start.take() {
        if !matches!(state.upgrade, UpgradeState::Retiring) {
            match result {
                Ok(job) => {
                    state.upgrade = UpgradeState::Release(job.id.clone());
                    if let Some(status) = &mut state.release_status
                        && status
                            .job
                            .as_ref()
                            .is_none_or(|existing| existing.id != job.id)
                    {
                        status.job = Some(job);
                    }
                    state.apply_release(Ok(ReleaseSnapshot {
                        installed_window: state.installed_window.clone(),
                        release: state.release_status.clone(),
                    }));
                }
                Err(message) => state.upgrade = UpgradeState::Failed(message),
            }
        }
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

fn scene_no_tailnet(state: &mut AppState) {
    state.follow_service = false;
    state.refreshed = Some("14:32:05".into());
    state.phase = Phase::Ready(no_tailnet_snapshot());
}

fn scene_members(state: &mut AppState) {
    state.follow_service = false;
    state.refreshed = Some("14:32:05".into());
    state.phase = Phase::Ready(members_snapshot());
}

/// The last row selected, so its highlight meets the card's border and bottom corners.
fn scene_members_selected(state: &mut AppState) {
    scene_members(state);
    state.table.select("builder");
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

fn release_fixture(phase: Option<ReleasePhase>) -> ReleaseSnapshot {
    use elsecall::release_update::{JobId, Release, ReleaseVersion};
    let release = Release {
        version: ReleaseVersion::try_from("0.1.1".to_owned()).expect("fixture version"),
    };
    let job = phase.map(|phase| Job {
        id: JobId::try_from("a".repeat(32)).expect("fixture job"),
        release: release.clone(),
        from_build: "1111111111111111".parse().expect("fixture build"),
        started_at_ms: 20,
        reopen: true,
        wait_seconds: 300,
        phase,
        updated_at: 1700000000,
    });
    ReleaseSnapshot {
        installed_window: Some("1111111111111111".into()),
        release: Some(ReleaseStatus {
            current_version: ReleaseVersion::try_from("0.1.0".to_owned()).expect("fixture current"),
            available: Some(release),
            checked_at: Some(1700000000),
            check_error: None,
            job,
        }),
    }
}
fn scene_release_available(state: &mut AppState) {
    scene_members(state);
    state.self_build = Some("1111111111111111".into());
    state.opened_at_ms = 1;
    state.apply_release(Ok(release_fixture(None)));
}
fn scene_release_phase(state: &mut AppState, phase: ReleasePhase) {
    scene_release_available(state);
    state.apply_release(Ok(release_fixture(Some(phase))));
}
fn scene_release_loading(state: &mut AppState) {
    scene_release_available(state);
    state.phase = Phase::Loading;
    state.refreshed = None;
}
fn scene_release_downloading(state: &mut AppState) {
    scene_release_phase(state, ReleasePhase::Downloading);
}
fn scene_release_waiting(state: &mut AppState) {
    scene_release_phase(state, ReleasePhase::WaitingForWork);
}
fn scene_release_installing(state: &mut AppState) {
    scene_release_phase(state, ReleasePhase::Installing);
}
fn scene_release_reopening(state: &mut AppState) {
    scene_release_phase(state, ReleasePhase::Reopening);
}
fn scene_release_future(state: &mut AppState) {
    scene_release_phase(state, ReleasePhase::Unknown);
}
fn scene_release_failed(state: &mut AppState) {
    scene_release_phase(
        state,
        ReleasePhase::Failed {
            error: elsecall::failure::Failure::new(
                "release_tool_failed",
                "The release could not be verified.",
                "Check connectivity and retry the update.",
            ),
            rolled_back: false,
        },
    );
}
fn scene_release_offline(state: &mut AppState) {
    scene_release_available(state);
    state
        .release_status
        .as_mut()
        .expect("release scene")
        .check_error = Some(elsecall::failure::Failure::new(
        "release_tool_failed",
        "The release server could not be reached.",
        "The cached offer remains available.",
    ));
}

fn scene_demo_airdrop(state: &mut AppState) {
    scene_solo(state);
    state.onboarding = crate::onboarding_demo::flow();
}

fn scene_add_mac_preparing(state: &mut AppState) {
    scene_members(state);
    state.onboarding.state = crate::onboarding::State::Preparing;
}
fn scene_add_mac_picker(state: &mut AppState) {
    scene_demo_airdrop(state);
    state.onboarding.state = crate::onboarding::State::ChoosingRecipient;
}
fn scene_add_mac_blocked(state: &mut AppState) {
    scene_members(state);
    state.onboarding.state =
        crate::onboarding::State::Failed(crate::onboarding::Problem::InstallerUnavailable);
}
fn scene_add_mac_cancelled(state: &mut AppState) {
    scene_demo_airdrop(state);
    state.onboarding.state = crate::onboarding::State::Cancelled;
}
fn scene_add_mac_failed(state: &mut AppState) {
    scene_demo_airdrop(state);
    state.onboarding.state =
        crate::onboarding::State::Failed(crate::onboarding::Problem::TransferFailed);
}
fn scene_add_mac_unconfirmed(state: &mut AppState) {
    scene_demo_airdrop(state);
    state.onboarding.state = crate::onboarding::State::DeliveryUnconfirmed;
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
        standing: Standing::Rostered,
    }
}

/// A fresh Mac whose Tailscale is not running: no presence, so no roster.
fn no_tailnet_snapshot() -> FleetSnapshot {
    FleetSnapshot {
        enrolled: false,
        members: Vec::new(),
        standing: Standing::OffTailnet {
            hint: "Make sure Tailscale is running and logged in on this machine; the service retries every 30 seconds.".into(),
        },
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
                    one: 9.4,
                    five: 11.8,
                    fifteen: 14.2,
                }),
            ),
        ],
        standing: Standing::Rostered,
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
#[path = "onboarding_flow_tests.rs"]
mod onboarding_flow_tests;

#[cfg(test)]
mod tests {
    use super::*;

    struct Desktop {
        reply: std::sync::Mutex<std::sync::mpsc::Receiver<Result<(), String>>>,
        requests: std::sync::atomic::AtomicUsize,
        closed: std::sync::atomic::AtomicBool,
    }
    impl WindowUpgrade for Desktop {
        fn start_release(&self) -> Result<Job, String> {
            Err("No remote release in this staged-window test.".into())
        }
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
        rt.state.installed_window = Some("2222222222222222".into());
        rt.state.apply_refresh(Update {
            result: Ok(members_snapshot()),
            refreshed_at: "14:32:07".into(),
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
        rt.state.installed_window = Some("2222222222222222".into());
        rt.state.apply_refresh(Update {
            result: Ok(members_snapshot()),
            refreshed_at: "14:32:09".into(),
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

    struct ReleaseDesktop {
        reply: std::sync::Mutex<std::sync::mpsc::Receiver<Result<Job, String>>>,
        starts: std::sync::atomic::AtomicUsize,
        closes: std::sync::atomic::AtomicUsize,
    }
    impl WindowUpgrade for ReleaseDesktop {
        fn replace_and_reopen(&self) -> Result<(), String> {
            panic!("a remote offer must install the full release")
        }
        fn start_release(&self) -> Result<Job, String> {
            self.starts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.reply.lock().unwrap().recv().unwrap()
        }
        fn close_old_window(&self) {
            self.closes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    #[test]
    fn a_release_offer_is_visible_while_the_fleet_is_loading() {
        let _guard = theme::serial_render();
        let mut state = init();
        scene_release_loading(&mut state);
        let mut rt = rune::Runtime::new(
            state,
            view::fleet,
            (1120.0, 560.0),
            rune::Env::new(false, 1.0),
        );
        rt.frame(0.0);
        assert!(rt.presented("upgrade-window").is_some());
        assert!(
            rt.elements()
                .iter()
                .any(|element| element.label.as_deref() == Some("Upgrade and reopen"))
        );
        assert!(matches!(rt.state.phase, Phase::Loading));
    }
    #[test]
    fn release_button_installs_once_shows_progress_keeps_errors_and_retires_only_after_success() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let _guard = theme::serial_render();
        let (reply, receiver) = std::sync::mpsc::channel();
        let desktop = Arc::new(ReleaseDesktop {
            reply: std::sync::Mutex::new(receiver),
            starts: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
        });
        let mut state = init();
        scene_release_available(&mut state);
        state.window_upgrade = desktop.clone();
        let (wake, ready) = std::sync::mpsc::channel();
        state.release_start.set_waker(Arc::new(move || {
            wake.send(()).unwrap();
        }));
        let mut rt = rune::Runtime::new(
            state,
            view::fleet,
            (1120.0, 560.0),
            rune::Env::new(false, 1.0),
        );
        rt.frame(0.0);
        click_upgrade(&mut rt);
        click_upgrade(&mut rt);
        assert!(rt.state.updating());
        let first = release_fixture(Some(ReleasePhase::Starting))
            .release
            .unwrap()
            .job
            .unwrap();
        reply.send(Ok(first.clone())).unwrap();
        ready
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        poll(&mut rt.state, 0.0, &mut Tx::default());
        assert_eq!(desktop.starts.load(Ordering::SeqCst), 1);
        rt.state
            .apply_release(Ok(release_fixture(Some(ReleasePhase::Downloading))));
        assert_eq!(rt.state.upgrade_label(), "Downloading…");
        rt.state
            .apply_release(Ok(release_fixture(Some(ReleasePhase::Unknown))));
        assert_eq!(rt.state.upgrade_label(), "Updating…");
        assert_eq!(desktop.closes.load(Ordering::SeqCst), 0);
        rt.state
            .apply_release(Err("The service is restarting.".into()));
        assert_eq!(rt.state.upgrade_label(), "Reconnecting…");
        assert_eq!(rt.state.rows().len(), 3);
        rt.state
            .apply_release(Ok(release_fixture(Some(ReleasePhase::Failed {
                error: elsecall::release_update::invalid("The release could not be verified."),
                rolled_back: false,
            }))));
        assert!(!rt.state.updating());
        assert!(
            rt.state
                .update_error()
                .unwrap()
                .contains("could not be verified")
        );
        assert_eq!(desktop.closes.load(Ordering::SeqCst), 0);
        rt.invalidate();
        rt.frame(0.0);
        click_upgrade(&mut rt);
        let mut second = first.clone();
        second.id = elsecall::release_update::JobId::try_from("b".repeat(32)).unwrap();
        second.started_at_ms = 30;
        reply.send(Ok(second.clone())).unwrap();
        ready
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        poll(&mut rt.state, 0.0, &mut Tx::default());
        assert_eq!(desktop.starts.load(Ordering::SeqCst), 2);
        // An old poll arriving after the new request cannot put the previous failure back.
        rt.state
            .apply_release(Ok(release_fixture(Some(ReleasePhase::Failed {
                error: elsecall::release_update::invalid("old failure"),
                rolled_back: false,
            }))));
        assert!(rt.state.updating());
        assert!(rt.state.update_error().is_none());
        let mut snapshot = release_fixture(None);
        second.phase = ReleasePhase::Reopening;
        snapshot.release.as_mut().unwrap().job = Some(second.clone());
        rt.state.apply_release(Ok(snapshot));
        assert_eq!(rt.state.upgrade_label(), "Reopening…");
        assert_eq!(desktop.closes.load(Ordering::SeqCst), 0);
        let mut snapshot = release_fixture(None);
        second.phase = ReleasePhase::Succeeded;
        snapshot.release.as_mut().unwrap().job = Some(second.clone());
        snapshot.release.as_mut().unwrap().available = None;
        rt.state.apply_release(Ok(snapshot));
        assert_eq!(desktop.closes.load(Ordering::SeqCst), 1);
        assert!(matches!(rt.state.upgrade, UpgradeState::Retiring));
        // The fresh window and a window opened after an old completed job stay open.
        let mut fresh = init();
        fresh.window_upgrade = desktop.clone();
        fresh.opened_at_ms = 40;
        let mut snapshot = release_fixture(None);
        snapshot.release.as_mut().unwrap().job = Some(second);
        fresh.apply_release(Ok(snapshot));
        assert_eq!(desktop.closes.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn an_error_keeps_the_last_snapshot_and_its_refresh_time() {
        let mut state = init();
        state.apply_refresh(Update {
            result: Ok(solo_snapshot()),
            refreshed_at: "14:32:05".into(),
        });
        state.apply_refresh(Update {
            result: Err(SourceFailure::ServiceUnavailable {
                message: "The local service could not be reached.".into(),
                hint: "Run elsecall install if absent, or elsecall service restart if installed."
                    .into(),
            }),
            refreshed_at: "14:32:07".into(),
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
        });
        assert!(state.shown().is_none());
        assert!(!state.stale());
        assert!(state.refreshed().is_none());
    }
}
