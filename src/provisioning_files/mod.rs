//! Ownership-aware per-user file provisioning. Every publication has a durable
//! reverse operation first. This is not a privileged installation interface.
mod creation;
mod directory;
pub use creation::Creation;
mod finalization;
mod identity;
mod private_acl;
mod receipt;
pub use finalization::Finalization;

use crate::failure::{Failure, Result};
use directory::Directory;
use identity::{Identity, Node, inspect};
use receipt::{Entry, Record, StagingKind, Storage};
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelativePath(String);
impl RelativePath {
    fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for RelativePath {
    type Error = Failure;
    fn try_from(value: String) -> Result<Self> {
        if value.is_empty()
            || value.len() > 1024
            || value.contains('\0')
            || value
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == ".." || part.len() > 255)
        {
            return Err(conflict());
        }
        Ok(Self(value))
    }
}
impl From<RelativePath> for String {
    fn from(value: RelativePath) -> Self {
        value.0
    }
}

#[derive(Clone, Copy)]
pub enum ExistingFile {
    Refuse,
    Reuse,
    RestoreOnDetach,
}
#[derive(Clone, Copy)]
pub enum FileMode {
    Private,
    Executable,
    Public,
    PublicExecutable,
}
impl FileMode {
    fn bits(self) -> u16 {
        match self {
            Self::Private => 0o600,
            Self::Executable => 0o700,
            Self::Public => 0o644,
            Self::PublicExecutable => 0o755,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    Introduced,
    Reused,
    Restorable,
}
#[derive(Debug, Serialize)]
pub struct Cleanup {
    pub clean: bool,
    pub pending: Vec<Pending>,
}
#[derive(Debug, Serialize)]
pub struct Pending {
    /// Receipt-local index. Paths and prior contents stay private.
    pub entry: Option<usize>,
    pub error: Failure,
}

/// An exclusive session, scoped to an already existing user-owned directory.
/// The separate receipt directory must be new, private and on the same volume.
/// Retain it until the teardown coordinator has acknowledged cleanup evidence.
pub struct Session {
    root: Directory,
    storage: Storage,
    record: Record,
    receipt_parent: Directory,
    receipt_name: CString,
    receipt_path: PathBuf,
}
impl Session {
    pub fn resume(receipt: &Path) -> Result<Self> {
        crate::local_access::require_user()?;
        let (parent, name, receipt_path) = receipt_location(receipt)?;
        let directory = Directory::open(&receipt_path)?;
        let meta = directory.0.metadata().map_err(io)?;
        if meta.mode() & 0o077 != 0 {
            return Err(conflict());
        }
        private_acl::check(&directory.0)?;
        let storage = Storage::lock(directory, false)?;
        let record = storage.load()?;
        if record.version != 1
            || record.entries.len() > 4096
            || !record.root.is_absolute()
            || receipt_path.starts_with(&record.root)
            || record.root.starts_with(&receipt_path)
            || record.root.canonicalize().map_err(io)? != record.root
        {
            return Err(conflict());
        }
        if inspect(&parent, &name)?.as_ref() != Some(&record.storage) {
            return Err(conflict());
        }
        let root = Directory::open(&record.root)?;
        let root_meta = root.0.metadata().map_err(io)?;
        if (root_meta.dev(), root_meta.ino()) != (record.root_device, record.root_inode) {
            return Err(conflict());
        }
        storage.recover_write()?;
        Ok(Self {
            root,
            storage,
            record,
            receipt_parent: parent,
            receipt_name: name,
            receipt_path,
        })
    }
    pub fn put(
        &mut self,
        path: RelativePath,
        bytes: &[u8],
        mode: FileMode,
        existing: ExistingFile,
    ) -> Result<Ownership> {
        self.active()?;
        if bytes.len() > 256 * 1024 * 1024 {
            return Err(conflict());
        }
        let (parent, name) = self.root.parent(&path)?;
        let prior = inspect(&parent, &name)?;
        if let Some(identity) = &prior {
            if !matches!(identity.node, Node::File { .. }) {
                return Err(conflict());
            }
            match existing {
                ExistingFile::Refuse => return Err(conflict()),
                ExistingFile::Reuse => return self.reuse(path),
                ExistingFile::RestoreOnDetach => (),
            }
        }
        let index = self.reserve(path, prior, StagingKind::File)?;
        self.storage
            .directory
            .create_file(&staged(index), bytes, mode.bits())?;
        self.publish(index, &parent, &name)
    }
    pub fn directory(&mut self, path: RelativePath) -> Result<Ownership> {
        self.active()?;
        let (parent, name) = self.root.parent(&path)?;
        if let Some(prior) = inspect(&parent, &name)? {
            if !prior.is_directory() {
                return Err(conflict());
            }
            return self.reuse(path);
        }
        let index = self.reserve(path, None, StagingKind::Directory)?;
        self.storage.directory.mkdir(&staged(index))?;
        self.publish(index, &parent, &name)
    }
    /// New links point only inside this session's root. Pre-existing links are
    /// deliberately refused; deciding whether an installation is reusable is separate policy.
    pub fn link(&mut self, path: RelativePath, target: RelativePath) -> Result<Ownership> {
        self.active()?;
        let (parent, name) = self.root.parent(&path)?;
        if inspect(&parent, &name)?.is_some() {
            return Err(conflict());
        }
        let (target_parent, target_name) = self.root.parent(&target)?;
        if !matches!(
            inspect(&target_parent, &target_name)?.map(|identity| identity.node),
            Some(Node::File { .. })
        ) {
            return Err(conflict());
        }
        let target = CString::new(
            self.record
                .root
                .join(target.as_str())
                .as_os_str()
                .as_encoded_bytes(),
        )
        .map_err(|_| conflict())?;
        let index = self.reserve(path, None, StagingKind::Link)?;
        self.storage.directory.symlink(&staged(index), &target)?;
        self.publish(index, &parent, &name)
    }
    fn active(&self) -> Result<()> {
        self.private_storage()?;
        if self.record.cleaning || self.record.entries.len() >= 4096 {
            return Err(conflict());
        }
        Ok(())
    }
    fn private_storage(&self) -> Result<()> {
        let metadata = self.storage.directory.0.metadata().map_err(io)?;
        if metadata.uid() != crate::local_access::user_id()
            || metadata.mode() & 0o077 != 0
            || inspect(&self.receipt_parent, &self.receipt_name)?.as_ref()
                != Some(&self.record.storage)
        {
            return Err(conflict());
        }
        private_acl::check(&self.storage.directory.0)
    }
    fn reuse(&mut self, path: RelativePath) -> Result<Ownership> {
        self.record.entries.push(Entry::Reused { path });
        if let Err(error) = self.storage.save(&self.record) {
            self.record.entries.pop();
            return Err(error);
        }
        Ok(Ownership::Reused)
    }
    fn reserve(
        &mut self,
        path: RelativePath,
        prior: Option<Identity>,
        staging_kind: StagingKind,
    ) -> Result<usize> {
        let index = self.record.entries.len();
        self.record.entries.push(Entry::Changed {
            path,
            prior,
            installed: None,
            staging_kind,
            captured: false,
            restored: false,
        });
        if let Err(error) = self.storage.save(&self.record) {
            self.record.entries.pop();
            return Err(error);
        }
        Ok(index)
    }
    fn publish(
        &mut self,
        index: usize,
        parent: &Directory,
        name: &std::ffi::CStr,
    ) -> Result<Ownership> {
        let stage = staged(index);
        let after = inspect(&self.storage.directory, &stage)?.ok_or_else(conflict)?;
        let Entry::Changed {
            prior, installed, ..
        } = &mut self.record.entries[index]
        else {
            unreachable!()
        };
        let before = prior.clone();
        *installed = Some(Box::new(after.clone()));
        // This must be durable before the first change to the destination.
        if let Err(error) = self.storage.save(&self.record) {
            let Entry::Changed { installed, .. } = &mut self.record.entries[index] else {
                unreachable!()
            };
            *installed = None;
            return Err(error);
        }
        if inspect(parent, name)? != before {
            return Err(conflict());
        }
        self.storage
            .directory
            .rename(&stage, parent, name, before.is_some())?;
        if before.is_some() && inspect(&self.storage.directory, &stage)? != before {
            // A concurrent writer won between observation and swap. Preserve its
            // object by putting it back, provided our publication is still intact.
            if inspect(parent, name)? == Some(after) {
                self.storage.directory.rename(&stage, parent, name, true)?;
            }
            return Err(conflict());
        }
        Ok(if before.is_some() {
            Ownership::Restorable
        } else {
            Ownership::Introduced
        })
    }
    /// Permanently refuses further provisioning, then restores in reverse order.
    /// It keeps going after independent failures and never equates an absent
    /// connection, pathname, or replaced file with verified ownership.
    pub fn cleanup(&mut self) -> Result<Cleanup> {
        self.private_storage()?;
        if self.record.finalizing {
            let clean = self.finalizing_is_clean()?;
            return Ok(Cleanup {
                clean,
                pending: if clean {
                    Vec::new()
                } else {
                    vec![Pending {
                        entry: None,
                        error: conflict(),
                    }]
                },
            });
        }
        self.record.cleaning = true;
        self.storage.save(&self.record)?;
        let mut pending = Vec::new();
        for index in (0..self.record.entries.len()).rev() {
            if let Err(error) = self.undo(index) {
                pending.push(Pending {
                    entry: Some(index),
                    error,
                });
            }
        }
        if self
            .storage
            .directory
            .names()?
            .iter()
            .any(|name| name != b"receipt.json" && name != b"lock")
        {
            pending.push(Pending {
                entry: None,
                error: conflict(),
            });
        }
        Ok(Cleanup {
            clean: pending.is_empty(),
            pending,
        })
    }
    fn undo(&mut self, index: usize) -> Result<()> {
        let Entry::Changed {
            path,
            prior,
            installed,
            staging_kind,
            captured,
            restored,
        } = &self.record.entries[index]
        else {
            return Ok(());
        };
        if *restored {
            return Ok(());
        }
        let path = path.clone();
        let prior = prior.clone();
        let after = installed.as_deref().cloned();
        let was_captured = *captured;
        let stage = staged(index);
        let staged_identity = inspect(&self.storage.directory, &stage)?;
        let Some(after) = after else {
            // No destination mutation is allowed before installed is durable.
            // The creation capability reserves this exact private scratch slot.
            if let Some(identity) = staged_identity {
                if !staging_kind.matches(&identity) {
                    return Err(conflict());
                }
                self.storage
                    .directory
                    .remove(&stage, identity.is_directory())?
                    .durability?;
            }
            return self.restored(index);
        };
        if staged_identity.as_ref() == Some(&after) {
            // Unpublished scratch or an interrupted restoration/quarantine. The
            // exact introduced inode is here, so unrelated destinations survive.
            return self.discard_staged(index, &path, &after);
        }
        let (parent, name) = self.root.parent(&path)?;
        let current = inspect(&parent, &name)?;
        if current == prior && staged_identity.is_none() && was_captured {
            // The private-slot deletion was authorized durably after capture.
            return self.restored(index);
        }
        if current.as_ref() != Some(&after) || staged_identity != prior {
            return Err(conflict());
        }
        if prior.is_some() {
            parent.rename(&name, &self.storage.directory, &stage, true)?;
            if inspect(&parent, &name)? != prior
                || inspect(&self.storage.directory, &stage)?.as_ref() != Some(&after)
            {
                if inspect(&parent, &name)? == prior {
                    parent.rename(&name, &self.storage.directory, &stage, true)?;
                }
                return Err(conflict());
            }
        } else {
            // Capture before deleting: a pathname replacement is never unlinked.
            parent.rename(&name, &self.storage.directory, &stage, false)?;
            if inspect(&self.storage.directory, &stage)?.as_ref() != Some(&after) {
                self.storage
                    .directory
                    .rename(&stage, &parent, &name, false)?;
                return Err(conflict());
            }
        }
        self.discard_staged(index, &path, &after)
    }
    fn captured(&mut self, index: usize, value: bool) -> Result<()> {
        let Entry::Changed { captured, .. } = &mut self.record.entries[index] else {
            unreachable!()
        };
        *captured = value;
        self.storage.save(&self.record)
    }
    fn discard_staged(
        &mut self,
        index: usize,
        path: &RelativePath,
        after: &Identity,
    ) -> Result<()> {
        self.captured(index, true)?;
        let removal = match self
            .storage
            .directory
            .remove(&staged(index), after.is_directory())
        {
            Ok(removal) => removal,
            Err(error) => {
                if after.is_directory() {
                    // Reset first: a crash during rollback must not leave stale
                    // deletion authority over an object returned to the user.
                    self.captured(index, false)?;
                    let (parent, name) = self.root.parent(path)?;
                    if parent.stat(&name)?.is_none() {
                        self.storage
                            .directory
                            .rename(&staged(index), &parent, &name, false)?;
                    }
                }
                return Err(error);
            }
        };
        // Successful unlink keeps captured=true even if the subsequent sync
        // fails. A retry can finish instead of attempting an impossible rollback.
        removal.durability?;
        self.restored(index)
    }
    fn restored(&mut self, index: usize) -> Result<()> {
        let Entry::Changed { restored, .. } = &mut self.record.entries[index] else {
            unreachable!()
        };
        *restored = true;
        self.storage.save(&self.record)
    }
    /// Prepare final removal. The caller must durably retain the returned ticket
    /// outside the receipt before running it. It survives interruption even after
    /// the last receipt file is gone; no finalization evidence is self-deleted.
    pub fn finalization(mut self) -> Result<Finalization> {
        self.private_storage()?;
        if self.record.finalizing {
            if !self.finalizing_is_clean()? {
                return Err(conflict());
            }
            return Finalization::new(&self);
        }
        if !self.record.cleaning || !self.cleanup()?.clean {
            return Err(conflict());
        }
        let names = self.storage.directory.names()?;
        if names.iter().any(|n| n != b"receipt.json" && n != b"lock") {
            return Err(conflict());
        }
        if inspect(&self.receipt_parent, &self.receipt_name)?.as_ref() != Some(&self.record.storage)
        {
            return Err(conflict());
        }
        self.record.finalizing = true;
        self.storage.save(&self.record)?;
        Finalization::new(&self)
    }
    fn finalizing_is_clean(&self) -> Result<bool> {
        Ok(self.record.entries.iter().all(|entry| {
            matches!(
                entry,
                Entry::Reused { .. } | Entry::Changed { restored: true, .. }
            )
        }) && self
            .storage
            .directory
            .names()?
            .iter()
            .all(|name| name == b"receipt.json" || name == b"lock"))
    }
}
fn receipt_location(path: &Path) -> Result<(Directory, CString, PathBuf)> {
    if !path.is_absolute()
        || path
            .components()
            .any(|p| matches!(p, std::path::Component::ParentDir))
    {
        return Err(conflict());
    }
    let name = path.file_name().ok_or_else(conflict)?;
    let parent_path = path
        .parent()
        .ok_or_else(conflict)?
        .canonicalize()
        .map_err(io)?;
    let name = CString::new(name.as_encoded_bytes()).map_err(|_| conflict())?;
    let parent = Directory::open(&parent_path)?;
    // An unrelated account must not be able to race receipt finalization by
    // replacing entries in its parent. This parent is private coordinator storage.
    if parent.0.metadata().map_err(io)?.mode() & 0o077 != 0 {
        return Err(conflict());
    }
    private_acl::check(&parent.0)?;
    Ok((parent, name, parent_path.join(path.file_name().unwrap())))
}
fn staged(index: usize) -> CString {
    CString::new(format!("staged-{index}")).unwrap()
}
fn conflict() -> Failure {
    Failure::new(
        "provisioning_ownership_conflict",
        "A provisioning object cannot be safely changed or restored.",
        "Keep the private receipt; inspect its pending entry and resolve the conflicting object before retrying cleanup.",
    )
}
fn io(error: std::io::Error) -> Failure {
    Failure::new(
        "provisioning_io_failed",
        "A provisioning filesystem operation did not complete.",
        "Keep the private receipt, check storage availability and permissions, then retry cleanup.",
    )
    .with_details(serde_json::json!({"errno": error.raw_os_error()}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_refuse_traversal_and_ambiguous_components() {
        for path in ["", "/file", "a//b", "a/../b", ".", "a/./b", "a/", "a\0b"] {
            assert!(RelativePath::try_from(path.to_owned()).is_err());
        }
        assert!(
            RelativePath::try_from("Library/LaunchAgents/dev.elsecall.agent.plist".to_owned())
                .is_ok()
        );
    }
}
