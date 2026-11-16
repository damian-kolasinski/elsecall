//! Port for presenting a prepared file directly to the system recipient picker.
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferEvent {
    FinishedUnconfirmed,
    Cancelled,
    Failed,
}

// Main-thread interface: AppKit owns discovery, recipient selection and transfer.
pub trait InstallerTransfer {
    fn start(&mut self, file: &Path) -> Result<(), ()>;
    fn poll(&mut self) -> Option<TransferEvent>;
}
