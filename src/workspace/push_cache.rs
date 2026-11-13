//! What the caller last pushed to one member's copy of a workspace, so the next sync
//! sends only the difference when the copy still matches it.
use crate::workspace::{
    digest::Digest, git::ObjectFormat, index::IndexListing, manifest::Manifest,
};
use std::path::{Path, PathBuf};

/// Where a caller keeps its caches for one workspace.
#[derive(Clone, Debug)]
pub struct CacheFiles {
    directory: PathBuf,
    root: String,
    workspace: String,
}
impl CacheFiles {
    /// Caches for the workspace at `root` under `directory`, which is private.
    pub fn new(directory: &Path, root: &str) -> Self {
        Self {
            directory: directory.to_owned(),
            root: root.to_owned(),
            workspace: Digest::of(root.as_bytes()).hex()[..32].to_owned(),
        }
    }
    /// The name the workspace's files share.
    pub fn id(&self) -> &str {
        &self.workspace
    }
    /// Records which path the workspace's files stand for, so they can be
    /// cleaned up once the workspace is deleted.
    pub fn registry(&self) -> PathBuf {
        self.directory.join(format!("{}.workspace", self.workspace))
    }
    pub fn register(&self) {
        if !self.registry().exists() {
            let _ = crate::private_files::replace(&self.registry(), self.root.as_bytes());
        }
    }
    pub fn stat(&self) -> PathBuf {
        self.directory.join(format!("{}.stat", self.workspace))
    }
    pub fn pushed(&self, member: &str) -> PathBuf {
        self.directory
            .join(format!("{}-{member}.pushed", self.workspace))
    }
}

/// One of a caller's state files, as `list` finds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateFile {
    /// A workspace's stat cache.
    Stat { id: String },
    /// What a workspace last pushed to a member.
    Pushed { id: String, member: String },
    /// The path a workspace's files stand for.
    Registry { id: String },
}

/// The caller state files in `directory`, with their paths; other files are left
/// out.
pub fn list(directory: &Path) -> Vec<(StateFile, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let is_id = |id: &str| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit());
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let file = if let Some(id) = name.strip_suffix(".stat") {
                StateFile::Stat { id: id.into() }
            } else if let Some(id) = name.strip_suffix(".workspace") {
                StateFile::Registry { id: id.into() }
            } else {
                let (id, member) = name.strip_suffix(".pushed")?.split_once('-')?;
                StateFile::Pushed {
                    id: id.into(),
                    member: member.into(),
                }
            };
            let id = match &file {
                StateFile::Stat { id } | StateFile::Registry { id } => id,
                StateFile::Pushed { id, .. } => id,
            };
            is_id(id).then(|| (file, entry.path()))
        })
        .collect()
}

/// The workspace path the registry of `id` records, if it is the path `id`
/// stands for.
pub fn registered_root(directory: &Path, id: &str) -> Option<String> {
    let root = std::fs::read_to_string(directory.join(format!("{id}.workspace"))).ok()?;
    (CacheFiles::new(directory, &root).id() == id).then_some(root)
}

pub struct Pushed {
    pub manifest: Manifest,
    pub index: IndexListing,
}

impl Pushed {
    pub fn load(path: &Path, format: ObjectFormat) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        let (length, rest) = bytes.split_at_checked(8)?;
        let length = u64::from_be_bytes(length.try_into().ok()?) as usize;
        let (manifest, index) = rest.split_at_checked(length)?;
        Some(Self {
            manifest: Manifest::decode(manifest)?,
            index: IndexListing::decode(index, format)?,
        })
    }

    pub fn save(path: &Path, manifest: &Manifest, index: &IndexListing) {
        let manifest = manifest.encode();
        let mut bytes = (manifest.len() as u64).to_be_bytes().to_vec();
        bytes.extend_from_slice(&manifest);
        bytes.extend_from_slice(&index.encode());
        let _ = crate::private_files::replace(path, &bytes);
    }
}
