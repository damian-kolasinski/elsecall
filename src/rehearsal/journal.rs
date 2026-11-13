//! Owner-only recovery state. It contains only synthetic scratch paths and
//! identities of synthetic callers, never consumer inputs or secret values.
use crate::{
    failure::{Failure, Result},
    paths::Paths,
    private_files,
    scheduling::{processes::ProcessIdentity, timestamp::Timestamp},
};
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    path::{Path, PathBuf},
};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub session: String,
    pub root: PathBuf,
    pub until: Timestamp,
    pub callers: Vec<ProcessIdentity>,
}
pub fn path(paths: &Paths) -> PathBuf {
    paths.data.join("rehearsal-session.json")
}
impl Journal {
    pub fn save(&self, paths: &Paths) -> Result<()> {
        private_files::replace(
            &path(paths),
            &serde_json::to_vec(self).expect("recovery state serializes"),
        )
        .map_err(|e| Failure::io("Save rehearsal recovery state", e))
    }
    pub fn read(paths: &Paths, member: &str) -> Result<Option<Self>> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path(paths))
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Failure::io("Read rehearsal recovery state", e)),
        };
        let mut bytes = Vec::new();
        file.take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| Failure::io("Read rehearsal recovery state", e))?;
        let state: Self = serde_json::from_slice(&bytes)
            .map_err(|_| super::invalid("Invalid rehearsal recovery state."))?;
        let prefix = format!("elsecall-rehearsal-{}-{member}-", state.session);
        if bytes.len() > 1024 * 1024
            || !super::protocol::valid_session(&state.session)
            || state.callers.len() > 10000
            || state.root.parent() != Some(Path::new("/private/tmp"))
            || !state
                .root
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix))
        {
            return Err(super::invalid("Invalid rehearsal recovery identity."));
        }
        Ok(Some(state))
    }
}
pub fn clear(paths: &Paths) -> Result<()> {
    match std::fs::remove_file(path(paths)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Failure::io("Remove rehearsal recovery state", e)),
    }
}
