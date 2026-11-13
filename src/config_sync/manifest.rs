//! What the fleet mirrors: for each synced path, its tree of directories and files,
//! each file by mode, size and keyed fingerprint. A manifest never holds content;
//! content lives in the replica store, addressed by fingerprint.
//!
//! A manifest arriving from a member is untrusted: deserializing validates it, so a
//! `Manifest` value always respects the limits and shapes below.
use crate::{
    config_sync::{
        fingerprint::{Fingerprint, SyncKey},
        synced_path::{self, SyncedPath},
    },
    roster::MemberName,
    workspace::path::WorkspacePath,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The most paths one fleet syncs.
pub const MAXIMUM_PATHS: usize = 16;
/// The most entries (files and directories) across every synced path.
pub const MAXIMUM_ENTRIES: usize = 4000;
pub const MAXIMUM_FILE_BYTES: u64 = 16 << 20;
pub const MAXIMUM_TOTAL_BYTES: u64 = 64 << 20;

/// A regular file: owner permission bits only (members never grant group or
/// other access), size and keyed fingerprint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileNode {
    pub mode: u32,
    pub size: u64,
    pub fingerprint: Fingerprint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Node {
    Directory,
    File(FileNode),
}

/// One synced path: missing, a single file, or a directory of entries keyed by
/// their path inside it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", content = "content", rename_all = "snake_case")]
pub enum Tree {
    #[default]
    Absent,
    File(FileNode),
    Directory(BTreeMap<WorkspacePath, Node>),
}

/// The key that names a file of a tree in comparisons: `None` for a path that is
/// itself a file.
pub type FileKey = Option<WorkspacePath>;

impl Tree {
    /// Every regular file, by its key.
    pub fn files(&self) -> BTreeMap<FileKey, FileNode> {
        match self {
            Tree::Absent => BTreeMap::new(),
            Tree::File(file) => BTreeMap::from([(None, *file)]),
            Tree::Directory(entries) => entries
                .iter()
                .filter_map(|(path, node)| match node {
                    Node::File(file) => Some((Some(path.clone()), *file)),
                    Node::Directory => None,
                })
                .collect(),
        }
    }

    pub fn entry_count(&self) -> usize {
        match self {
            Tree::Absent => 0,
            Tree::File(_) => 1,
            Tree::Directory(entries) => entries.len() + 1,
        }
    }

    pub fn bytes(&self) -> u64 {
        self.files().values().map(|f| f.size).sum()
    }

    /// Checks shape and bounds: owner-only modes, sizes, and every entry's parent
    /// listed as a directory.
    pub fn validate(&self) -> Result<(), String> {
        let check = |file: &FileNode| {
            if file.mode & !0o700 != 0 {
                return Err("file modes carry owner bits only".to_owned());
            }
            if file.size > MAXIMUM_FILE_BYTES {
                return Err(format!(
                    "files are at most {} MiB",
                    MAXIMUM_FILE_BYTES >> 20
                ));
            }
            Ok(())
        };
        match self {
            Tree::Absent => Ok(()),
            Tree::File(file) => check(file),
            Tree::Directory(entries) => {
                // Names a case-insensitive file system would merge cannot both be held.
                let mut folded = std::collections::HashSet::new();
                for path in entries.keys() {
                    if !folded.insert(path.as_str().to_lowercase()) {
                        return Err(format!(
                            "{} collides with another entry when case is ignored",
                            path.as_str()
                        ));
                    }
                }
                for (path, node) in entries {
                    if let Some(parent) = path.parent()
                        && entries.get(&parent) != Some(&Node::Directory)
                    {
                        return Err(format!("{} has no parent directory entry", path.as_str()));
                    }
                    if let Node::File(file) = node {
                        check(file)?;
                    }
                }
                Ok(())
            }
        }
    }
}

/// The synced set as one master issued it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawManifest")]
pub struct Manifest {
    /// Increases with every change the master issues.
    pub generation: u64,
    /// The member that issued this generation.
    pub issued_by: MemberName,
    pub key: SyncKey,
    pub paths: BTreeMap<SyncedPath, Tree>,
    /// Opaque service settings, carried by the same replica store and channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<FileNode>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    generation: u64,
    issued_by: MemberName,
    key: SyncKey,
    paths: BTreeMap<SyncedPath, Tree>,
    #[serde(default)]
    service: Option<FileNode>,
}

impl TryFrom<RawManifest> for Manifest {
    type Error = String;
    fn try_from(raw: RawManifest) -> Result<Self, String> {
        let manifest = Manifest {
            generation: raw.generation,
            issued_by: raw.issued_by,
            key: raw.key,
            paths: raw.paths,
            service: raw.service,
        };
        manifest.validate()?;
        Ok(manifest)
    }
}

impl Manifest {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(service) = self.service {
            Tree::File(service).validate()?;
            if service.size > crate::service_configuration::LIMIT as u64 {
                return Err("service configuration exceeds 64 KiB".into());
            }
        }
        let paths: Vec<SyncedPath> = self.paths.keys().cloned().collect();
        synced_path::check_set(&paths, MAXIMUM_PATHS)?;
        let mut entries = 0;
        let mut bytes = 0;
        for tree in self.paths.values() {
            tree.validate()?;
            entries += tree.entry_count();
            bytes += tree.bytes();
        }
        if entries > MAXIMUM_ENTRIES {
            return Err(format!(
                "the synced set has more than {MAXIMUM_ENTRIES} files and directories"
            ));
        }
        if bytes > MAXIMUM_TOTAL_BYTES {
            return Err(format!(
                "the synced set is larger than {} MiB",
                MAXIMUM_TOTAL_BYTES >> 20
            ));
        }
        Ok(())
    }

    /// Identifies the synced content: equal for equal paths and trees whoever
    /// issued them, keyed so it reveals nothing.
    pub fn id(&self) -> Fingerprint {
        match self.service {
            None => content_id(&self.key, &self.paths),
            Some(node) => self.key.digest_labelled(
                "service-manifest",
                &serde_json::to_vec(&(&self.paths, node)).expect("manifest serializes"),
            ),
        }
    }

    /// Every blob the replica store must hold, including service settings.
    pub fn files(&self) -> impl Iterator<Item = FileNode> + '_ {
        self.paths
            .values()
            .flat_map(|tree| tree.files().into_values())
            .chain(self.service)
    }

    /// Every fingerprint the replica store must hold.
    pub fn fingerprints(&self) -> BTreeSet<Fingerprint> {
        self.files().map(|f| f.fingerprint).collect()
    }
}

/// The id of a set of trees under `key`.
pub fn content_id(key: &SyncKey, paths: &BTreeMap<SyncedPath, Tree>) -> Fingerprint {
    key.digest_labelled(
        "manifest",
        &serde_json::to_vec(paths).expect("trees serialize"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(key: &SyncKey, content: &[u8]) -> FileNode {
        FileNode {
            mode: 0o600,
            size: content.len() as u64,
            fingerprint: key.fingerprint(content),
        }
    }

    #[test]
    fn manifests_validate_on_arrival_and_identify_content() {
        let key = SyncKey::generate().unwrap();
        let path = |p: &str| WorkspacePath::parse(p).unwrap();
        let tree = Tree::Directory(BTreeMap::from([
            (path("a"), Node::Directory),
            (path("a/b"), Node::File(file(&key, b"x"))),
            (path("c"), Node::File(file(&key, b"yy"))),
        ]));
        assert_eq!(tree.files().len(), 2);
        assert_eq!(tree.bytes(), 3);
        let manifest = Manifest {
            service: None,
            generation: 1,
            issued_by: "alpha".parse().unwrap(),
            key: key.clone(),
            paths: BTreeMap::from([(SyncedPath::parse("~/.config/demo").unwrap(), tree)]),
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, manifest);
        assert_eq!(back.id(), manifest.id());
        let mut other = manifest.clone();
        other.generation = 2;
        other.issued_by = "beta".parse().unwrap();
        assert_eq!(other.id(), manifest.id(), "same content, same id");
        assert_eq!(manifest.fingerprints().len(), 2);

        // Missing parents, group bits, forbidden paths and oversize files are refused.
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let entries = &mut value["paths"]["~/.config/demo"]["content"];
        entries.as_object_mut().unwrap().remove("a");
        assert!(serde_json::from_value::<Manifest>(value).is_err());
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["paths"]["~/.config/demo"]["content"]["c"]["mode"] = 0o644.into();
        assert!(serde_json::from_value::<Manifest>(value).is_err());
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let tree = value["paths"]
            .as_object_mut()
            .unwrap()
            .remove("~/.config/demo")
            .unwrap();
        value["paths"]["~/.ssh"] = tree;
        assert!(serde_json::from_value::<Manifest>(value).is_err());
        let big = Tree::File(FileNode {
            size: MAXIMUM_FILE_BYTES + 1,
            ..file(&key, b"")
        });
        assert!(big.validate().is_err());
    }
}
