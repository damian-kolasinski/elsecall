//! The service's local configuration, written by the operator.
use crate::{
    failure::{Failure, Result},
    housekeeping::settings::{self, Settings as Housekeeping},
    remote_run::leftovers::Cap,
    scheduling::{
        capacity::Cores,
        parameters::{
            AdmissionOverrides, EstimatesOverrides, Overrides, Parameters, PlacementOverrides,
        },
        request::AtDeadline,
    },
    workspace::location::Setting,
};
use serde::Deserialize;
use std::{
    io::Read,
    path::{Path, PathBuf},
};

/// Settings that belong to this member alone, independent of the fleet.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceConfiguration {
    /// Capacity kept free of Elsecall work so the machine stays responsive.
    pub reserved: Cores,
    pub logs: crate::service_log::Settings,
    pub sync_backups: usize,
    /// Where copies of other members' workspaces live here.
    pub location: Setting,
    /// The root of mapped copies and object stores; `None` for the default.
    pub workspace_root: Option<PathBuf>,
    /// PATH for work placed here; `None` keeps the service's own.
    pub placed_path: Option<String>,
    /// How long processes a placed command leaves running may run after it
    /// exits; zero stops them at once.
    pub leftovers_cap: Cap,
    /// Repositories here that may already hold callers' history.
    pub object_sources: Vec<PathBuf>,
    /// When copies of other members' workspaces are removed.
    pub housekeeping: Housekeeping,
    /// What placement, the load gate and learned estimates decide with.
    pub parameters: Parameters,
    /// Files and directories this member may mirror to the fleet while it is
    /// master, each enabled by the operator after a plan.
    pub sync_paths: Vec<crate::config_sync::synced_path::SyncedPath>,
}

impl Default for ServiceConfiguration {
    fn default() -> Self {
        Self {
            logs: Default::default(),
            sync_backups: crate::config_sync::state::BACKUPS_TOTAL,
            reserved: Default::default(),
            location: Default::default(),
            workspace_root: None,
            placed_path: None,
            leftovers_cap: Default::default(),
            object_sources: Vec::new(),
            housekeeping: Default::default(),
            parameters: Default::default(),
            sync_paths: Vec::new(),
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct LogsSection {
    max_size: Option<String>,
    keep: Option<usize>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RetentionSection {
    sync_backups: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    logs: LogsSection,
    #[serde(default)]
    retention: RetentionSection,
    #[serde(default)]
    capacity: CapacitySection,
    #[serde(default)]
    workspace: WorkspaceSection,
    #[serde(default)]
    placed_runs: PlacedRunsSection,
    #[serde(default)]
    housekeeping: HousekeepingSection,
    #[serde(default)]
    placement: PlacementOverrides,
    #[serde(default)]
    admission: AdmissionOverrides,
    #[serde(default)]
    estimates: EstimatesOverrides,
    #[serde(default)]
    sync: SyncSection,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SyncSection {
    #[serde(default)]
    paths: Vec<String>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct HousekeepingSection {
    interval: Option<String>,
    at_deadline: Option<AtDeadline>,
    idle_after: Option<String>,
    remove_deleted_workspaces: Option<bool>,
    min_free_percent: Option<u8>,
    min_free: Option<String>,
    compact_store_packs: Option<u32>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct WorkspaceSection {
    #[serde(default)]
    location: Setting,
    #[serde(default)]
    root: Option<PathBuf>,
    #[serde(default)]
    object_sources: Vec<PathBuf>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct PlacedRunsSection {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    leftovers_cap: Option<String>,
}

/// The longest `[placed_runs] leftovers_cap` may be.
const MAXIMUM_LEFTOVERS_CAP: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct CapacitySection {
    #[serde(default)]
    reserved_cores: Cores,
}

const LIMIT: u64 = 64 * 1024;
/// Each source is tried in turn when a sync lacks history.
const MAXIMUM_SOURCES: usize = 16;

/// Reads the configuration; a missing file means defaults.
pub fn load(path: &Path) -> Result<ServiceConfiguration> {
    let mut text = String::new();
    match std::fs::File::open(path) {
        Ok(file) => file
            .take(LIMIT + 1)
            .read_to_string(&mut text)
            .map_err(|e| Failure::io("Read service configuration", e))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(e) => return Err(Failure::io("Read service configuration", e)),
    };
    parse(&text, path)
}

/// Validates service settings without reading or changing files.
pub fn parse(text: &str, path: &Path) -> Result<ServiceConfiguration> {
    let invalid = |detail: String| {
        Failure::new(
            "invalid_configuration",
            format!("The service configuration is invalid: {detail}"),
            format!(
                "Fix {} (see README, \"Service configuration\"), then run elsecall service restart.",
                path.display()
            ),
        )
    };
    if text.len() as u64 > LIMIT {
        return Err(invalid("the file is larger than 64 KiB.".into()));
    }
    let file: File = toml::from_str(text).map_err(|_| {
        invalid("unknown section/key, invalid TOML or a value of the wrong type".into())
    })?;
    let plain_absolute = |path: &Path| {
        path.is_absolute()
            && !path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
    };
    if let Some(root) = &file.workspace.root
        && !plain_absolute(root)
    {
        return Err(invalid(
            "[workspace] root must be an absolute path without ..".into(),
        ));
    }
    if file.workspace.object_sources.len() > MAXIMUM_SOURCES
        || !file
            .workspace
            .object_sources
            .iter()
            .all(|source| plain_absolute(source) && source.to_str().is_some())
    {
        return Err(invalid(format!(
            "[workspace] object_sources must list at most {MAXIMUM_SOURCES} absolute paths of git repositories, without .."
        )));
    }
    if let Some(path) = &file.placed_runs.path
        && (path.is_empty() || path.contains('\0'))
    {
        return Err(invalid(
            "[placed_runs] path must be a PATH such as /opt/homebrew/bin:/usr/bin:/bin.".into(),
        ));
    }
    let leftovers_cap = match &file.placed_runs.leftovers_cap {
        None => Cap::default(),
        Some(text) => humantime::parse_duration(text)
            .ok()
            .filter(|cap| *cap <= MAXIMUM_LEFTOVERS_CAP)
            .map(Cap)
            .ok_or_else(|| {
                invalid(
                    "[placed_runs] leftovers_cap must be a duration such as \"10m\", from \"0s\" to \"24h\"."
                        .into(),
                )
            })?,
    };
    let parameters = Overrides {
        placement: file.placement,
        admission: file.admission,
        estimates: file.estimates,
    }
    .apply(&Parameters::default())
    .map_err(invalid)?;
    let sync_paths = file
        .sync
        .paths
        .iter()
        .map(|text| crate::config_sync::synced_path::SyncedPath::parse(text))
        .collect::<std::result::Result<Vec<_>, _>>()
        .and_then(|paths| {
            crate::config_sync::synced_path::check_set(
                &paths,
                crate::config_sync::manifest::MAXIMUM_PATHS,
            )
            .map(|()| paths)
        })
        .map_err(|why| invalid(format!("[sync] paths: {why}.")))?;
    let mut logs = crate::service_log::Settings::default();
    if let Some(size) = file.logs.max_size {
        logs.max_bytes = settings::parse_size(&size)
            .filter(|n| (1024..=1024 * 1024 * 1024).contains(n))
            .ok_or_else(|| invalid("[logs] max_size must be from 1024 B to 1 GiB.".into()))?;
    }
    if let Some(keep) = file.logs.keep {
        if !(1..=100).contains(&keep) {
            return Err(invalid("[logs] keep must be from 1 to 100.".into()));
        }
        logs.keep = keep;
    }
    let sync_backups = file
        .retention
        .sync_backups
        .unwrap_or(crate::config_sync::state::BACKUPS_TOTAL);
    if !(1..=10_000).contains(&sync_backups) {
        return Err(invalid(
            "[retention] sync_backups must be from 1 to 10000.".into(),
        ));
    }
    Ok(ServiceConfiguration {
        logs,
        sync_backups,
        sync_paths,
        parameters,
        reserved: file.capacity.reserved_cores,
        location: file.workspace.location,
        workspace_root: file.workspace.root,
        placed_path: file.placed_runs.path,
        leftovers_cap,
        object_sources: file.workspace.object_sources,
        housekeeping: housekeeping(file.housekeeping).map_err(|e| invalid(e.into()))?,
    })
}

fn housekeeping(section: HousekeepingSection) -> std::result::Result<Housekeeping, &'static str> {
    let defaults = Housekeeping::default();
    let duration = |text: Option<String>, default, what| match text {
        None => Ok(default),
        Some(text) => humantime::parse_duration(&text)
            .ok()
            .filter(|d| !d.is_zero() && *d <= std::time::Duration::from_secs(10 * 365 * 86_400))
            .ok_or(what),
    };
    let interval = duration(
        section.interval,
        defaults.interval,
        "[housekeeping] interval must be a duration such as \"1h\", at most 7 days.",
    )?;
    if interval > crate::scheduling::request::MAXIMUM_DEADLINE {
        return Err("[housekeeping] interval must be a duration such as \"1h\", at most 7 days.");
    }
    let min_free_percent = section
        .min_free_percent
        .unwrap_or(defaults.min_free_percent);
    if min_free_percent > 90 {
        return Err("[housekeeping] min_free_percent must be from 0 to 90.");
    }
    Ok(Housekeeping {
        interval,
        at_deadline: section.at_deadline.unwrap_or(defaults.at_deadline),
        idle_after: duration(
            section.idle_after,
            defaults.idle_after,
            "[housekeeping] idle_after must be a duration such as \"14d\".",
        )?,
        remove_deleted_workspaces: section
            .remove_deleted_workspaces
            .unwrap_or(defaults.remove_deleted_workspaces),
        min_free_percent,
        min_free_bytes: match section.min_free {
            None => defaults.min_free_bytes,
            Some(text) => settings::parse_size(&text)
                .ok_or("[housekeeping] min_free must be a size such as \"50GB\".")?,
        },
        compact_store_packs: section
            .compact_store_packs
            .unwrap_or(defaults.compact_store_packs),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_log_and_backup_limits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let defaults = load(&path).unwrap();
        assert_eq!(defaults.logs, crate::service_log::Settings::default());
        assert_eq!(defaults.sync_backups, 1000);
        std::fs::write(
            &path,
            "[logs]\nmax_size = \"2MB\"\nkeep = 3\n[retention]\nsync_backups = 50\n",
        )
        .unwrap();
        let config = load(&path).unwrap();
        assert_eq!(config.logs.max_bytes, 2_000_000);
        assert_eq!(config.logs.keep, 3);
        assert_eq!(config.sync_backups, 50);
    }

    #[test]
    fn reads_reserve_defaults_when_absent_and_rejects_invalid_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(load(&path).unwrap().reserved, Cores::ZERO);
        std::fs::write(&path, "[capacity]\nreserved_cores = 2.5\n").unwrap();
        assert_eq!(load(&path).unwrap().reserved, Cores::from_millis(2500));
        std::fs::write(&path, "").unwrap();
        assert_eq!(load(&path).unwrap().reserved, Cores::ZERO);
        assert_eq!(load(&path).unwrap().location, Setting::Mapped);
        std::fs::write(
            &path,
            "[workspace]\nlocation = \"same-path\"\nroot = \"/w\"\n[placed_runs]\npath = \"/usr/bin\"\n",
        )
        .unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.location, Setting::SamePath);
        assert_eq!(loaded.workspace_root, Some(PathBuf::from("/w")));
        assert_eq!(loaded.placed_path.as_deref(), Some("/usr/bin"));
        assert_eq!(
            loaded.leftovers_cap,
            Cap(std::time::Duration::from_secs(600))
        );
        assert!(loaded.object_sources.is_empty());
        std::fs::write(&path, "[placed_runs]\nleftovers_cap = \"90s\"\n").unwrap();
        assert_eq!(
            load(&path).unwrap().leftovers_cap,
            Cap(std::time::Duration::from_secs(90))
        );
        std::fs::write(&path, "[placed_runs]\nleftovers_cap = \"0s\"\n").unwrap();
        assert_eq!(
            load(&path).unwrap().leftovers_cap,
            Cap(std::time::Duration::ZERO),
            "stopped at once"
        );
        std::fs::write(
            &path,
            "[workspace]\nobject_sources = [\"/Users/me/project\", \"/srv/mirror.git\"]\n",
        )
        .unwrap();
        assert_eq!(
            load(&path).unwrap().object_sources,
            [PathBuf::from("/Users/me/project"), "/srv/mirror.git".into()]
        );
        assert_eq!(loaded.housekeeping, Housekeeping::default());
        assert_eq!(loaded.parameters, Parameters::default());
        std::fs::write(
            &path,
            "[placement]\nkeep_local_margin = 0.2\nkeep_local_margin_floor = \"5s\"\nrun_prior = \"90s\"\n[admission]\nsaturated_utilization = 0.8\nmemory_pressure = \"critical\"\n[estimates]\nminimum_runs = 5\nrecency = \"7d\"\n",
        )
        .unwrap();
        let parameters = load(&path).unwrap().parameters;
        assert_eq!(parameters.placement.keep_local_margin, 0.2);
        assert_eq!(
            parameters.placement.keep_local_margin_floor,
            std::time::Duration::from_secs(5)
        );
        assert_eq!(
            parameters.placement.run_prior,
            std::time::Duration::from_secs(90)
        );
        assert_eq!(parameters.admission.saturated_utilization, 0.8);
        assert_eq!(parameters.estimates.minimum_runs, 5);
        assert_eq!(
            parameters.estimates.recency,
            std::time::Duration::from_secs(7 * 86_400)
        );
        assert_eq!(
            parameters.placement.cold_run_factor,
            Parameters::default().placement.cold_run_factor
        );
        std::fs::write(
            &path,
            "[housekeeping]\ninterval = \"30m\"\nat_deadline = \"start\"\nidle_after = \"7d\"\nremove_deleted_workspaces = false\nmin_free_percent = 5\nmin_free = \"20GB\"\ncompact_store_packs = 0\n",
        )
        .unwrap();
        assert_eq!(
            load(&path).unwrap().housekeeping,
            Housekeeping {
                interval: std::time::Duration::from_secs(1800),
                at_deadline: AtDeadline::Start,
                idle_after: std::time::Duration::from_secs(7 * 86_400),
                remove_deleted_workspaces: false,
                min_free_percent: 5,
                min_free_bytes: 20_000_000_000,
                compact_store_packs: 0,
            }
        );
        assert!(loaded.sync_paths.is_empty());
        std::fs::write(&path, "[sync]\npaths = [\"~/.config/acme-ios\"]\n").unwrap();
        assert_eq!(
            load(&path).unwrap().sync_paths[0].to_string(),
            "~/.config/acme-ios"
        );
        for invalid in [
            "[capacity]\nreserved_cores = -1\n",
            "[capacity]\nreserved = 1\n",
            "[unknown]\n",
            "[workspace]\nlocation = \"elsewhere\"\n",
            "[workspace]\nroot = \"relative\"\n",
            "[workspace]\nobject_sources = [\"relative\"]\n",
            "[workspace]\nobject_sources = [\"/a/../b\"]\n",
            "[workspace]\nobject_sources = \"/a\"\n",
            "[housekeeping]\ninterval = \"0s\"\n",
            "[housekeeping]\ninterval = \"30d\"\n",
            "[housekeeping]\nidle_after = \"soon\"\n",
            "[housekeeping]\nmin_free = \"lots\"\n",
            "[housekeeping]\nmin_free_percent = 95\n",
            "[housekeeping]\nat_deadline = \"later\"\n",
            "[housekeeping]\nidle = \"14d\"\n",
            "[placement]\nkeep_local_margin = 1.5\n",
            "[placement]\nmargin = 0.2\n",
            "[admission]\nsaturated_utilization = 0\n",
            "[estimates]\nrecency = \"90d\"\n",
            "[sync]\npaths = [\"~/.ssh\"]\n",
            "[sync]\npaths = [\"/etc/hosts\"]\n",
            "[sync]\npaths = [\"~/.config/a\", \"~/.config/a/b\"]\n",
            "[sync]\nfiles = []\n",
            "[placed_runs]\nleftovers_cap = \"25h\"\n",
            "[placed_runs]\nleftovers_cap = \"later\"\n",
            "[placed_runs]\nleftovers_cap = 600\n",
            "[logs]\nmax_size = \"0B\"\n",
            "[logs]\nkeep = 0\n",
            "[logs]\nkeep = 101\n",
            "[logs]\nmax_size = \"2GB\"\n",
            "[retention]\nsync_backups = 0\n",
            "not toml",
        ] {
            std::fs::write(&path, invalid).unwrap();
            assert_eq!(load(&path).unwrap_err().code, "invalid_configuration");
        }
    }
}
