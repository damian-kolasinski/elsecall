//! Private durable receipt storage. Swapped-out originals are secret restoration
//! storage, not status artifacts. Callers never serialize this record in diagnostics.
use super::{RelativePath, conflict, directory::Directory, identity::Identity, io};
use crate::failure::Result;
use serde::{Deserialize, Serialize};
use std::{ffi::CString, fs::File, io::Read, os::unix::fs::MetadataExt, path::PathBuf};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub version: u32,
    pub creation: crate::onboarding_id::OperationId,
    pub root: PathBuf,
    pub root_device: u64,
    pub root_inode: u64,
    pub storage: Identity,
    pub cleaning: bool,
    pub finalizing: bool,
    pub entries: Vec<Entry>,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "ownership", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Entry {
    Reused {
        path: RelativePath,
    },
    Changed {
        path: RelativePath,
        prior: Option<Identity>,
        installed: Option<Box<Identity>>,
        staging_kind: StagingKind,
        captured: bool,
        restored: bool,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum StagingKind {
    File,
    Directory,
    Link,
}
impl StagingKind {
    pub fn matches(&self, identity: &Identity) -> bool {
        matches!(
            (self, &identity.node),
            (Self::File, super::identity::Node::File { .. })
                | (Self::Directory, super::identity::Node::Directory { .. })
                | (Self::Link, super::identity::Node::Link { .. })
        )
    }
}
pub(super) struct Storage {
    pub directory: Directory,
    _lock: File,
}
impl Storage {
    pub fn lock(directory: Directory, create: bool) -> Result<Self> {
        let lock = CString::new("lock").unwrap();
        let file = if create {
            directory.create_file(&lock, b"", 0o600)?
        } else {
            directory.open_node(&lock, 0)?
        };
        let meta = file.metadata().map_err(io)?;
        if !meta.is_file()
            || meta.uid() != crate::local_access::user_id()
            || meta.mode() & 0o077 != 0
            || meta.nlink() != 1
        {
            return Err(conflict());
        }
        super::private_acl::check(&file)?;
        file.try_lock().map_err(|_| conflict())?;
        Ok(Self {
            directory,
            _lock: file,
        })
    }
    pub fn load(&self) -> Result<Record> {
        let file = self
            .directory
            .open_node(&CString::new("receipt.json").unwrap(), 0)?;
        let metadata = file.metadata().map_err(io)?;
        if !metadata.is_file()
            || metadata.uid() != crate::local_access::user_id()
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
            || metadata.len() > 8 * 1024 * 1024
        {
            return Err(conflict());
        }
        super::private_acl::check(&file)?;
        let mut bytes = Vec::new();
        file.take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        serde_json::from_slice(&bytes).map_err(|_| conflict())
    }
    pub fn save(&self, record: &Record) -> Result<()> {
        self.recover_write()?;
        let temporary = CString::new("receipt.next").unwrap();
        let bytes = serde_json::to_vec(record).expect("receipt serializes");
        if bytes.len() > 8 * 1024 * 1024 {
            return Err(conflict());
        }
        self.directory.create_file(&temporary, &bytes, 0o600)?;
        let target = CString::new("receipt.json").unwrap();
        // SAFETY: valid descriptors/names. Atomic replacement is restricted to
        // our receipt metadata, never an installed or pre-existing user file.
        if unsafe {
            libc::renameat(
                self.directory.0.as_raw_fd(),
                temporary.as_ptr(),
                self.directory.0.as_raw_fd(),
                target.as_ptr(),
            )
        } != 0
        {
            return Err(io(std::io::Error::last_os_error()));
        }
        self.directory.sync()
    }
    pub fn recover_write(&self) -> Result<()> {
        let temporary = CString::new("receipt.next").unwrap();
        if let Some(meta) = self.directory.stat(&temporary)? {
            if meta.st_uid != crate::local_access::user_id()
                || meta.st_mode & libc::S_IFMT != libc::S_IFREG
                || meta.st_mode & 0o077 != 0
                || meta.st_nlink != 1
            {
                return Err(conflict());
            }
            // receipt.json is authoritative: a next file was never committed.
            self.directory.remove(&temporary, false)?.durability?;
        }
        Ok(())
    }
}
use std::os::fd::AsRawFd;
