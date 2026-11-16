//! One continuous Add Mac interaction. Preparation and transport are boundaries;
//! transport completion grants no installation, enrollment or readiness status.
use crate::installer_transfer::{InstallerTransfer, TransferEvent};
use rune::{Latest, Waker};
use std::{path::PathBuf, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    InstallerUnavailable,
    ServiceUnavailable,
    UnsupportedResponse,
    AirDropUnavailable,
    TransferFailed,
}

impl Problem {
    pub fn message(self) -> &'static str {
        match self {
            Self::InstallerUnavailable => {
                "A signed Mac installer is not available yet. No invitation or file was sent. Update Elsecall when the installer is available, then retry."
            }
            Self::ServiceUnavailable => {
                "The local service could not prepare onboarding. Run elsecall doctor, then retry Add Mac."
            }
            Self::UnsupportedResponse => {
                "This window cannot use the service's onboarding response. Install matching Elsecall versions, then retry. No file was sent."
            }
            Self::AirDropUnavailable => {
                "AirDrop could not open. Check that you are in a macOS desktop session with AirDrop available, then retry."
            }
            Self::TransferFailed => {
                "AirDrop did not complete the transfer. Check Wi-Fi, Bluetooth and AirDrop receiving on the other Mac, then retry."
            }
        }
    }
}

// Production preparation intentionally cannot construct a transfer in this
// slice. A verified installer acquisition boundary must land before enabling it.
pub enum Prepared {
    DemoFile(PathBuf),
}

pub trait OnboardingPreparation: Send + Sync {
    fn prepare(&self) -> Result<Prepared, Problem>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Preparing,
    ChoosingRecipient,
    DeliveryUnconfirmed,
    Cancelled,
    Failed(Problem),
}

pub struct Onboarding {
    pub state: State,
    pub demo: bool,
    preparation: Arc<dyn OnboardingPreparation>,
    prepared: Latest<Result<Prepared, Problem>>,
    transfer: Box<dyn InstallerTransfer>,
}

impl Onboarding {
    pub fn new(
        preparation: Arc<dyn OnboardingPreparation>,
        transfer: Box<dyn InstallerTransfer>,
    ) -> Self {
        Self {
            state: State::Idle,
            demo: false,
            preparation,
            prepared: Latest::default(),
            transfer,
        }
    }

    pub fn set_waker(&self, waker: Waker) {
        self.prepared.set_waker(waker);
    }

    pub fn busy(&self) -> bool {
        matches!(self.state, State::Preparing | State::ChoosingRecipient)
    }

    pub fn start(&mut self) {
        if self.busy() {
            return;
        }
        self.state = State::Preparing;
        self.prepared
            .request(Arc::clone(&self.preparation), |source| source.prepare());
    }

    pub fn poll(&mut self) -> bool {
        if let Some(prepared) = self.prepared.take() {
            self.state = match prepared {
                Ok(Prepared::DemoFile(path)) if self.demo => match self.transfer.start(&path) {
                    Ok(()) => State::ChoosingRecipient,
                    Err(()) => State::Failed(Problem::AirDropUnavailable),
                },
                Ok(_) => State::Failed(Problem::UnsupportedResponse),
                Err(problem) => State::Failed(problem),
            };
            return true;
        }
        if self.state == State::ChoosingRecipient
            && let Some(event) = self.transfer.poll()
        {
            self.state = match event {
                TransferEvent::FinishedUnconfirmed => State::DeliveryUnconfirmed,
                TransferEvent::Cancelled => State::Cancelled,
                TransferEvent::Failed => State::Failed(Problem::TransferFailed),
            };
            return true;
        }
        false
    }

    pub fn copy(&self) -> Option<(&'static str, &'static str)> {
        match self.state {
            State::Idle => None,
            State::Preparing => Some((
                "Preparing Add Mac…",
                "Checking installer availability. Keep this window open.",
            )),
            State::ChoosingRecipient => Some((
                "Choose a Mac in AirDrop",
                "Demo: only a harmless text file will be shared. Use Cancel in the AirDrop window to stop.",
            )),
            State::DeliveryUnconfirmed => Some((
                "AirDrop closed",
                "Delivery is unconfirmed. If you cancelled, you can retry. This demo does not install, connect or enroll a Mac.",
            )),
            State::Cancelled => Some((
                "AirDrop cancelled",
                "No onboarding was completed. You can retry Add Mac.",
            )),
            State::Failed(problem) => Some(("Add Mac needs attention", problem.message())),
        }
    }
}
