//! Explicit, atomic import of legacy repository policy into personal settings.
use crate::{
    workload_configuration::{self, Configuration, FILE_NAME},
    workload_settings::WorkloadSettings,
};
use elsecall_contract::failure::{Failure, Result};
use elsecall_host::{paths::Paths, private_files};
use serde::Serialize;
use std::{
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, Serialize)]
pub struct Migrated {
    pub changed: bool,
    pub configuration: PathBuf,
    pub backup: Option<PathBuf>,
    pub imported: usize,
    pub sources_preserved: bool,
}

pub fn migrate(paths: &Paths, sources: &[PathBuf]) -> Result<Migrated> {
    private_files::owned_tree(&paths.home, &paths.data)?;
    private_files::directory(&paths.data)?;
    let _lock = private_files::try_lock(&paths.data.join("workloads.lock"))?.ok_or_else(|| {
        Failure::new(
            "operation_in_progress",
            "Another workload migration is running.",
            "Wait for it to finish, then retry.",
        )
    })?;
    let target = paths.data.join(FILE_NAME);
    if std::fs::symlink_metadata(&target).is_ok_and(|m| !m.is_file()) {
        return Err(Failure::new(
            "unsafe_configuration",
            "Workload configuration must be a regular file.",
            "Inspect workloads.toml before retrying.",
        ));
    }
    let original = workload_configuration::read(&target)?;
    let original_text = original.as_deref().unwrap_or_default();
    Configuration::parse(original_text)?;
    let mut document: toml_edit::DocumentMut = original_text
        .parse()
        .map_err(|_| workload_configuration::invalid())?;
    let mut imported = 0;
    for source in sources {
        let source = if source.is_dir() {
            source.join(".elsecall.toml")
        } else {
            source.clone()
        };
        let directory = source
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .canonicalize()
            .map_err(|e| Failure::io("Resolve legacy configuration directory", e))?;
        let text = workload_configuration::read(&source)?.ok_or_else(|| {
            Failure::new(
                "invalid_request",
                "A legacy workload configuration does not exist.",
                "Pass each repository directory or its .elsecall.toml file to config migrate.",
            )
        })?;
        let settings =
            WorkloadSettings::parse(&text).map_err(|_| workload_configuration::invalid())?;
        let key = directory
            .to_str()
            .ok_or_else(workload_configuration::invalid)?;
        if document
            .get("directories")
            .and_then(|d| d.get(key))
            .is_some()
        {
            let current = Configuration::parse(&document.to_string())?;
            let existing = current.for_directory(&directory);
            if &settings != existing {
                return Err(Failure::new(
                    "configuration_conflict",
                    "A directory already has different global workload settings; nothing was changed.",
                    "Review the existing workloads.toml entry and legacy file, reconcile them explicitly, then retry.",
                ));
            }
            continue;
        }
        let legacy: toml_edit::DocumentMut = text
            .parse()
            .map_err(|_| workload_configuration::invalid())?;
        if document.get("directories").is_none() {
            document["directories"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        document["directories"][key] = toml_edit::Item::Table(legacy.as_table().clone());
        imported += 1;
    }
    let updated = document.to_string();
    Configuration::parse(&updated)?;
    let changed = imported > 0;
    let backup = if changed {
        let backup = if let Some(original) = &original {
            let mut backup = tempfile::Builder::new()
                .prefix("workloads-before-migration-")
                .suffix(".toml")
                .tempfile_in(&paths.data)
                .map_err(|e| Failure::io("Create workload backup", e))?;
            backup
                .write_all(original.as_bytes())
                .and_then(|_| backup.as_file().sync_all())
                .map_err(|e| Failure::io("Write workload backup", e))?;
            Some(
                backup
                    .into_temp_path()
                    .keep()
                    .map_err(|e| Failure::io("Keep workload backup", e.error))?,
            )
        } else {
            None
        };
        private_files::write(&target, updated.as_bytes(), 0o600)?;
        backup
    } else {
        None
    };
    Ok(Migrated {
        changed,
        configuration: target,
        backup,
        imported,
        sources_preserved: true,
    })
}
