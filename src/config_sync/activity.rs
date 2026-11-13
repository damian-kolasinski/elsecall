//! Tracks sync operations across both transports so a relaunch cannot cut off a
//! push, commit or role handover. Closing is atomic with entering an operation.
use std::sync::{Arc, Mutex};
#[derive(Default)]
struct State {
    active: usize,
    closed: bool,
}
#[derive(Clone, Default)]
pub struct Activity(Arc<Mutex<State>>);
pub struct Guard(Activity);
impl Activity {
    pub fn enter(&self) -> crate::failure::Result<Guard> {
        let mut state = self.0.lock().expect("sync activity lock");
        if state.closed {
            return Err(crate::service_configuration::pending());
        }
        state.active += 1;
        Ok(Guard(self.clone()))
    }
    pub fn close_if_idle(&self) -> bool {
        let mut state = self.0.lock().expect("sync activity lock");
        if state.active != 0 || state.closed {
            return false;
        }
        state.closed = true;
        true
    }
    pub fn reopen(&self) {
        self.0.lock().expect("sync activity lock").closed = false;
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.0.lock().expect("sync activity lock").active -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn close_waits_for_every_nested_operation_and_refuses_new_ones() {
        let activity = Activity::default();
        let first = activity.enter().unwrap();
        let nested = activity.enter().unwrap();
        assert!(!activity.close_if_idle());
        drop(first);
        assert!(!activity.close_if_idle());
        drop(nested);
        assert!(activity.close_if_idle());
        assert!(activity.enter().is_err());
        activity.reopen();
        assert!(activity.enter().is_ok());
    }
}
