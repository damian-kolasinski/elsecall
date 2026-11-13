//! Durable record of this member's fleet membership, in Elsecall's state directory.
use crate::{
    failure::{Failure, Result},
    local_access::user_id,
    private_files,
    roster::Roster,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const LIMIT: u64 = 1024 * 1024;

/// Whether this machine belongs to a fleet. A solo machine is the master of a fleet
/// of one and trusts no other key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Membership {
    Solo,
    Enrolled(Roster),
}

#[derive(Serialize, Deserialize)]
struct Record {
    schema_version: u32,
    roster: Roster,
}

pub struct MembershipStore {
    path: PathBuf,
}
impl MembershipStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<Membership> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&self.path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Membership::Solo),
            Err(e) => return Err(Failure::io("Open fleet membership", e)),
        };
        let meta = file
            .metadata()
            .map_err(|e| Failure::io("Inspect fleet membership", e))?;
        if !meta.is_file() || meta.uid() != user_id() || meta.mode() & 0o077 != 0 {
            return Err(unreadable(&self.path));
        }
        let mut bytes = Vec::new();
        file.take(LIMIT)
            .read_to_end(&mut bytes)
            .map_err(|e| Failure::io("Read fleet membership", e))?;
        match serde_json::from_slice::<Record>(&bytes) {
            Ok(record) if record.schema_version == 1 => Ok(Membership::Enrolled(record.roster)),
            _ => Err(unreadable(&self.path)),
        }
    }

    pub fn save(&self, membership: &Membership) -> Result<()> {
        match membership {
            Membership::Solo => private_files::remove_file(&self.path),
            Membership::Enrolled(roster) => {
                let bytes = serde_json::to_vec_pretty(&Record {
                    schema_version: 1,
                    roster: roster.clone(),
                })
                .expect("serializable roster");
                private_files::write(&self.path, &bytes, 0o600)
            }
        }
    }
}

fn unreadable(path: &Path) -> Failure {
    Failure::new(
        "invalid_fleet_state",
        "The fleet membership record is unreadable or not owner-only.",
        format!(
            "Inspect {}; move it aside to leave the fleet, then enrol this member again.",
            path.display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roster::tests::member;
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn round_trips_and_rejects_tampered_or_exposed_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        let store = MembershipStore::new(path.clone());
        assert_eq!(store.load().unwrap(), Membership::Solo);
        let enrolled = Membership::Enrolled(Roster::solo(member("a", 1)));
        store.save(&enrolled).unwrap();
        assert_eq!(store.load().unwrap(), enrolled);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(store.load().unwrap_err().code, "invalid_fleet_state");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let text = fs::read_to_string(&path)
            .unwrap()
            .replace("\"master\": \"a\"", "\"master\": \"b\"");
        fs::write(&path, text).unwrap();
        assert_eq!(store.load().unwrap_err().code, "invalid_fleet_state");
        store.save(&Membership::Solo).unwrap();
        assert!(!path.exists());
    }
}
