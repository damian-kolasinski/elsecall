//! Window state, scenes and the rune app spec.

mod add_mac_scenes;

use self::add_mac_scenes::*;
use elsecall_peers::roster::Readiness;
use std::num::NonZeroU32;
use std::sync::Arc;

use rune::{App, AppSpec, Chrome, Latest, MenuItem, Tx, Waker, col};
use rune_kit::{SegmentedControl, TableState, WindowTitlebar};

use crate::add_mac_flow::{Action, AddMac, AddRequest, Page as AddMacPage, RunUpdate};
use crate::add_mac_source::{self, AddMacSource, Inbox};
use crate::configuration_view::{self, ConfigurationView, Tab};
use crate::fleet_source::{
    FleetSnapshot, Gate, LoadAverages, MemberRow, MemoryUse, Reachability, Release, Role,
    SourceFailure, Standing,
};
use crate::refresh::{self, Update};
use crate::release_source::{self, ReleaseSnapshot};
use crate::socket_source;
use crate::theme;
use crate::uninstalled::{self, Bundle};
use crate::view;
use crate::window_upgrade::{self, UpgradeState, WindowUpgrade};
use crate::workload_policy::{Policy, ReadError};
use elsecall_lifecycle::release_update::{Job, Phase as ReleasePhase, Status as ReleaseStatus};

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
    ("resource-load", scene_resource_load),
    ("members-selected", scene_members_selected),
    (
        "members-awaiting-provisioning",
        scene_members_awaiting_provisioning,
    ),
    ("outdated-member", scene_outdated_member),
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
    ("release-fleet", scene_release_fleet),
    ("add-mac-prepare", scene_add_mac_prepare),
    ("add-mac-network", scene_add_mac_network),
    ("add-mac-looking", scene_add_mac_looking),
    ("add-mac-choose", scene_add_mac_choose),
    ("add-mac-sign-in", scene_add_mac_sign_in),
    ("add-mac-adding", scene_add_mac_adding),
    ("add-mac-approve", scene_add_mac_approve),
    ("add-mac-firewall", scene_add_mac_firewall),
    ("add-mac-firewall-failed", scene_add_mac_firewall_failed),
    ("add-mac-done", scene_add_mac_done),
    ("add-mac-failed", scene_add_mac_failed),
    ("configuration-rules", scene_configuration_rules),
    ("configuration-search", scene_configuration_search),
    ("configuration-no-matches", scene_configuration_no_matches),
    ("configuration-toml", scene_configuration_toml),
    ("configuration-missing", scene_configuration_missing),
    ("configuration-invalid", scene_configuration_invalid),
];

pub(crate) enum Phase {
    Loading,
    Unavailable {
        failure: SourceFailure,
        last: Option<FleetSnapshot>,
    },
    Ready(FleetSnapshot),
}

pub(crate) struct AppState {
    pub(crate) snapshot_at: elsecall_contract::scheduling_model::timestamp::Timestamp,
    pub(crate) load_member: Option<String>,
    pub(crate) configuration: ConfigurationView,
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
    bundle: Arc<dyn Bundle>,
    add_mac: AddMac,
    add_source: Arc<dyn AddMacSource>,
    nearby_feed: Arc<
        Latest<
            Result<
                Vec<elsecall_fleet::nearby_macs::NearbyMac>,
                elsecall_contract::failure::Failure,
            >,
        >,
    >,
    add_inbox: Arc<Inbox<RunUpdate>>,
    /// When the sheet last asked for the Macs nearby, on the window's clock.
    last_look: Option<f64>,
    /// The window's clock at the last poll, which drives the sheet's animations.
    clock: f64,
}

/// How often the sheet looks for Macs nearby while it lists them.
const LOOK_EVERY: f64 = 3.0;
/// Frames for the sheet's illustrations.
const ANIMATION_FRAME: std::time::Duration = std::time::Duration::from_millis(33);

impl AppState {
    fn open_configuration(&mut self, tx: &mut Tx) {
        if !self.add_mac.is_open() {
            self.load_member = None;
            self.configuration.open();
            tx.instant();
        }
    }

    pub(crate) fn add_mac(&self) -> &AddMac {
        &self.add_mac
    }

    pub(crate) fn add_mac_mut(&mut self) -> &mut AddMac {
        &mut self.add_mac
    }

    pub(crate) fn clock(&self) -> f64 {
        self.clock
    }

    pub(crate) fn open_add_mac(&mut self) {
        self.load_member = None;
        self.add_mac.open();
        self.last_look = None;
    }

    /// Closes the sheet, stopping an add that still runs.
    /// While an add runs, closing only stops it: the sheet stays to show how it
    /// ended.
    pub(crate) fn close_add_mac(&mut self) {
        self.add_mac.forget_sign_in();
        if self.add_mac.adding() {
            self.add_source.stop(self.add_mac.run());
            return;
        }
        self.add_mac.close();
    }

    pub(crate) fn stop_add_mac(&mut self) {
        self.add_mac.forget_sign_in();
        self.add_source.stop(self.add_mac.run());
    }

    /// The window is quitting: an add it started stops with it.
    fn quit_add_mac(&mut self) {
        if self.add_mac.adding() {
            self.add_source.stop(self.add_mac.run());
        }
    }

    pub(crate) fn start_add_mac(&mut self) {
        if let Some(request) = self.add_mac.start() {
            self.run_add_mac(request);
        }
    }

    pub(crate) fn retry_add_mac(&mut self) {
        if let Some(request) = self.add_mac.retry() {
            self.run_add_mac(request);
        }
    }

    /// Starts the add on a thread of its own; its updates arrive in the inbox.
    fn run_add_mac(&self, request: AddRequest) {
        let source = Arc::clone(&self.add_source);
        let inbox = Arc::clone(&self.add_inbox);
        let run = request.run;
        std::thread::spawn(move || {
            source.add(request, &|update| inbox.push(RunUpdate { run, update }));
        });
    }

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

    /// Whether this master is giving the members the build its last update installed.
    /// Only for the successful update to the release the service runs, so a record a
    /// restart or a later update left behind never shows as progress.
    pub(crate) fn fleet_updating(&self) -> bool {
        let Some(status) = self.release_status.as_ref() else {
            return false;
        };
        let (Some(job), Some(fleet)) = (status.job.as_ref(), status.fleet.as_ref()) else {
            return false;
        };
        fleet.job == job.id
            && matches!(job.phase, ReleasePhase::Succeeded)
            && job.release.version == status.current_version
            && matches!(
                fleet.state,
                elsecall_lifecycle::release_update::FollowUpState::Distributing { .. }
            )
    }
    pub(crate) fn release_notice(&self) -> Option<String> {
        let status = self.release_status.as_ref()?;
        if let Some(job) = &status.job
            && !job.phase.terminal()
        {
            return Some(format!("Updating to {}", job.release.version));
        }
        if self.fleet_updating() {
            return Some(format!("Updating the fleet to {}", status.current_version));
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
                self.snapshot_at = elsecall_contract::scheduling_model::timestamp::Timestamp::now();
                self.refreshed = Some(update.refreshed_at);
                if self.load_member.as_ref().is_some_and(|name| {
                    !snapshot
                        .members
                        .iter()
                        .any(|m| &m.name == name && m.reach == Reachability::Reachable)
                }) {
                    self.load_member = None;
                }
                self.add_mac
                    .know_members(snapshot.members.iter().map(|m| m.name.clone()));
                self.phase = Phase::Ready(snapshot);
            }
            Err(failure) => {
                // Uninstall stops the service before it removes the CLI link and the
                // app, so a window that cannot reach the service and finds both gone
                // was uninstalled. A service that answers with an error is still there.
                if matches!(failure, SourceFailure::ServiceUnavailable { .. })
                    && self.bundle.uninstalled()
                {
                    self.bundle.quit();
                }
                let last = self.shown().cloned();
                self.phase = Phase::Unavailable { failure, last };
            }
        }
    }
}

pub(crate) fn run() {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let mut ready = if args
        .first()
        .is_some_and(|arg| arg == elsecall_lifecycle::window_startup::READY_ARGUMENT)
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
    let app = window()
        .on_quit(|state, _| state.quit_add_mac())
        .args(args)
        .on_ready(move |_| {
            if let Some(socket) = ready.take()
                && elsecall_lifecycle::window_startup::acknowledge(&socket).is_err()
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
    window().args(argv).try_run()
}

fn window() -> App<AppState> {
    App::new(spec()).chrome(CHROME).menu_item(
        MenuItem::new("Configuration…", AppState::open_configuration)
            .id("workload-configuration")
            .key_equivalent("cmd+comma".parse().expect("known menu shortcut")),
    )
}

fn render(state: &AppState, cx: &mut rune::Cx<AppState>) -> rune::El {
    let kit = theme::kit(cx.system_dark());
    let navigation = SegmentedControl::new(
        &["Fleet", "Configuration"],
        Some(usize::from(state.configuration.visible())),
    )
    .label("Elsecall pages")
    .width(Some(244.0))
    .on_pick(|state: &mut AppState, index, tx| {
        if index == 0 {
            state.configuration.close();
            tx.instant();
        } else {
            state.open_configuration(tx);
        }
    })
    .build(&kit)
    .key("main-navigation");
    let toolbar = WindowTitlebar::from_insets(cx.chrome_insets())
        .leading_padding(theme::SPACE_8)
        .fill(kit.c.page)
        .leading(navigation)
        .build(&kit)
        .border_sides([0.0, 0.0, 1.0, 0.0], kit.c.border_base);
    let page = if state.configuration.visible() {
        configuration_view::render(state, &kit)
    } else {
        view::fleet(state, cx)
    };
    let base = col()
        .w_full()
        .h_full()
        .fill(kit.c.page)
        .child(toolbar)
        .child(col().w_full().grow(1.0).min_h(0.0).child(page));
    let mut overlays = Vec::new();
    if !state.configuration.visible() && !state.add_mac.is_open() {
        crate::load_dialog::overlay(state, &kit, &mut overlays);
    }
    crate::add_mac_view::sheet(state, &kit, cx.is_reduced_motion(), &mut overlays);
    rune_kit::root(base, overlays)
}

fn spec() -> AppSpec<AppState> {
    AppSpec {
        min_size: Some((1040.0, 480.0)),
        scenes: SCENES,
        fonts: theme::FONTS,
        families: theme::FAMILIES,
        setup: Some(setup),
        poll: Some(poll),
        ..AppSpec::new("Elsecall", (1120.0, 560.0), init, render)
    }
}

fn init() -> AppState {
    AppState {
        snapshot_at: elsecall_contract::scheduling_model::timestamp::Timestamp::now(),
        load_member: None,
        configuration: ConfigurationView::default(),
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
        opened_at_ms: elsecall_lifecycle::release_update::now_millis(),
        release_disconnected: false,
        bundle: Arc::new(uninstalled::RunningBundle::current()),
        add_mac: AddMac::default(),
        add_source: add_mac_source::production(),
        nearby_feed: Arc::new(Latest::default()),
        add_inbox: Arc::new(Inbox::default()),
        last_look: None,
        clock: 0.0,
    }
}

fn own_build() -> Option<String> {
    let path = std::env::current_exe().ok()?;
    elsecall_lifecycle::window_app::running_build(&path)
        .ok()
        .map(|build| build.to_string())
}

fn setup(state: &mut AppState, waker: Waker) {
    state.configuration.set_waker(waker.clone());
    state.feed.set_waker(waker.clone());
    state.upgrade_feed.set_waker(waker.clone());
    state.release_feed.set_waker(waker.clone());
    state.release_start.set_waker(waker.clone());
    state.nearby_feed.set_waker(waker.clone());
    state.add_inbox.set_waker(waker);
    if state.follow_service {
        refresh::start(Arc::clone(&state.feed), socket_source::shared());
        release_source::start(
            Arc::clone(&state.release_feed),
            release_source::production(),
        );
    }
}

fn poll(state: &mut AppState, now: f64, tx: &mut Tx) -> bool {
    let mut changed = poll_add_mac(state, now, tx);
    changed |= state.configuration.poll();
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

/// The Add Mac sheet's background work and animation frames.
fn poll_add_mac(state: &mut AppState, now: f64, tx: &mut Tx) -> bool {
    state.clock = now;
    let mut changed = false;
    for update in state.add_inbox.drain() {
        if let Some(link) = state.add_mac.apply(update) {
            let _ = tx.open_url(link.as_str());
        }
        changed = true;
    }
    if let Some(found) = state.nearby_feed.take() {
        state.add_mac.set_nearby(found);
        changed = true;
    }
    // Scenes show a fixed list rather than this network's.
    if state.follow_service
        && state.add_mac.looking()
        && !state.nearby_feed.is_pending()
        && state.last_look.is_none_or(|at| now - at >= LOOK_EVERY)
    {
        state.last_look = Some(now);
        state
            .nearby_feed
            .request(Arc::clone(&state.add_source), |source| source.nearby());
    }
    let animated = matches!(
        state.add_mac.page,
        AddMacPage::Prepare
            | AddMacPage::Network
            | AddMacPage::Adding {
                action: Some(Action::Tailnet { .. }),
                ..
            }
    );
    if animated {
        tx.wake_after(ANIMATION_FRAME);
        changed = true;
    } else if state.add_mac.looking() {
        tx.wake_after(std::time::Duration::from_secs_f64(LOOK_EVERY));
    }
    changed
}

fn sample_policy() -> Result<Policy, ReadError> {
    Policy::parse(Some(
        include_str!("../../../tests/fixtures/development-workloads.toml").into(),
    ))
}

fn scene_configuration_rules(state: &mut AppState) {
    scene_loading(state);
    state.configuration.scene(Tab::Rules, sample_policy(), "");
}

fn scene_configuration_search(state: &mut AppState) {
    scene_loading(state);
    state
        .configuration
        .scene(Tab::Rules, sample_policy(), "BUILD");
}

fn scene_configuration_no_matches(state: &mut AppState) {
    scene_loading(state);
    state
        .configuration
        .scene(Tab::Rules, sample_policy(), "unlisted task");
}

fn scene_configuration_toml(state: &mut AppState) {
    scene_loading(state);
    state.configuration.scene(Tab::Toml, sample_policy(), "");
}

fn scene_configuration_missing(state: &mut AppState) {
    scene_loading(state);
    state
        .configuration
        .scene(Tab::Rules, Policy::parse(None), "");
}

fn scene_configuration_invalid(state: &mut AppState) {
    scene_loading(state);
    state
        .configuration
        .scene(Tab::Rules, Err(ReadError::Invalid), "");
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

fn scene_resource_load(state: &mut AppState) {
    state.snapshot_at = elsecall_contract::scheduling_model::timestamp::Timestamp::parse_rfc3339(
        "2026-11-01T14:00:00Z",
    )
    .unwrap();
    state.follow_service = false;
    state.refreshed = Some("14:00:00".into());
    let fixture = serde_json::from_str(include_str!("../tests/fixtures/resource-load.json"))
        .expect("resource scene");
    state.phase =
        Phase::Ready(socket_source::snapshot_from_value(&fixture).expect("resource snapshot"));
}

fn scene_members(state: &mut AppState) {
    state.follow_service = false;
    state.refreshed = Some("14:32:05".into());
    state.phase = Phase::Ready(members_snapshot());
}

/// The last row selected, so its highlight meets the card's border and bottom corners.
fn scene_members_selected(state: &mut AppState) {
    scene_members(state);
    state.table.select("macbook-pro");
}

fn scene_members_awaiting_provisioning(state: &mut AppState) {
    let mut snapshot = members_snapshot();
    snapshot.members[0].release = Release::Matching {
        version: "0.4.0".into(),
    };
    let mac = &mut snapshot.members[2];
    mac.readiness = Readiness::AwaitingProvisioning;
    mac.reach = Reachability::Reachable;
    mac.gate = Gate::Open;
    mac.release = Release::Matching {
        version: "0.4.0".into(),
    };
    state.follow_service = false;
    state.refreshed = Some("14:32:05".into());
    state.phase = Phase::Ready(snapshot);
}

/// A member the master's last update left behind, as `elsecall fleet upgrade`
/// would still have to bring it forward.
fn scene_outdated_member(state: &mut AppState) {
    state.follow_service = false;
    state.refreshed = Some("14:32:05".into());
    let mut snapshot = members_snapshot();
    snapshot.members[2].release = Release::Behind {
        version: "0.2.1".into(),
    };
    let reachable = |name: &str, release: Release| {
        let mut row = member(
            name,
            Role::Member,
            Reachability::Reachable,
            Gate::Open,
            Some((0.12, 8)),
            Some(gigabytes(10, 32)),
            Some(LoadAverages {
                one: 1.1,
                five: 1.3,
                fifteen: 1.2,
            }),
        );
        row.release = release;
        row
    };
    snapshot.members.push(reachable(
        "mac-studio",
        Release::Ahead {
            version: "0.3.0-rc.1".into(),
        },
    ));
    snapshot.members.push(reachable(
        "imac",
        Release::OtherBuild {
            version: "0.2.2".into(),
        },
    ));
    state.phase = Phase::Ready(snapshot);
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
    use elsecall_lifecycle::release_update::{JobId, Release, ReleaseVersion};
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
            fleet: None,
        }),
    }
}

/// The master updated itself and now gives its build to the members, which still
/// run the previous release.
fn scene_release_fleet(state: &mut AppState) {
    use elsecall_lifecycle::release_update::{
        FleetFollowUp, FollowUpState, JobId, Release, ReleaseVersion,
    };
    scene_outdated_member(state);
    let current = ReleaseVersion::try_from("0.2.2".to_owned()).expect("fixture current");
    let id = JobId::try_from("a".repeat(32)).expect("fixture job");
    state.apply_release(Ok(ReleaseSnapshot {
        installed_window: None,
        release: Some(ReleaseStatus {
            current_version: current.clone(),
            available: None,
            checked_at: Some(1700000000),
            check_error: None,
            job: Some(Job {
                id: id.clone(),
                release: Release { version: current },
                from_build: "1111111111111111".parse().expect("fixture build"),
                started_at_ms: 20,
                reopen: true,
                wait_seconds: 300,
                phase: ReleasePhase::Succeeded,
                updated_at: 1700000000,
            }),
            fleet: Some(FleetFollowUp {
                job: id,
                state: FollowUpState::Distributing {
                    started_at: 1700000000,
                },
            }),
        }),
    }));
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
            error: elsecall_contract::failure::Failure::new(
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
        .check_error = Some(elsecall_contract::failure::Failure::new(
        "release_tool_failed",
        "The release server could not be reached.",
        "The cached offer remains available.",
    ));
}

fn solo_snapshot() -> FleetSnapshot {
    FleetSnapshot {
        enrolled: false,
        members: vec![member(
            "mac-mini",
            Role::Master,
            Reachability::Reachable,
            Gate::Open,
            Some((0.18, 8)),
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
                Some((0.42, 12)),
                Some(gigabytes(36, 64)),
                Some(LoadAverages {
                    one: 4.8,
                    five: 4.5,
                    fifteen: 4.2,
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
                "macbook-pro",
                Role::Member,
                Reachability::Reachable,
                Gate::Blocked {
                    leading_process: Some("Xcode".into()),
                },
                Some((0.91, 8)),
                Some(gigabytes(52, 64)),
                Some(LoadAverages {
                    one: 12.0,
                    five: 9.4,
                    fifteen: 7.2,
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
    cpu: Option<(f64, u32)>,
    memory: Option<MemoryUse>,
    load: Option<LoadAverages>,
) -> MemberRow {
    let release = match reach {
        Reachability::Reachable => Release::Matching {
            version: "0.2.2".into(),
        },
        Reachability::Unreachable => Release::Unknown { version: None },
    };
    MemberRow {
        name: name.into(),
        // The scenes' master is this Mac.
        this_member: role == Role::Master,
        role,
        reach,
        readiness: Readiness::Ready,
        gate,
        contributors: Default::default(),
        cpu: cpu.map(|(utilization, _)| utilization),
        logical_cpus: cpu.and_then(|(_, count)| NonZeroU32::new(count)),
        memory,
        load,
        release,
    }
}

fn gigabytes(used: u64, total: u64) -> MemoryUse {
    MemoryUse {
        used_bytes: used << 30,
        total_bytes: total << 30,
    }
}

#[cfg(test)]
#[path = "add_mac_tests.rs"]
mod add_mac_tests;

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
                error: elsecall_lifecycle::release_update::invalid(
                    "The release could not be verified.",
                ),
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
        second.id = elsecall_lifecycle::release_update::JobId::try_from("b".repeat(32)).unwrap();
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
                error: elsecall_lifecycle::release_update::invalid("old failure"),
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

    struct Bundle {
        uninstalled: std::sync::atomic::AtomicBool,
        quits: std::sync::atomic::AtomicUsize,
    }
    impl crate::uninstalled::Bundle for Bundle {
        fn uninstalled(&self) -> bool {
            self.uninstalled.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn quit(&self) {
            self.quits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn the_window_closes_once_the_service_is_unreachable_and_it_was_uninstalled() {
        use std::sync::atomic::Ordering;
        let bundle = Arc::new(Bundle {
            uninstalled: false.into(),
            quits: 0.into(),
        });
        let mut state = init();
        state.bundle = bundle.clone();
        let unavailable = || Update {
            result: Err(SourceFailure::ServiceUnavailable {
                message: "The local service could not be reached.".into(),
                hint: "Run elsecall install if absent, or elsecall service restart if installed."
                    .into(),
            }),
            refreshed_at: "14:32:07".into(),
        };
        // A service restart, as during an update, keeps the window.
        state.apply_refresh(unavailable());
        assert_eq!(bundle.quits.load(Ordering::SeqCst), 0);
        bundle.uninstalled.store(true, Ordering::SeqCst);
        // A service that answers, even with an error, keeps it too.
        state.apply_refresh(Update {
            result: Ok(solo_snapshot()),
            refreshed_at: "14:32:09".into(),
        });
        state.apply_refresh(Update {
            result: Err(SourceFailure::Rejected {
                message: "The fleet response could not be read.".into(),
                hint: "Reinstall matching CLI and service versions with elsecall install.".into(),
            }),
            refreshed_at: "14:32:11".into(),
        });
        assert_eq!(bundle.quits.load(Ordering::SeqCst), 0);
        // Uninstalled: no service, no app and no CLI link.
        state.apply_refresh(unavailable());
        assert_eq!(bundle.quits.load(Ordering::SeqCst), 1);
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
