//! Same-user onboarding lifecycle. No installer is supported by this release yet.
//! Records contain only status, never invitations, signing keys or credentials.
use crate::{
    failure::{Failure, Result},
    onboarding_id::OperationId,
    onboarding_invitation::LIFETIME,
    private_files,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
    sync::Mutex,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Blocked,
    Cancelled,
    Expired,
}

/// Reserved wire shape. This release never constructs a transferable artifact.
#[derive(Debug, Serialize)]
pub struct Artifact {
    pub path: PathBuf,
    pub format: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Operation {
    pub version: u32,
    pub id: OperationId,
    pub expires_at: u64,
    pub stage: Stage,
    pub artifact: Option<Artifact>,
    pub error: Option<Failure>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    id: OperationId,
    expires_at: u64,
    stage: Stage,
}

pub struct Operations {
    directory: PathBuf,
    changes: Mutex<()>,
}
impl Operations {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            changes: Mutex::new(()),
        }
    }
    pub fn prepare(&self, now: u64) -> Result<Operation> {
        let _lock = self.changes.lock().unwrap();
        private_files::directory(&self.directory)?;
        let record = Record {
            version: 1,
            id: OperationId::generate()?,
            expires_at: now.checked_add(LIFETIME).ok_or_else(invalid)?,
            stage: Stage::Blocked,
        };
        self.save(&record)?;
        Ok(record.into_operation())
    }
    pub fn status(&self, id: &OperationId, now: u64, cancel: bool) -> Result<Operation> {
        let _lock = self.changes.lock().unwrap();
        private_files::check_directory(&self.directory).map_err(|_| unknown())?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.path(id))
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    unknown()
                } else {
                    invalid()
                }
            })?;
        let meta = file.metadata().map_err(|_| invalid())?;
        if !meta.is_file()
            || meta.uid() != crate::local_access::user_id()
            || meta.mode() & 0o077 != 0
        {
            return Err(invalid());
        }
        let mut bytes = Vec::new();
        file.take(4097)
            .read_to_end(&mut bytes)
            .map_err(|_| invalid())?;
        if bytes.len() > 4096 {
            return Err(invalid());
        }
        let mut record: Record = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if record.version != 1 || record.id != *id {
            return Err(invalid());
        }
        if record.stage == Stage::Blocked {
            if now >= record.expires_at {
                record.stage = Stage::Expired;
            } else if cancel {
                record.stage = Stage::Cancelled;
            }
            if record.stage != Stage::Blocked {
                self.save(&record)?;
            }
        }
        Ok(record.into_operation())
    }
    fn path(&self, id: &OperationId) -> PathBuf {
        self.directory.join(format!("{}.json", id.as_str()))
    }
    fn save(&self, record: &Record) -> Result<()> {
        private_files::write(
            &self.path(&record.id),
            &serde_json::to_vec(record).expect("status serializes"),
            0o600,
        )
    }
}
impl Record {
    fn into_operation(self) -> Operation {
        Operation { version: self.version, id: self.id, expires_at: self.expires_at, stage: self.stage, artifact: None,
            error: (self.stage == Stage::Blocked).then(|| Failure::new(
                "onboarding_installer_unavailable", "This release has no verified plain-Mac installer.",
                "A signed and notarized installer with invitation support must be published before Add Mac can send it.")) }
    }
}
fn unknown() -> Failure {
    Failure::new(
        "onboarding_operation_not_found",
        "This onboarding operation was not found.",
        "Start Add Mac again.",
    )
}
fn invalid() -> Failure {
    Failure::new(
        "onboarding_state_unavailable",
        "The private onboarding status is unreadable.",
        "Inspect onboarding state permissions before retrying.",
    )
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
