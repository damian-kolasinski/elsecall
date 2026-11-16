//! Retryable removal evidence lives with the coordinator, outside removed storage.
use super::{
    Session, conflict,
    directory::Directory,
    identity::{Identity, directory_identity, inspect},
    io, private_acl, receipt_location,
};
use crate::failure::Result;
use serde::{Deserialize, Serialize};
use std::{ffi::CString, os::unix::fs::MetadataExt, path::PathBuf};

/// Private coordinator journal data. Never accept a ticket from a network request
/// or use it with elevated privileges. Its owner is the same local user.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finalization {
    version: u32,
    receipt: PathBuf,
    directory: Identity,
    record: Identity,
    lock: Identity,
    parent: Identity,
    removed: bool,
}
impl Finalization {
    pub(super) fn new(session: &Session) -> Result<Self> {
        Ok(Self {
            version: 1,
            removed: false,
            receipt: session.receipt_path.clone(),
            directory: session.record.storage.clone(),
            record: inspect(
                &session.storage.directory,
                &CString::new("receipt.json").unwrap(),
            )?
            .ok_or_else(conflict)?,
            lock: inspect(&session.storage.directory, &CString::new("lock").unwrap())?
                .ok_or_else(conflict)?,
            parent: directory_identity(&session.receipt_parent)?,
        })
    }
    /// Idempotent after success, and retryable when only part of the metadata was
    /// removed. Persist this ticket again after each attempt: its removal evidence is
    /// now set. If interrupted after rmdir but before that evidence is saved, a
    /// missing directory remains unverified and needs independent inspection.
    /// This is not proof of service/dependency/device cleanup.
    pub fn run(&mut self) -> Result<()> {
        crate::local_access::require_user()?;
        if self.version != 1 {
            return Err(conflict());
        }
        let (parent, name, _) = receipt_location(&self.receipt)?;
        if directory_identity(&parent)? != self.parent {
            return Err(conflict());
        }
        let Some(identity) = inspect(&parent, &name)? else {
            return if self.removed {
                parent.sync()
            } else {
                Err(conflict())
            };
        };
        if self.removed || identity != self.directory {
            return Err(conflict());
        }
        let directory = Directory(parent.open_node(&name, libc::O_DIRECTORY)?);
        let metadata = directory.0.metadata().map_err(io)?;
        if (metadata.dev(), metadata.ino()) != (identity.device as u64, identity.inode) {
            return Err(conflict());
        }
        private_acl::check(&directory.0)?;
        let lock_name = CString::new("lock").unwrap();
        let lock = if let Some(identity) = inspect(&directory, &lock_name)? {
            if identity != self.lock {
                return Err(conflict());
            }
            let lock = directory.open_node(&lock_name, 0)?;
            lock.try_lock().map_err(|_| conflict())?;
            Some(lock)
        } else {
            None
        };
        if directory
            .names()?
            .iter()
            .any(|n| n != b"receipt.json" && n != b"lock")
        {
            return Err(conflict());
        }
        let record_name = CString::new("receipt.json").unwrap();
        if let Some(identity) = inspect(&directory, &record_name)? {
            if identity != self.record {
                return Err(conflict());
            }
            directory.remove(&record_name, false)?.durability?;
        }
        if lock.is_some() {
            directory.remove(&lock_name, false)?.durability?;
        }
        if inspect(&parent, &name)?.as_ref() != Some(&self.directory) {
            return Err(conflict());
        }
        let removal = parent.remove(&name, true)?;
        self.removed = true;
        removal.durability
    }
}
