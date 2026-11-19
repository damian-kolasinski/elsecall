//! Subprocess environments belong to each test, even inside an admitted check.
#![allow(dead_code)]
use std::{ffi::OsStr, process::Command};

/// Preserve the toolchain and OS environment, but remove every inherited Elsecall
/// control before a fixture adds its own. Unknown future controls are covered too.
pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.env("TOKIO_WORKER_THREADS", "2");
    for (name, _) in std::env::vars_os() {
        if name == "ELSECALL" || name.as_encoded_bytes().starts_with(b"ELSECALL_") {
            command.env_remove(name);
        }
    }
    command
}

pub(crate) fn async_command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    command(program).into()
}

/// A scheduling allowance, not an expectation about how long an operation takes.
/// Poll the observable condition and fail with diagnostics when it never happens.
pub(crate) const WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Install fixture policy at the same personal boundary real CLI calls read.
/// `directory` scopes a complete policy; `None` supplies global defaults.
pub(crate) fn workload_policy(
    home: &std::path::Path,
    directory: Option<&std::path::Path>,
    settings: &str,
) {
    let paths = elsecall_host::paths::Paths::for_home(home).unwrap();
    elsecall_host::private_files::directory(&paths.data).unwrap();
    let target = paths
        .data
        .join(elsecall_runs::workload_configuration::FILE_NAME);
    let mut table: toml::Table = std::fs::read_to_string(&target)
        .ok()
        .map(|s| toml::from_str(&s).unwrap())
        .unwrap_or_default();
    let settings: toml::Table = toml::from_str(settings).unwrap();
    if let Some(directory) = directory {
        let directory = directory.canonicalize().unwrap();
        table
            .entry("directories")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .unwrap()
            .insert(
                directory.to_str().unwrap().to_owned(),
                toml::Value::Table(settings),
            );
    } else {
        let directories = table.remove("directories");
        table = settings;
        if let Some(directories) = directories {
            table.insert("directories".into(), directories);
        }
    }
    std::fs::write(target, toml::to_string(&table).unwrap()).unwrap();
}
