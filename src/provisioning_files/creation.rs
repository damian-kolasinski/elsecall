//! Creation authority is retained outside storage before its first mutation.
use super::{
    Session, conflict,
    directory::Directory,
    identity::{Identity, directory_identity, inspect},
    io, private_acl,
    receipt::{Record, Storage},
    receipt_location,
};
use crate::{failure::Result, onboarding_id::OperationId};
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

/// Store this in the private coordinator journal before `start`. It reserves one
/// absent slot inside private coordinator storage, including partial creation.
/// This is local trusted journal data, never a network request or root capability.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Creation {
    version: u32,
    id: OperationId,
    root: PathBuf,
    root_identity: Identity,
    receipt: PathBuf,
    parent: Identity,
}
impl Creation {
    /// No mutation. The caller allocates the receipt name in its private namespace
    /// and must not use that slot for unrelated files while retaining this ticket.
    pub fn prepare(root: &Path, receipt: &Path) -> Result<Self> {
        crate::local_access::require_user()?;
        let root_path = root.canonicalize().map_err(io)?;
        let root = Directory::open(&root_path)?;
        let (parent, name, receipt) = receipt_location(receipt)?;
        let root_identity = directory_identity(&root)?;
        let parent_identity = directory_identity(&parent)?;
        if receipt.starts_with(&root_path)
            || root_path.starts_with(&receipt)
            || parent_identity.device != root_identity.device
            || parent.stat(&name)?.is_some()
        {
            return Err(conflict());
        }
        Ok(Self {
            version: 1,
            id: OperationId::generate()?,
            root: root_path,
            root_identity,
            receipt,
            parent: parent_identity,
        })
    }
    /// Retry after a process interruption, even before the first receipt committed.
    /// Only reserved creation metadata is allowed in a partially created slot.
    pub fn start(&self) -> Result<Session> {
        crate::local_access::require_user()?;
        if self.version != 1 {
            return Err(conflict());
        }
        let (parent, name, receipt_path) = receipt_location(&self.receipt)?;
        let root = Directory::open(&self.root)?;
        if directory_identity(&parent)? != self.parent
            || directory_identity(&root)? != self.root_identity
        {
            return Err(conflict());
        }
        let exists = parent.stat(&name)?.is_some();
        if exists {
            let directory = Directory(parent.open_node(&name, libc::O_DIRECTORY)?);
            if directory.0.metadata().map_err(io)?.mode() & 0o077 != 0 {
                return Err(conflict());
            }
            private_acl::check(&directory.0)?;
            if directory
                .stat(&CString::new("receipt.json").unwrap())?
                .is_some()
            {
                let session = Session::resume(&receipt_path)?;
                if session.record.creation != self.id || session.record.root != self.root {
                    return Err(conflict());
                }
                return Ok(session);
            }
            if directory
                .names()?
                .iter()
                .any(|n| n != b"lock" && n != b"receipt.next")
            {
                return Err(conflict());
            }
        } else {
            parent.mkdir(&name)?;
        }
        let directory = Directory(parent.open_node(&name, libc::O_DIRECTORY)?);
        // The private parent has no inheritable grants. Clearing newly-created
        // storage still explicitly establishes the intended macOS ACL boundary.
        if !exists {
            private_acl::clear(&directory.0)?;
        }
        let identity = inspect(&parent, &name)?.ok_or_else(conflict)?;
        let lock_exists = directory.stat(&CString::new("lock").unwrap())?.is_some();
        let storage = Storage::lock(directory, !lock_exists)?;
        let record = Record {
            version: 1,
            creation: self.id.clone(),
            root: self.root.clone(),
            root_device: self.root_identity.device as u64,
            root_inode: self.root_identity.inode,
            storage: identity,
            cleaning: false,
            finalizing: false,
            entries: Vec::new(),
        };
        storage.save(&record)?;
        Ok(Session {
            root,
            storage,
            record,
            receipt_parent: parent,
            receipt_name: name,
            receipt_path,
        })
    }
}
