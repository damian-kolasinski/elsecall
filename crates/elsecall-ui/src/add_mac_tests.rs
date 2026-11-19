//! The Add Mac sheet driven by pointer through the real view, with the CLI faked
//! at its boundary.

use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use elsecall_fleet::add_mac::{AddReport, Approval, Event, Step};
use elsecall_fleet::nearby_macs::NearbyMac;
use elsecall_fleet::remote_mac::LoginLink;
use rune::Tx;

use super::{AppState, init, poll, render, scene_members};
use crate::add_mac_flow::{AddRequest, AddUpdate, Page, mac};
use crate::add_mac_source::AddMacSource;
use crate::theme;

const LINK: &str = "https://login.tailscale.com/a/example";

/// Finds two Macs, and adds one: a few steps, an approval, then the report.
struct FakeCli {
    added: Mutex<Option<(String, String, String)>>,
    stopped: Mutex<bool>,
    /// Holds the add at its approval until the test lets it finish.
    approved: Mutex<mpsc::Receiver<()>>,
}

impl AddMacSource for FakeCli {
    fn nearby(&self) -> Result<Vec<NearbyMac>, elsecall_contract::failure::Failure> {
        Ok(vec![
            mac("MacBook Pro", true, None),
            mac("Studio", false, None),
        ])
    }
    fn add(&self, request: AddRequest, updates: &(dyn Fn(AddUpdate) + Sync)) {
        *self.added.lock().unwrap() = Some((
            request.host.clone(),
            request.account.clone(),
            request.password.to_string(),
        ));
        for step in [Step::Check, Step::Tailnet] {
            updates(AddUpdate::Event(Event::Step { step }));
        }
        updates(AddUpdate::Event(Event::Approve {
            link: LoginLink::parse(LINK).unwrap(),
            approval: Approval::Login,
        }));
        let _ = self
            .approved
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10));
        updates(AddUpdate::Event(Event::Step {
            step: Step::Install,
        }));
        let report: AddReport = serde_json::from_value(serde_json::json!({
            "member": "macbook-pro",
            "host": request.host,
            "mac": {"model": "Mac16,10", "serial_number": null, "macos_version": "26.1",
                    "ssh_host_key": "SHA256:example (ED25519)"},
            "readiness": "awaiting_provisioning",
            "tailscale_key_expiry": null,
            "steps": [],
            "next_step": ""
        }))
        .unwrap();
        updates(AddUpdate::Finished(Ok(report)));
    }
    fn stop(&self, _: u64) {
        *self.stopped.lock().unwrap() = true;
    }
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
    let mut now = 10.0;
    while !done(&rt.state) {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        now += 3.5;
        poll(&mut rt.state, now, &mut Tx::default());
        std::thread::sleep(Duration::from_millis(10));
    }
    rt.invalidate();
    rt.frame(0.0);
}

fn window(cli: Arc<FakeCli>) -> rune::Runtime<AppState> {
    let mut state = init();
    scene_members(&mut state);
    // The sheet looks for Macs, as in the running window, but through the fake.
    state.follow_service = true;
    state.add_source = cli;
    let mut rt = rune::Runtime::new(state, render, (1120.0, 560.0), rune::Env::new(false, 1.0));
    rt.frame(0.0);
    rt
}

#[test]
fn a_mac_is_added_from_the_header_through_every_page() {
    let _guard = theme::serial_render();
    let (approve, approved) = mpsc::channel();
    let cli = Arc::new(FakeCli {
        added: Mutex::new(None),
        stopped: Mutex::new(false),
        approved: Mutex::new(approved),
    });
    let mut rt = window(Arc::clone(&cli));

    click(&mut rt, "add-mac");
    assert!(
        rt.presented("add-mac-settings-art").is_some(),
        "the instructions show first"
    );
    click(&mut rt, "add-mac-continue");
    poll_until(&mut rt, "the Macs nearby", |state| {
        !state.add_mac().found().is_empty()
    });
    // The only Mac ready to add is picked; the one with Remote Login off cannot be.
    click(&mut rt, "add-mac-host-studio.local");
    assert_eq!(
        rt.state.add_mac().chosen().unwrap().host,
        "macbook-pro.local"
    );
    click(&mut rt, "add-mac-continue");
    assert!(rt.presented("add-mac-password").is_some());
    assert!(
        rt.presented("add-mac-target").is_some(),
        "the exact Mac is shown"
    );

    rt.state.add_mac_mut().account.replace_text("devbox");
    rt.state.add_mac_mut().password.replace_text("s3cret");
    rt.invalidate();
    rt.frame(0.0);
    click(&mut rt, "add-mac-start");
    poll_until(&mut rt, "the approval", |state| {
        matches!(
            state.add_mac().page,
            Page::Adding {
                approval: Some(_),
                ..
            }
        )
    });
    assert!(rt.presented("add-mac-approve").is_some());
    assert!(rt.presented("add-mac-open-link").is_some());
    assert_eq!(
        cli.added.lock().unwrap().clone().unwrap(),
        ("macbook-pro.local".into(), "devbox".into(), "s3cret".into())
    );
    assert_eq!(
        rt.state.add_mac().password.text,
        "",
        "the sheet no longer holds it"
    );

    approve.send(()).unwrap();
    poll_until(&mut rt, "the end", |state| {
        matches!(state.add_mac().page, Page::Done { .. })
    });
    assert!(rt.presented("add-mac-done").is_some());
    click(&mut rt, "add-mac-close");
    assert!(!rt.state.add_mac().is_open());
    assert!(!*cli.stopped.lock().unwrap());
}

#[test]
fn closing_the_sheet_while_adding_stops_the_add_and_keeps_the_sheet() {
    let _guard = theme::serial_render();
    let (approve, approved) = mpsc::channel();
    let cli = Arc::new(FakeCli {
        added: Mutex::new(None),
        stopped: Mutex::new(false),
        approved: Mutex::new(approved),
    });
    let mut rt = window(Arc::clone(&cli));
    click(&mut rt, "add-mac");
    click(&mut rt, "add-mac-continue");
    poll_until(&mut rt, "the Macs nearby", |state| {
        !state.add_mac().found().is_empty()
    });
    click(&mut rt, "add-mac-continue");
    rt.state.add_mac_mut().account.replace_text("devbox");
    rt.state.add_mac_mut().password.replace_text("s3cret");
    rt.invalidate();
    rt.frame(0.0);
    click(&mut rt, "add-mac-start");
    poll_until(&mut rt, "adding", |state| state.add_mac().adding());
    // The dialog's own close button stops the add, and the sheet stays to show how
    // it ended.
    click(&mut rt, "close");
    assert!(*cli.stopped.lock().unwrap());
    assert!(rt.state.add_mac().is_open());
    drop(approve);
}

#[test]
fn only_the_master_offers_add_mac() {
    let _guard = theme::serial_render();
    let mut state = init();
    scene_members(&mut state);
    let mut rt = rune::Runtime::new(state, render, (1120.0, 560.0), rune::Env::new(false, 1.0));
    rt.frame(0.0);
    assert!(rt.presented("add-mac").is_some());
    // The same fleet seen from a member that is not the master.
    let mut state = init();
    scene_members(&mut state);
    let mut snapshot = state.shown().cloned().unwrap();
    for row in &mut snapshot.members {
        row.this_member = row.role != crate::fleet_source::Role::Master;
    }
    state.phase = super::Phase::Ready(snapshot);
    let mut rt = rune::Runtime::new(state, render, (1120.0, 560.0), rune::Env::new(false, 1.0));
    rt.frame(0.0);
    assert!(rt.presented("add-mac").is_none());
}
