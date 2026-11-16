//! Owner-only update records shared by the old service, detached helper and successor.
use super::{Discovery, Job, JobId, Phase, invalid, now};
use crate::{
    failure::{Failure, Result},
    paths::Paths,
    private_files,
};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct Store {
    directory: PathBuf,
    cache: PathBuf,
}
impl Store {
    pub fn new(paths: &Paths) -> Self {
        Self {
            directory: paths.data.join("release-update"),
            cache: paths.home.join(".local/share/elsecall/releases"),
        }
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn prepare(&self, paths: &Paths) -> Result<()> {
        private_files::owned_tree(&paths.home, &self.directory)?;
        private_files::directory(&self.directory)
    }
    pub fn stage(&self, paths: &Paths, id: &JobId) -> Result<PathBuf> {
        let directory = self.cache.join(id.as_str());
        private_files::owned_tree(&paths.home, &directory)?;
        private_files::directory(&directory)?;
        Ok(directory)
    }
    pub fn discard_stage(&self, paths: &Paths, id: &JobId) -> Result<()> {
        let directory = self.cache.join(id.as_str());
        if !directory.exists() {
            return Ok(());
        }
        private_files::owned_tree(&paths.home, &directory)?;
        private_files::check_directory(&directory)?;
        fs::remove_dir_all(directory).map_err(|e| Failure::io("Remove unused release cache", e))
    }
    pub fn worker(&self, id: &JobId) -> PathBuf {
        self.directory.join(format!("worker-{}", id.as_str()))
    }
    pub fn job(&self) -> Result<Option<Job>> {
        read(&self.directory.join("job.json"))
    }
    pub fn discovery(&self) -> Result<Discovery> {
        Ok(read(&self.directory.join("discovery.json"))?.unwrap_or_default())
    }
    pub fn save_discovery(&self, state: &Discovery) -> Result<()> {
        write(&self.directory.join("discovery.json"), state)
    }
    pub fn save_job(&self, job: &Job) -> Result<()> {
        write(&self.directory.join("job.json"), job)
    }
    pub fn advance(&self, job: &mut Job, phase: Phase) -> Result<()> {
        job.phase = phase;
        job.updated_at = now();
        self.save_job(job)
    }
    pub fn helper_lock(&self) -> Result<Option<File>> {
        private_files::try_lock(&self.directory.join("helper.lock"))
    }
    pub fn request_lock(&self) -> Result<Option<File>> {
        private_files::try_lock(&self.directory.join("request.lock"))
    }
    pub fn backup(&self) -> PathBuf {
        self.directory.join("previous")
    }
    pub fn selection(&self) -> PathBuf {
        self.directory.join("selection.json")
    }
    pub fn helper_alive(&self) -> Result<bool> {
        Ok(self.helper_lock()?.is_none())
    }
}
fn read<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match File::open(path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(65537)
                .read_to_end(&mut bytes)
                .map_err(|e| Failure::io("Read update state", e))?;
            if bytes.len() > 65536 {
                return Err(invalid("Update state exceeds its size limit."));
            }
            serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| invalid("The update state is unreadable."))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Failure::io("Read update state", error)),
    }
}
pub fn write<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().expect("record parent");
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| Failure::io("Stage update record", e))?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|e| Failure::io("Protect update record", e))?;
    temporary
        .write_all(&serde_json::to_vec(value).expect("update record serializes"))
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|e| Failure::io("Write update record", e))?;
    temporary
        .persist(path)
        .map_err(|e| Failure::io("Publish update record", e.error))?;
    File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|e| Failure::io("Sync update record", e))
}
pub fn read_selection<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    read(path)
}

/// Remove only recognized updater records; verified release caches remain available to mise.
pub fn remove(paths: &Paths) -> Result<()> {
    let store = Store::new(paths);
    if !store.directory.exists() {
        return Ok(());
    }
    private_files::check_directory(&store.directory)?;
    if store.helper_alive()? {
        return Err(invalid(
            "A release update is running; wait for it before uninstalling.",
        ));
    }
    if store.backup().exists() {
        private_files::check_directory(&store.backup())?;
        for name in ["elsecall", "elsecall-ui"] {
            private_files::remove_file(&store.backup().join(name))?;
        }
        fs::remove_dir(store.backup())
            .map_err(|e| Failure::io("Remove release backup directory", e))?;
    }
    for name in [
        "job.json",
        "discovery.json",
        "selection.json",
        "helper.lock",
        "request.lock",
    ] {
        private_files::remove_file(&store.directory.join(name))?;
    }
    for entry in
        fs::read_dir(&store.directory).map_err(|e| Failure::io("Read update directory", e))?
    {
        let entry = entry.map_err(|e| Failure::io("Read update entry", e))?;
        if entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix("worker-"))
            .is_some_and(|id| JobId::try_from(id.to_owned()).is_ok())
        {
            private_files::remove_file(&entry.path())?;
        }
    }
    fs::remove_dir(store.directory).map_err(|e| Failure::io("Remove update directory", e))
}
pub fn active_helper(paths: &Paths) -> Result<bool> {
    let store = Store::new(paths);
    if !store.directory.exists() {
        return Ok(false);
    }
    private_files::check_directory(&store.directory)?;
    store.helper_alive()
}
