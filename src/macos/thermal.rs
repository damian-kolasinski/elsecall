//! Thermal pressure through the system notification state that backs
//! `ProcessInfo.thermalState`.
use crate::inspection::ThermalState;
use std::{
    ffi::{c_char, c_int},
    sync::OnceLock,
};

unsafe extern "C" {
    fn notify_register_check(name: *const c_char, out_token: *mut c_int) -> u32;
    fn notify_get_state(token: c_int, state: *mut u64) -> u32;
}

const THERMAL_PRESSURE: &std::ffi::CStr = c"com.apple.system.thermalpressurelevel";

fn token() -> Option<c_int> {
    static TOKEN: OnceLock<Option<c_int>> = OnceLock::new();
    *TOKEN.get_or_init(|| {
        let mut token = 0;
        // SAFETY: the name is NUL-terminated and the token pointer is valid. The
        // registration lives for the rest of the process, as the token is cached.
        let status = unsafe { notify_register_check(THERMAL_PRESSURE.as_ptr(), &mut token) };
        (status == 0).then_some(token)
    })
}

/// `None` when the state cannot be read.
pub fn state() -> Option<ThermalState> {
    let mut level = 0;
    // SAFETY: the token came from notify_register_check and the pointer is valid.
    let status = unsafe { notify_get_state(token()?, &mut level) };
    if status != 0 {
        return None;
    }
    // Levels from OSThermalNotification.h.
    Some(match level {
        0 => ThermalState::Nominal,
        1 => ThermalState::Moderate,
        2 => ThermalState::Heavy,
        3 => ThermalState::Trapping,
        _ => ThermalState::Sleeping,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_thermal_state() {
        assert!(super::state().is_some());
    }
}
