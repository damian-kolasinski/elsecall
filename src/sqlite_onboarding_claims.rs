//! Persistent single-use invitation ledger. No credentials or signed artifacts stored.
use crate::{
    failure::{Failure, Result},
    member_key::PublicKey,
    onboarding_claim::InvitationUses,
    private_files,
};
use rusqlite::{Connection, params};
use std::{
    fs::OpenOptions,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    sync::Mutex,
};

pub struct ClaimLedger(Mutex<Connection>);
impl ClaimLedger {
    /// Open inside the service's owner-only state directory. The directory is part
    /// of the trust boundary; another account must not replace the database.
    pub fn open(directory: &Path) -> Result<Self> {
        private_files::directory(directory)?;
        let path = directory.join("claims.sqlite");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|_| unavailable())?;
        let meta = file.metadata().map_err(|_| unavailable())?;
        if !meta.is_file()
            || meta.uid() != crate::local_access::user_id()
            || meta.mode() & 0o077 != 0
        {
            return Err(unavailable());
        }
        let db = Connection::open(path).map_err(|_| unavailable())?;
        db.execute_batch("PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS invitation_uses (invitation BLOB PRIMARY KEY NOT NULL, candidate TEXT NOT NULL);")
            .map_err(|_| unavailable())?;
        Ok(Self(Mutex::new(db)))
    }
}
impl InvitationUses for ClaimLedger {
    fn consume(&self, invitation: &[u8; 32], candidate: PublicKey) -> Result<()> {
        let db = self.0.lock().unwrap();
        match db.execute(
            "INSERT INTO invitation_uses (invitation, candidate) VALUES (?1, ?2)",
            params![invitation.as_slice(), candidate.to_string()],
        ) {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(Failure::new(
                    "onboarding_invitation_used",
                    "This invitation has already been claimed.",
                    "Request a new invitation; a retry cannot select a second machine.",
                ))
            }
            Err(_) => Err(unavailable()),
        }
    }
}
fn unavailable() -> Failure {
    Failure::new(
        "onboarding_claim_storage_unavailable",
        "The private claim ledger is unavailable.",
        "Inspect onboarding state permissions before retrying.",
    )
}
