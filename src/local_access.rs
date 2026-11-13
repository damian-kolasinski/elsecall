use crate::failure::{Failure, Result};

pub fn user_id() -> u32 {
    // SAFETY: geteuid has no preconditions and does not mutate memory.
    unsafe { libc::geteuid() }
}

pub fn authorize_peer(peer_uid: u32) -> Result<()> {
    if peer_uid == user_id() {
        Ok(())
    } else {
        Err(Failure::new(
            "access_denied",
            "The local service belongs to another user.",
            "Run Elsecall as the user who installed it.",
        ))
    }
}

pub fn require_user() -> Result<()> {
    if user_id() == 0 {
        Err(Failure::new(
            "unsupported_user",
            "Elsecall runs as a regular user, not root.",
            "Run without sudo from a logged-in macOS account.",
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_same_user_is_authorized() {
        assert!(super::authorize_peer(super::user_id()).is_ok());
        assert!(super::authorize_peer(super::user_id() + 1).is_err());
    }
}
