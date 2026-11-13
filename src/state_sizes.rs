//! Read-only disk diagnosis. Measure metadata only, without following links or
//! opening state contents (some files intentionally contain secrets).
use crate::{paths::Paths, service_log::Settings};
use serde::Serialize;
use std::{fs, io, os::unix::fs::MetadataExt, path::Path};

const MIB: u64 = 1024 * 1024;
#[derive(Debug, Serialize)]
pub struct Entry {
    pub name: String,
    pub bytes: u64,
    pub allocated_bytes: u64,
    pub warning_above_bytes: u64,
    pub unusually_large: bool,
}
#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub bytes: u64,
    pub allocated_bytes: u64,
    pub entries: Vec<Entry>,
    pub warnings: Vec<String>,
    pub incomplete: bool,
}

fn measure(path: &Path) -> io::Result<(u64, u64)> {
    let meta = fs::symlink_metadata(path)?;
    let mut bytes = if meta.is_dir() { 0 } else { meta.len() };
    let mut allocated = meta.blocks().saturating_mul(512);
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            let (b, a) = measure(&entry?.path())?;
            bytes = bytes.saturating_add(b);
            allocated = allocated.saturating_add(a);
        }
    }
    Ok((bytes, allocated))
}

pub fn inspect(paths: &Paths, logs: &Settings) -> Report {
    let mut report = Report::default();
    match fs::symlink_metadata(&paths.data) {
        Ok(meta) if meta.is_dir() => (),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return report,
        _ => {
            report.incomplete = true;
            report
                .warnings
                .push("State directory could not be measured safely.".into());
            return report;
        }
    }
    let entries = match fs::read_dir(&paths.data) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return report,
        Err(_) => {
            report.incomplete = true;
            report
                .warnings
                .push("State sizes could not be inspected.".into());
            return report;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            report.incomplete = true;
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let (bytes, allocated_bytes) = match measure(&entry.path()) {
            Ok(size) => size,
            Err(_) => {
                report.incomplete = true;
                continue;
            }
        };
        let warning_above_bytes = if name == "service.log"
            || name
                .strip_prefix("service.log.")
                .is_some_and(|n| n.parse::<usize>().is_ok())
        {
            logs.max_bytes
        } else if name.starts_with("records.sqlite") {
            512 * MIB
        } else if name.starts_with("load.sqlite") {
            128 * MIB
        } else if name == "config-sync" {
            1024 * MIB
        } else {
            256 * MIB
        };
        let unusually_large = bytes > warning_above_bytes;
        if unusually_large {
            report.warnings.push(format!("{name} exceeds its size warning threshold; inspect its retention (see docs/state-retention.md)."));
        }
        report.bytes = report.bytes.saturating_add(bytes);
        report.allocated_bytes = report.allocated_bytes.saturating_add(allocated_bytes);
        report.entries.push(Entry {
            name,
            bytes,
            allocated_bytes,
            warning_above_bytes,
            unusually_large,
        });
    }
    report.entries.sort_by(|a, b| a.name.cmp(&b.name));
    if report.bytes > 2 * 1024 * MIB {
        report
            .warnings
            .push("Total state exceeds 2 GiB; inspect the largest entries.".into());
    }
    if report.incomplete {
        report.warnings.push("Some state could not be measured; sizes are incomplete. Retry when files are stable and check directory access.".into());
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn measures_files_and_nested_state_flags_large_files_and_never_follows_links() {
        let home = tempfile::tempdir_in("/tmp").unwrap();
        let paths = Paths::for_home(home.path()).unwrap();
        fs::create_dir_all(paths.data.join("config-sync/backups")).unwrap();
        fs::write(
            paths.data.join("config-sync/backups/secret"),
            b"secret-value",
        )
        .unwrap();
        fs::File::create(paths.records())
            .unwrap()
            .set_len(513 * MIB)
            .unwrap();
        fs::write(paths.log(), vec![b'x'; 2048]).unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::File::create(outside.path().join("large"))
            .unwrap()
            .set_len(4096 * MIB)
            .unwrap();
        std::os::unix::fs::symlink(outside.path(), paths.data.join("link")).unwrap();
        let report = inspect(
            &paths,
            &Settings {
                max_bytes: 1024,
                keep: 2,
            },
        );
        assert!(!report.incomplete);
        assert!(report.bytes < 514 * MIB);
        assert!(
            report
                .entries
                .iter()
                .find(|e| e.name == "records.sqlite")
                .unwrap()
                .unusually_large
        );
        assert!(
            report
                .entries
                .iter()
                .find(|e| e.name == "service.log")
                .unwrap()
                .unusually_large
        );
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("secret-value")
        );
    }
}
