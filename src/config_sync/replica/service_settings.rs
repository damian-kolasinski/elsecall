//! Service settings as a typed attachment to the ordinary mirrored generation.
use super::*;
use crate::{
    configuration::ServiceConfiguration,
    service_configuration::{self, Configuration, Summary},
};

impl Replica {
    fn service_source(&self) -> PathBuf {
        self.state_file
            .parent()
            .expect("sync directory")
            .parent()
            .expect("state directory")
            .join(service_configuration::SOURCE)
    }

    /// Restore this small typed source synchronously with the recorded role
    /// change, before its command returns. Later asynchronous E1 upkeep must not
    /// overwrite edits made after that point.
    pub fn service_role_changed(&self, master: bool) -> Result<()> {
        let mut state = self.lock();
        if master {
            if !state.service_source_master
                && let Some(manifest) = state.applying.as_ref().or(state.manifest.as_ref())
                && let Some(node) = manifest.service
            {
                let bytes = self.store.read(&manifest.key, &node)?;
                Configuration::parse(&bytes)?;
                self.restore_service_source(&bytes, &manifest.key)?;
                state.service_source_master = true;
            }
        } else {
            state.service_source_master = false;
        }
        state.save(&self.state_file)
    }

    fn service_backup_directory(&self) -> PathBuf {
        self.service_source()
            .with_file_name("service-config-backups")
    }

    pub fn service_member(&self) -> Option<MemberName> {
        self.lock().service_member.clone()
    }

    pub fn configure_service(&self, member: MemberName) {
        *self.service_identity.lock().expect("service identity lock") = Some(member);
    }

    pub fn service_settings(&self) -> Result<Option<ServiceConfiguration>> {
        let Some(manifest) = self.manifest() else {
            return Ok(None);
        };
        let Some(node) = manifest.service else {
            return Ok(None);
        };
        let bytes = self.store.read(&manifest.key, &node)?;
        let config = Configuration::parse(&bytes)?;
        let name = self.service_identity.lock().expect("service identity lock");
        let name = name.as_ref().expect("configured service member");
        let settings = config.resolve(name)?;
        *self.service_active.lock().expect("active service lock") = Some((
            manifest
                .key
                .digest_labelled("effective-service", format!("{settings:?}").as_bytes()),
            manifest.generation,
        ));
        Ok(Some(settings))
    }

    /// Validated before *any* path in a generation changes.
    pub(super) fn validate_service(
        &self,
        manifest: &Manifest,
    ) -> Result<Option<Zeroizing<Vec<u8>>>> {
        let Some(node) = manifest.service else {
            return Ok(None);
        };
        let result = (|| {
            let bytes = self.store.read(&manifest.key, &node)?;
            let config = Configuration::parse(&bytes)?;
            if let Some(name) = self
                .service_identity
                .lock()
                .expect("service identity lock")
                .as_ref()
            {
                let settings = config.resolve(name)?;
                let root = settings
                    .workspace_root
                    .unwrap_or_else(|| self.home.home.join(".elsecall/workspaces"));
                let real_root = disk::resolved_location(&root).map_err(|_| {
                    service_configuration::invalid(
                        "The proposed workspace root cannot be resolved safely on this member.",
                    )
                })?;
                if !manifest.paths.is_empty()
                    && (self.home.home.starts_with(&root)
                        || self.home.real_home.starts_with(&real_root))
                {
                    return Err(service_configuration::invalid(
                        "The proposed workspace root contains the member's mirrored paths.",
                    ));
                }
                let home = Home::new(&self.home.home, &[root]);
                for path in manifest.paths.keys() {
                    home.locate(path, false).map_err(|_| service_configuration::invalid("A synced path overlaps the proposed workspace root or resolves to an unsafe location on this member."))?;
                }
            }
            Ok(bytes)
        })();
        // apply_locked already holds state; its caller records rejections.
        result.map(Some)
    }

    pub fn clear_service_rejection(&self) -> Result<()> {
        let mut state = self.lock();
        if state.service_rejection.take().is_some() {
            state.save(&self.state_file)?;
        }
        Ok(())
    }

    pub fn service_rejected(&self, error: Failure) {
        let mut state = self.lock();
        state.service_rejection = Some(error);
        let _ = state.save(&self.state_file);
    }

    pub(super) fn restore_service_source(&self, bytes: &[u8], key: &SyncKey) -> Result<()> {
        let configuration = Configuration::parse(bytes)?;
        let baseline;
        let bytes = if configuration.temporary().is_some() {
            baseline = configuration.baseline_bytes();
            baseline.as_slice()
        } else {
            bytes
        };
        let source = self.service_source();
        let before = match std::fs::read(&source) {
            Ok(before) if before != bytes => Some(before),
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(Failure::io("Read previous fleet service configuration", e)),
        };
        if let Some(before) = before {
            let directory = self.service_backup_directory();
            private_files::directory(&directory)?;
            let backup =
                directory.join(format!("service-{}.toml", key.fingerprint(&before).short()));
            private_files::replace(&backup, &before)
                .map_err(|e| Failure::io("Back up fleet service configuration", e))?;
            self.trim_service_backups()?;
        }
        private_files::replace(&source, bytes)
            .map_err(|e| Failure::io("Restore fleet service configuration replica", e))
    }

    pub(super) fn trim_service_backups(&self) -> Result<()> {
        let mut backups = Vec::new();
        let directory = self.service_backup_directory();
        match std::fs::symlink_metadata(&directory) {
            Ok(_) => private_files::check_directory(&directory)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(Failure::io("Inspect service backup directory", e)),
        }
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(Failure::io("List service configuration backups", e)),
        };
        for entry in entries {
            let entry =
                entry.map_err(|e| Failure::io("Inspect service configuration backup", e))?;
            let name = entry.file_name();
            let ours = name
                .to_str()
                .and_then(|n| n.strip_prefix("service-"))
                .and_then(|n| n.strip_suffix(".toml"))
                .is_some_and(|n| n.len() == 16 && n.bytes().all(|b| b.is_ascii_hexdigit()));
            if !ours {
                continue;
            }
            let metadata = std::fs::symlink_metadata(entry.path())
                .map_err(|e| Failure::io("Inspect service configuration backup", e))?;
            if metadata.is_file() {
                backups.push((
                    metadata
                        .modified()
                        .map_err(|e| Failure::io("Inspect service backup time", e))?,
                    entry.path(),
                ));
            }
        }
        backups.sort();
        let excess = backups.len().saturating_sub(20);
        for (_, path) in backups.into_iter().take(excess) {
            private_files::remove_file(&path)?;
        }
        Ok(())
    }

    pub fn service_pending(&self) -> bool {
        let desired = (|| -> Result<_> {
            let Some(manifest) = self.manifest() else {
                return Ok(None);
            };
            let Some(node) = manifest.service else {
                return Ok(None);
            };
            let bytes = self.store.read(&manifest.key, &node)?;
            let config = Configuration::parse(&bytes)?;
            let Some(name) = self
                .service_identity
                .lock()
                .expect("service identity")
                .clone()
            else {
                return Ok(None);
            };
            let settings = config.resolve(&name)?;
            Ok(Some(manifest.key.digest_labelled(
                "effective-service",
                format!("{settings:?}").as_bytes(),
            )))
        })();
        match desired {
            Ok(Some(desired)) => {
                self.service_active
                    .lock()
                    .expect("active service lock")
                    .map(|a| a.0)
                    != Some(desired)
            }
            Err(_) => true,
            _ => false,
        }
    }

    pub(super) fn service_summary(&self, manifest: Option<&Manifest>) -> Summary {
        let active = *self.service_active.lock().expect("active service lock");
        let node = manifest.and_then(|m| m.service);
        Summary {
            current: None,
            generation: manifest.filter(|_| node.is_some()).map(|m| m.generation),
            active_generation: active.map(|a| a.1),
            fingerprint: node.map(|n| n.fingerprint.short()),
            effective_fingerprint: None,
            pending: self.service_pending(),
            rejection: self.lock().service_rejection.clone(),
        }
    }
}

impl Replica {
    pub fn plan_service(&self, offered: &Manifest) -> Result<service_configuration::MemberPlan> {
        let bytes = self.validate_service(offered)?.ok_or_else(|| {
            service_configuration::invalid("The service plan has no configuration.")
        })?;
        let config = Configuration::parse(&bytes)?;
        let name = self
            .service_identity
            .lock()
            .expect("service identity lock")
            .clone()
            .expect("service member configured");
        let before = if let Some(manifest) = self.manifest().filter(|m| m.service.is_some()) {
            let bytes = self
                .store
                .read(&manifest.key, &manifest.service.expect("filtered"))?;
            Configuration::parse(&bytes)?.resolve(&name)?
        } else {
            crate::configuration::load(&self.service_source().with_file_name("config.toml"))?
        };
        Ok(service_configuration::compare(
            &before,
            &config.resolve(&name)?,
            &offered.key,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_recovery_has_a_separate_twenty_version_budget_on_write_and_open() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let data = home.join("state");
        private_files::directory(&data).unwrap();
        let layout = || Layout::for_home(&home, &data);
        let store = Arc::new(
            crate::config_sync::store::FileStore::open(&data.join("config-sync/objects")).unwrap(),
        );
        let replica = Replica::open_with_backup_limit(layout(), store.clone(), 1).unwrap();
        let key = replica.key().unwrap();
        for n in 0..23 {
            std::fs::write(replica.service_source(), format!("# local version {n}")).unwrap();
            replica.restore_service_source(b"# accepted", &key).unwrap();
        }
        let backups = replica.service_backup_directory();
        assert_eq!(std::fs::read_dir(&backups).unwrap().count(), 20);
        assert!(!data.join("config-sync/backups").exists());
        // Startup also repairs excess versions, independently of E1's limit.
        for n in 0..3 {
            private_files::replace(
                &backups.join(format!("service-{n:016x}.toml")),
                b"# recovery",
            )
            .unwrap();
        }
        drop(replica);
        let replica = Replica::open_with_backup_limit(layout(), store.clone(), 1).unwrap();
        assert_eq!(std::fs::read_dir(&backups).unwrap().count(), 20);
        drop(replica);
        std::fs::remove_dir_all(&backups).unwrap();
        let outside = home.join("outside");
        private_files::directory(&outside).unwrap();
        let kept = outside.join("service-0000000000000000.toml");
        std::fs::write(&kept, "untouched").unwrap();
        std::os::unix::fs::symlink(&outside, &backups).unwrap();
        assert_eq!(
            Replica::open_with_backup_limit(layout(), store, 1)
                .err()
                .unwrap()
                .code,
            "unsafe_directory"
        );
        assert_eq!(std::fs::read_to_string(kept).unwrap(), "untouched");
    }
}
