//! Window upgrade and process handoff boundary.

pub trait WindowUpgrade: Send + Sync {
    fn replace_and_reopen(&self) -> Result<(), String>;
    /// Called only after a fresh instance has been launched successfully.
    fn close_old_window(&self);
}

pub enum UpgradeState {
    Idle,
    Installing,
    Failed(String),
}

pub fn available(installed: Option<&str>, running: Option<&str>) -> bool {
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
