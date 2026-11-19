//! Window upgrade and process handoff boundary.

pub(crate) trait WindowUpgrade: Send + Sync {
    fn replace_and_reopen(&self) -> Result<(), String>;
    fn start_release(&self) -> Result<elsecall_lifecycle::release_update::Job, String>;
    /// Called only after a fresh instance has been launched successfully.
    fn close_old_window(&self);
}

pub(crate) enum UpgradeState {
    Idle,
    Installing,
    StartingRelease,
    Retiring,
    Release(elsecall_lifecycle::release_update::JobId),
    Failed(String),
}

pub(crate) fn available(installed: Option<&str>, running: Option<&str>) -> bool {
    matches!((installed, running), (Some(installed), Some(running)) if installed != running)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_a_known_different_window_is_offered() {
        assert!(!available(None, Some("aaaa")));
        assert!(!available(Some("aaaa"), None));
        assert!(!available(Some("aaaa"), Some("aaaa")));
        assert!(available(Some("bbbb"), Some("aaaa")));
    }
}
