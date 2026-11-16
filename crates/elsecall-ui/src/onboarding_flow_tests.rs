//! Pointer-driven real view/controller tests, with fakes only at preparation
//! and native transport boundaries.
use super::*;
use crate::{
    installer_transfer::{InstallerTransfer, TransferEvent},
    onboarding::{Onboarding, OnboardingPreparation, Prepared, Problem, State},
};
use std::{
    cell::{Cell, RefCell},
    path::Path,
    rc::Rc,
    sync::{Mutex, mpsc},
    time::Duration,
};

struct Preparation(Mutex<mpsc::Receiver<Result<Prepared, Problem>>>);
impl OnboardingPreparation for Preparation {
    fn prepare(&self) -> Result<Prepared, Problem> {
        self.0
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
    }
}
struct Transfer {
    starts: Rc<Cell<usize>>,
    event: Rc<RefCell<Option<TransferEvent>>>,
    available: bool,
}
impl InstallerTransfer for Transfer {
    fn start(&mut self, _: &Path) -> Result<(), ()> {
        self.starts.set(self.starts.get() + 1);
        if self.available { Ok(()) } else { Err(()) }
    }
    fn poll(&mut self) -> Option<TransferEvent> {
        self.event.borrow_mut().take()
    }
}
fn click(rt: &mut rune::Runtime<AppState>) {
    let rect = rt.presented("add-mac").unwrap().0;
    let (x, y) = (rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0);
    rt.pointer_move(x, y);
    rt.frame(0.0);
    rt.pointer_down(x, y);
    rt.frame(0.0);
    rt.pointer_up(x, y);
    rt.frame(0.0);
}

#[test]
fn add_mac_prepares_once_opens_picker_directly_and_allows_retry_after_cancel_or_failure() {
    let _guard = theme::serial_render();
    let (reply, receiver) = mpsc::channel();
    let (wake, ready) = mpsc::channel();
    let starts = Rc::new(Cell::new(0));
    let event = Rc::new(RefCell::new(None));
    let mut state = init();
    scene_release_available(&mut state);
    state.onboarding = Onboarding::new(
        Arc::new(Preparation(Mutex::new(receiver))),
        Box::new(Transfer {
            starts: starts.clone(),
            event: event.clone(),
            available: true,
        }),
    );
    state.onboarding.demo = true;
    state.onboarding.set_waker(Arc::new(move || {
        let _ = wake.send(());
    }));
    let mut rt = rune::Runtime::new(
        state,
        view::fleet,
        (1120.0, 560.0),
        rune::Env::new(false, 1.0),
    );
    rt.frame(0.0);
    assert!(rt.presented("upgrade-window").is_some());
    for outcome in [
        TransferEvent::Cancelled,
        TransferEvent::Failed,
        TransferEvent::FinishedUnconfirmed,
    ] {
        click(&mut rt);
        assert_eq!(rt.state.onboarding.state, State::Preparing);
        rt.state.apply_release(Ok(release_fixture(None)));
        assert_eq!(rt.state.onboarding.state, State::Preparing);
        click(&mut rt); // Busy button cannot create a second operation.
        assert_eq!(rt.state.rows().len(), 3);
        reply
            .send(Ok(Prepared::DemoFile("/tmp/demo.txt".into())))
            .unwrap();
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        poll(&mut rt.state, 0.0, &mut Tx::default());
        assert_eq!(rt.state.onboarding.state, State::ChoosingRecipient);
        *event.borrow_mut() = Some(outcome);
        poll(&mut rt.state, 0.0, &mut Tx::default());
        assert!(!rt.state.onboarding.busy());
        let expected = match outcome {
            TransferEvent::Cancelled => State::Cancelled,
            TransferEvent::Failed => State::Failed(Problem::TransferFailed),
            TransferEvent::FinishedUnconfirmed => State::DeliveryUnconfirmed,
        };
        assert_eq!(rt.state.onboarding.state, expected);
        rt.invalidate();
        rt.frame(0.0);
    }
    assert_eq!(starts.get(), 3);
}

#[test]
fn production_blocker_and_unverified_artifact_never_open_native_transfer() {
    let _guard = theme::serial_render();
    for result in [
        Err(Problem::InstallerUnavailable),
        Ok(Prepared::DemoFile("/tmp/unverified.zip".into())),
    ] {
        let (reply, receiver) = mpsc::channel();
        let (wake, ready) = mpsc::channel();
        let starts = Rc::new(Cell::new(0));
        let mut state = init();
        scene_members(&mut state);
        state.onboarding = Onboarding::new(
            Arc::new(Preparation(Mutex::new(receiver))),
            Box::new(Transfer {
                starts: starts.clone(),
                event: Rc::new(RefCell::new(None)),
                available: true,
            }),
        );
        state.onboarding.set_waker(Arc::new(move || {
            let _ = wake.send(());
        }));
        let mut rt = rune::Runtime::new(
            state,
            view::fleet,
            (1040.0, 480.0),
            rune::Env::new(false, 1.0),
        );
        rt.frame(0.0);
        click(&mut rt);
        reply.send(result).unwrap();
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        poll(&mut rt.state, 0.0, &mut Tx::default());
        assert!(matches!(rt.state.onboarding.state, State::Failed(_)));
        assert_eq!(starts.get(), 0);
    }
}

#[test]
fn picker_open_failure_offers_retry_without_losing_the_fleet() {
    struct Picker(Rc<Cell<usize>>);
    impl InstallerTransfer for Picker {
        fn start(&mut self, _: &Path) -> Result<(), ()> {
            self.0.set(self.0.get() + 1);
            if self.0.get() == 1 { Err(()) } else { Ok(()) }
        }
        fn poll(&mut self) -> Option<TransferEvent> {
            None
        }
    }
    let _guard = theme::serial_render();
    let (reply, receiver) = mpsc::channel();
    let (wake, ready) = mpsc::channel();
    let starts = Rc::new(Cell::new(0));
    let mut state = init();
    scene_members(&mut state);
    state.onboarding = Onboarding::new(
        Arc::new(Preparation(Mutex::new(receiver))),
        Box::new(Picker(starts.clone())),
    );
    state.onboarding.demo = true;
    state.onboarding.set_waker(Arc::new(move || {
        let _ = wake.send(());
    }));
    let mut rt = rune::Runtime::new(
        state,
        view::fleet,
        (1120.0, 560.0),
        rune::Env::new(false, 1.0),
    );
    rt.frame(0.0);
    for expected in [
        State::Failed(Problem::AirDropUnavailable),
        State::ChoosingRecipient,
    ] {
        click(&mut rt);
        reply
            .send(Ok(Prepared::DemoFile("/tmp/demo.txt".into())))
            .unwrap();
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        poll(&mut rt.state, 0.0, &mut Tx::default());
        assert_eq!(rt.state.onboarding.state, expected);
        assert_eq!(rt.state.rows().len(), 3);
        rt.invalidate();
        rt.frame(0.0);
    }
    assert_eq!(starts.get(), 2);
}
