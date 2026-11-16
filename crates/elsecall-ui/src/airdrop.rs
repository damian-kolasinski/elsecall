//! Public AppKit NSSharingService adapter. No Finder or sharing-method menu.
use crate::installer_transfer::{InstallerTransfer, TransferEvent};
use std::{
    ffi::{CString, c_char, c_void},
    os::unix::ffi::OsStrExt,
    path::Path,
    ptr::NonNull,
};

unsafe extern "C" {
    fn elsecall_airdrop_start(path: *const c_char) -> *mut c_void;
    fn elsecall_airdrop_result(handle: *mut c_void) -> i32;
    fn elsecall_airdrop_release(handle: *mut c_void);
}

#[derive(Default)]
pub struct AirDrop {
    active: Option<NonNull<c_void>>,
}

impl InstallerTransfer for AirDrop {
    fn start(&mut self, file: &Path) -> Result<(), ()> {
        if self.active.is_some() || !file.is_absolute() || !file.is_file() {
            return Err(());
        }
        let path = CString::new(file.as_os_str().as_bytes()).map_err(|_| ())?;
        // SAFETY: called only from rune's main-thread actions/poll. C copies the
        // path and returns an owned handle; callbacks stay inside the bridge.
        self.active = NonNull::new(unsafe { elsecall_airdrop_start(path.as_ptr()) });
        self.active.map(|_| ()).ok_or(())
    }

    fn poll(&mut self) -> Option<TransferEvent> {
        let handle = self.active?;
        let result = unsafe { elsecall_airdrop_result(handle.as_ptr()) };
        let event = match result {
            0 => return None,
            1 => TransferEvent::FinishedUnconfirmed,
            2 => TransferEvent::Cancelled,
            _ => TransferEvent::Failed,
        };
        self.active = None;
        unsafe { elsecall_airdrop_release(handle.as_ptr()) };
        Some(event)
    }
}

impl Drop for AirDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.active.take() {
            unsafe { elsecall_airdrop_release(handle.as_ptr()) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_files_never_enter_appkit() {
        let mut transfer = AirDrop::default();
        assert!(transfer.start(Path::new("relative.zip")).is_err());
        assert!(
            transfer
                .start(Path::new("/nonexistent/elsecall.zip"))
                .is_err()
        );
        assert!(transfer.poll().is_none());
    }
}
