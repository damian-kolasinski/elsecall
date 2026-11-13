//! The shared object stores under the workspace root: one per caller repository,
//! at `<root>/.stores/<calling member>/<id>.git`.
use crate::{failure::Result, workspace::git::Git};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Store {
    pub calling_member: String,
    pub path: PathBuf,
}

/// Every store under `root`, never through a link.
pub fn list(root: &Path) -> Vec<Store> {
    let mut stores = Vec::new();
    let Ok(members) = std::fs::read_dir(root.join(".stores")) else {
        return stores;
    };
    for member in members.flatten() {
        if !member.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Some(calling_member) = member.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(member.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir())
                && entry.file_name().to_string_lossy().ends_with(".git")
            {
                stores.push(Store {
                    calling_member: calling_member.clone(),
                    path: entry.path(),
                });
            }
        }
    }
    stores.sort_by(|a, b| a.path.cmp(&b.path));
    stores
}

/// Whether `path` is a store directory under `root`, as a copy's alternates must
/// name.
pub fn is_store_of(root: &Path, calling_member: &str, path: &Path) -> bool {
    path.parent() == Some(&root.join(".stores").join(calling_member))
        && path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".git"))
}

/// Whether the directory at `path` is a bare repository, as a store is.
pub fn is_repository(path: &Path) -> bool {
    crate::workspace::confined::Confined::open(path)
        .ok()
        .and_then(|store| store.child("objects").ok().flatten())
        .is_some()
        && path.join("HEAD").is_file()
}

/// How many packs the store holds.
pub fn packs(store: &Path) -> usize {
    std::fs::read_dir(store.join("objects/pack"))
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".pack"))
                .count()
        })
        .unwrap_or(0)
}

/// Rolls the store's small packs together, leaving large ones as they are, on one
/// thread, as housekeeping holds one core. Nothing is dropped: packs are combined
/// whole, so objects no reference reaches stay, since copies' indexes and their
/// own repositories may name them.
pub fn compact(store: &Path) -> Result<()> {
    Git::member(store).output(&[
        "-c",
        "pack.threads=1",
        "repack",
        "-d",
        "--geometric=2",
        "-q",
        "--no-write-bitmap-index",
    ])?;
    Ok(())
}
