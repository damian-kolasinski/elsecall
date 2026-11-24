//! The one-click path through the real view: Provision on a member awaiting
//! provisioning asks the master, the row follows the install step by step,
//! a step that needs a person opens the Setup dialog, and the member ends
//! ready. The service is faked at the window's boundary: the provisioning port
//! and the fleet reads the refresh thread would deliver.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rune::Tx;

use super::{
    AppState, init, poll, render, scene_members_awaiting_provisioning,
    scene_provisioning_needs_you, scene_provisioning_ready, scene_provisioning_running,
};
use crate::fleet_source::FleetSnapshot;
use crate::provisioning_source::{Apply, Plan, ProvisioningSource};
use crate::provisioning_state::ProvisioningState;
use crate::refresh::Update;
use crate::theme;

#[derive(Default)]
struct FakeService {
    asked: Mutex<Vec<String>>,
    read: Mutex<Vec<String>>,
}

impl ProvisioningSource for FakeService {
    fn provision(&self, member: &str) -> Result<(), String> {
        self.asked.lock().unwrap().push(member.to_owned());
        Ok(())
    }
    fn plan(&self, member: &str) -> Result<Plan, String> {
        self.read.lock().unwrap().push(member.to_owned());
        Ok(Plan {
            steps: Vec::new(),
            unresolved: Vec::new(),
            partial: false,
        })
    }
    fn apply(&self, _member: &str) -> Result<Option<Apply>, String> {
        Ok(Some(Apply {
            finished: true,
            steps: Vec::new(),
        }))
    }
}

fn fleet_of(scene: fn(&mut AppState)) -> FleetSnapshot {
    let mut state = init();
    scene(&mut state);
    state.shown().cloned().unwrap()
}

fn click(rt: &mut rune::Runtime<AppState>, key: &str) {
    let rect = rt
        .presented(key)
        .unwrap_or_else(|| panic!("{key} is not on screen"))
        .0;
    let (x, y) = (rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0);
    rt.pointer_move(x, y);
    rt.frame(1.0 / 60.0);
    rt.pointer_down(x, y);
    rt.frame(1.0 / 60.0);
    rt.pointer_up(x, y);
    rt.frame(0.0);
}

/// Polls until `done` holds, as the window does when its wakers fire.
fn poll_until(rt: &mut rune::Runtime<AppState>, what: &str, done: impl Fn(&AppState) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !done(&rt.state) {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        let now = rt.state.clock() + 0.5;
        poll(&mut rt.state, now, &mut Tx::default());
        std::thread::sleep(Duration::from_millis(10));
    }
    rt.invalidate();
    rt.frame(0.0);
}

/// The fleet as the refresh thread delivers it next.
fn refresh(rt: &mut rune::Runtime<AppState>, fleet: FleetSnapshot) {
    rt.state.apply_refresh(Update {
        result: Ok(fleet),
        refreshed_at: "14:33:00".into(),
    });
    let now = rt.state.clock() + 2.0;
    poll(&mut rt.state, now, &mut Tx::default());
    rt.invalidate();
    rt.frame(0.0);
    // Let what left the row finish fading out.
    for _ in 0..3 {
        rt.frame(1.0);
    }
}

#[test]
fn one_click_provisions_a_member_until_it_is_ready() {
    let _guard = theme::serial_render();
    let service = Arc::new(FakeService::default());
    let mut state = init();
    scene_members_awaiting_provisioning(&mut state);
    state.provisioning =
        ProvisioningState::new(Arc::clone(&service) as Arc<dyn ProvisioningSource>);
    let mut rt = rune::Runtime::new(state, render, (1120.0, 560.0), rune::Env::new(false, 1.0));
    rt.frame(0.0);

    click(&mut rt, "provision-macbook-pro");
    poll_until(&mut rt, "the ask to reach the service", |_| {
        service.asked.lock().unwrap().as_slice() == ["macbook-pro"]
    });
    assert!(
        rt.state.provisioning.requested("macbook-pro"),
        "the row says it is starting until the fleet shows it running"
    );

    refresh(&mut rt, fleet_of(scene_provisioning_running));
    assert!(rt.presented("installing-macbook-pro").is_some());
    assert!(rt.presented("provision-macbook-pro").is_none());
    assert!(!rt.state.provisioning.requested("macbook-pro"));

    // Xcode needs privileges only a person installs: the row says so, and
    // opens the member's setup.
    refresh(&mut rt, fleet_of(scene_provisioning_needs_you));
    assert!(rt.presented("attention").is_some(), "the items are listed");
    click(&mut rt, "needs-you-macbook-pro");
    poll_until(&mut rt, "the setup to be read", |state| {
        state.provisioning.details.plan.is_some() && state.provisioning.details.apply.is_some()
    });
    assert_eq!(service.read.lock().unwrap().as_slice(), ["macbook-pro"]);
    assert!(rt.presented("setup-details").is_some());
    assert!(
        rt.presented("person-step:provisioning_step_failed:macbook-pro:xcodes:xcode@26.2")
            .is_some(),
        "the person's step is in the dialog"
    );
    rt.state.provisioning.close_setup();

    refresh(&mut rt, fleet_of(scene_provisioning_ready));
    for gone in [
        "provision-macbook-pro",
        "installing-macbook-pro",
        "needs-you-macbook-pro",
        "attention",
    ] {
        assert!(rt.presented(gone).is_none(), "{gone} after it became ready");
    }
}

#[test]
fn a_ready_members_install_failure_opens_its_placement_history() {
    let _guard = theme::serial_render();
    let mut state = init();
    super::scene_tool_install_failed(&mut state);
    let service = Arc::new(FakeService::default());
    state.provisioning =
        ProvisioningState::new(Arc::clone(&service) as Arc<dyn ProvisioningSource>);
    let mut rt = rune::Runtime::new(state, render, (1120.0, 700.0), rune::Env::new(false, 1.0));
    rt.frame(0.0);

    click(&mut rt, "install-failed-macbook-pro");
    assert_eq!(rt.state.provisioning.setup.as_deref(), Some("macbook-pro"));
    assert!(rt.presented("setup-placement-history").is_some());
    poll_until(&mut rt, "the failed member’s plan", |state| {
        state.provisioning.details.plan.is_some()
    });
    click(&mut rt, "setup-provision");
    poll_until(&mut rt, "the retry request", |_| {
        !service.asked.lock().unwrap().is_empty()
    });
    assert!(rt.state.provisioning.requested("macbook-pro"));
    click(&mut rt, "setup-provision");
    let mut repaired = fleet_of(scene_provisioning_ready);
    let apply = repaired.members[2].provisioning.as_mut().unwrap();
    apply.started_at = apply.started_at.after(Duration::from_secs(60));
    apply.finished_at = Some(apply.started_at);

    repaired.members[2].pin_exclusions =
        rt.state.shown().unwrap().members[2].pin_exclusions.clone();
    refresh(&mut rt, repaired);
    poll_until(&mut rt, "the updated plan", |state| {
        state.provisioning.details.plan.is_some()
    });
    assert_eq!(service.asked.lock().unwrap().as_slice(), ["macbook-pro"]);
    assert_eq!(
        service.read.lock().unwrap().len(),
        2,
        "the completed install refreshes the plan"
    );
    assert!(!rt.state.provisioning.requested("macbook-pro"));
    assert!(
        rt.presented("setup-placement-history").is_some(),
        "past decisions survive repair"
    );
    rt.state.provisioning.close_setup();
    rt.invalidate();
    rt.frame(1.0);
    assert!(rt.presented("install-failed-macbook-pro").is_none());
}
