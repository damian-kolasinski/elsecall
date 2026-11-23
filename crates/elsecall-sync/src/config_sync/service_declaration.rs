//! Declaring a path edits the master's source policy. Delivery remains an
//! independent operation that requires confirming a reviewed sync plan.

use super::{ConfigSync, blocking, configuration};
use crate::service_configuration::{self, Configuration};
use elsecall_contract::failure::{Failure, Result};
use elsecall_fleet::config_sync_model::synced_path::{MAXIMUM_PATHS, SyncedPath, check_set};
use elsecall_peers::roster::MemberName;
use std::{fs, io::Read, path::Path};
use toml_edit::{Array, DocumentMut, value};

impl ConfigSync {
    pub(crate) async fn declare(&self, path: SyncedPath) -> Result<()> {
        let _activity = self.activity.enter()?;
        let _issuing = self.issuing.lock().await;
        let (_, master) = self.require_master()?;
        self.refuse_while_held()?;
        let local = self.configuration.clone();
        let managed = self
            .replica
            .manifest()
            .is_some_and(|manifest| manifest.service.is_some());
        blocking(move || declare(&local, managed.then_some(&master), path)).await
    }
}

fn source(path: &Path) -> Result<String> {
    let mut text = String::new();
    match fs::File::open(path) {
        Ok(file) => {
            file.take(64 * 1024 + 1)
                .read_to_string(&mut text)
                .map_err(|error| Failure::io("Read sync declaration", error))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Failure::io("Read sync declaration", error)),
    }
    Ok(text)
}

fn read_source(path: &Path, managed: bool) -> Result<String> {
    if managed {
        return String::from_utf8(service_configuration::read(path)?).map_err(|error| {
            Failure::io(
                "Read fleet service configuration",
                std::io::Error::new(std::io::ErrorKind::InvalidData, error),
            )
        });
    }
    source(path)
}

fn declare(local: &Path, master: Option<&MemberName>, path: SyncedPath) -> Result<()> {
    let managed = local.with_file_name(service_configuration::SOURCE);
    let file = if master.is_some() {
        managed.as_path()
    } else {
        local
    };
    if fs::symlink_metadata(file).is_ok_and(|metadata| !metadata.is_file()) {
        return Err(Failure::new(
            "invalid_configuration",
            "The sync declaration's configuration is not a regular file.",
            "Edit the configuration through its existing source.",
        ));
    }
    let original = read_source(file, master.is_some())?;
    let mut paths = match master {
        Some(master) => {
            Configuration::parse(original.as_bytes())?
                .resolve(master)?
                .sync_paths
        }
        None => configuration::parse(&original, file)?.sync_paths,
    };
    if paths.contains(&path) {
        return Ok(());
    }
    paths.push(path.clone());
    check_set(&paths, MAXIMUM_PATHS).map_err(|why| {
        Failure::new(
            "invalid_configuration",
            why,
            "Choose a non-overlapping directory under your home.",
        )
    })?;
    let mut document = original.parse::<DocumentMut>().map_err(|_| {
        Failure::new(
            "invalid_configuration",
            "The service settings could not be edited as TOML.",
            "Correct the configuration file and try again.",
        )
    })?;
    let setting = match master {
        Some(master) => &mut document["members"][master.as_str()]["sync"]["paths"],
        None => &mut document["sync"]["paths"],
    };
    if let Some(array) = setting.as_array_mut() {
        array.push(path.to_string());
    } else {
        *setting = value(paths.iter().map(ToString::to_string).collect::<Array>());
    }
    let updated = document.to_string();
    if master.is_some() {
        Configuration::parse(updated.as_bytes())?;
    } else {
        configuration::parse(&updated, file)?;
    }
    if read_source(file, master.is_some())? != original {
        return Err(Failure::new(
            "configuration_changed",
            "The service configuration changed while adding the path.",
            "Review the current settings and try again.",
        ));
    }
    elsecall_host::private_files::replace(file, updated.as_bytes())
        .map_err(|error| Failure::io("Write sync declaration", error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_declaration_preserves_defaults_and_unrelated_comments() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("config.toml");
        let managed = local.with_file_name(service_configuration::SOURCE);
        fs::write(
            &managed,
            "# operator settings\n[defaults.sync]\npaths = [\"~/.config/other\"]\n",
        )
        .unwrap();
        let master = "mac-mini".parse().unwrap();
        let path = SyncedPath::parse("~/.config/acme-ios").unwrap();
        declare(&local, Some(&master), path.clone()).unwrap();
        declare(&local, Some(&master), path).unwrap();
        let bytes = fs::read(&managed).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(
            text.contains("# operator settings\n[defaults.sync]"),
            "{text}"
        );
        let settings = Configuration::parse(&bytes).unwrap();
        assert_eq!(settings.resolve(&master).unwrap().sync_paths.len(), 2);
        assert_eq!(
            settings
                .resolve(&"macbook-pro".parse().unwrap())
                .unwrap()
                .sync_paths
                .len(),
            1
        );
        assert!(!local.exists());
    }

    #[test]
    fn missing_managed_source_is_not_replaced_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("config.toml");
        let master = "mac-mini".parse().unwrap();
        let path = SyncedPath::parse("~/.config/acme-ios").unwrap();
        assert!(declare(&local, Some(&master), path).is_err());
        assert!(!local.with_file_name(service_configuration::SOURCE).exists());
    }
}
