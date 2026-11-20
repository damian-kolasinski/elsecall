//! A member's drift signal: one hash over everything its plan depends on, read
//! without running any tool, so the master's provisioning loop can tell every
//! minute whether anything changed without planning. See
//! `docs/fleet-provisioning.md`, "Delivery 6".
//!
//! It is two hashes. `pins` covers the pins of the member's recent workspaces
//! (read as data, without when each last ran) and, on the master, its global
//! mise config's pins and `fleet-tools.toml`: every member's plan reads them.
//! `tools` covers where the member's tools live (names, sizes and modification
//! times one level deep, never their content), its build and whether its Xcode
//! privileges are ready: its own plan reads them, and the master's resolves
//! every member's. Only the hashes leave the member, with two facts the loop
//! acts on: whether its privileges are ready, and the state of its latest
//! apply. `GET /peer/v1/tools/drift` answers it to enrolled members.
use crate::{
    install::{Applier, record::Record},
    pins,
    plan_api::{self, RecentWorkspace, RecentWorkspaces},
};
use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::get};
use elsecall_contract::{
    api,
    failure::{Failure, Result},
    scheduling_model::timestamp::Timestamp,
};
use elsecall_fleet::{fleet::Fleet, provisioning_model::ApplyState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
};

pub const PEER_ROUTE: &str = "/peer/v1/tools/drift";

/// What the loop learns from a member each minute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Signal {
    /// SHA-256 over the pins this member reads, in hex.
    pub(crate) pins: String,
    /// SHA-256 over where its tools live, in hex.
    pub(crate) tools: String,
    pub(crate) privileges_ready: bool,
    /// The member's latest apply, none when it never applied a plan.
    pub(crate) apply: Option<ApplySeen>,
}

/// The state of a member's latest apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ApplySeen {
    pub(crate) state: ApplyState,
    pub(crate) started_at: Timestamp,
}

impl ApplySeen {
    pub(crate) fn of(record: &Record) -> Self {
        Self {
            state: if record.running() {
                ApplyState::Running
            } else {
                ApplyState::Finished
            },
            started_at: record.started_at,
        }
    }
}

/// What this member reads its signal from.
pub struct Drift {
    recent: Arc<dyn RecentWorkspaces>,
    applier: Arc<Applier>,
    build: String,
    /// `fleet-tools.toml`, which counts only while this member is the master.
    exceptions: PathBuf,
}

impl Drift {
    pub fn new(
        recent: Arc<dyn RecentWorkspaces>,
        applier: Arc<Applier>,
        build: String,
        data: &Path,
    ) -> Arc<Self> {
        Arc::new(Self {
            recent,
            applier,
            build,
            exceptions: crate::toolset::store::path(data),
        })
    }

    /// This member's signal; `master` adds what only the master's plan reads.
    pub(crate) async fn signal(self: &Arc<Self>, master: bool) -> Result<Signal> {
        let drift = Arc::clone(self);
        tokio::task::spawn_blocking(move || drift.read(master))
            .await
            .map_err(|_| {
                Failure::new(
                    "tools_failed",
                    "Reading this member's drift signal stopped unexpectedly.",
                    "Run elsecall service restart, then retry.",
                )
            })?
    }

    fn read(&self, master: bool) -> Result<Signal> {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                Failure::new(
                    "pin_source_unreadable",
                    "HOME is not set, so workspaces cannot be named.",
                    "Run elsecall service restart from a login session.",
                )
            })?;
        // The apply first: one that ends while the folders are read is then
        // seen finished at the next look, with what it installed.
        let apply = self.applier.latest().as_ref().map(ApplySeen::of);
        let privileges_ready = privileges_ready();
        let mut pins = Hashed::default();
        pins.part("workspaces", &workspace_pins(&self.recent.recent(), &home));
        if master {
            let global = pins::global_pins(&home)
                // Its Debug form is enough within one build, which `tools` names.
                .map(|reading| format!("{:?}", reading.pins).into_bytes())
                .unwrap_or_else(|failure| failure.code.into_bytes());
            pins.part("global", &global);
            pins.part(
                "exceptions",
                &std::fs::read(&self.exceptions).unwrap_or_default(),
            );
        }
        let mut tools = Hashed::default();
        tools.part("build", self.build.as_bytes());
        tools.part("privileges", &[u8::from(privileges_ready)]);
        for place in locations(&home) {
            tools.part(place.name, &listing(&place));
        }
        Ok(Signal {
            pins: pins.finish(),
            tools: tools.finish(),
            privileges_ready,
            apply,
        })
    }
}

/// The member route, behind the member API's authentication.
pub fn peer_routes(drift: Arc<Drift>) -> Router<Arc<Fleet>> {
    Router::new().route(
        PEER_ROUTE,
        get(move || {
            let drift = Arc::clone(&drift);
            async move {
                match drift.signal(false).await {
                    Ok(signal) => Json(api::Response::new(signal)).into_response(),
                    Err(error) => {
                        elsecall_api_response::failure(StatusCode::INTERNAL_SERVER_ERROR, error)
                    }
                }
            }
        }),
    )
}

/// Whether the Xcode installer would find its root steps allowed, as it checks.
fn privileges_ready() -> bool {
    use elsecall_host::command_capture::{OverridePath, classify, path_override};
    match classify(path_override("ELSECALL_XCODE_ADMIN")) {
        OverridePath::Absolute(_) => true,
        OverridePath::NotAbsolute => false,
        OverridePath::Unset => crate::xcode_privileges::status().is_ok_and(|status| status.ready),
    }
}

/// The recent workspaces' pins as the plan reads them, ordered by name and
/// without when each last ran, which changes with every run.
fn workspace_pins(recent: &[RecentWorkspace], home: &Path) -> Vec<u8> {
    let report = plan_api::read_report(recent, home);
    let mut value = serde_json::to_value(&report).expect("a report serializes");
    if let Some(workspaces) = value["workspaces"].as_array_mut() {
        for workspace in workspaces.iter_mut() {
            if let Some(object) = workspace.as_object_mut() {
                object.remove("last_run_at");
            }
        }
        workspaces.sort_by(|a, b| a["workspace"].as_str().cmp(&b["workspace"].as_str()));
    }
    serde_json::to_vec(&value).expect("a value serializes")
}

/// A place the inventory reads tools from, and which of its entries count.
struct Place {
    name: &'static str,
    path: PathBuf,
    entries: Entries,
}

#[derive(Clone, Copy)]
enum Entries {
    /// Every entry but dotfiles, stamped: mise's installs, one folder a tool.
    All,
    /// Only Xcode bundles, stamped: an application folder holds much else that
    /// updates by itself.
    Xcodes,
    /// Entries by name only: mount points, whose times change as they mount.
    Names,
    /// The file or folder itself.
    Itself,
}

/// Where this member's tools live, as the inventory finds them.
fn locations(home: &Path) -> Vec<Place> {
    use elsecall_host::command_capture::path_override;
    let place = |name, path, entries| Place {
        name,
        path,
        entries,
    };
    let data = path_override("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
    let mut places = vec![place(
        "mise_installs",
        data.join("mise/installs"),
        Entries::All,
    )];
    places.extend(
        crate::mise_cli::candidates()
            .into_iter()
            .map(|path| place("mise", path, Entries::Itself)),
    );
    places.extend(
        crate::xcodes::directories()
            .into_iter()
            .flatten()
            .map(|path| place("xcodes", path, Entries::Xcodes)),
    );
    match path_override("ELSECALL_SIMCTL") {
        Some(path) => places.push(place("simulators", path, Entries::Itself)),
        None => {
            let simulators = Path::new("/Library/Developer/CoreSimulator");
            places.push(place("simulators", simulators.join("Images"), Entries::All));
            places.push(place(
                "simulators",
                simulators.join("Volumes"),
                Entries::Names,
            ));
        }
    }
    places
}

/// A place's identity and its counted entries': name, and for most, size and
/// modification time, links followed. Nothing is read from inside a file.
fn listing(place: &Place) -> Vec<u8> {
    let mut out = place.path.as_os_str().as_encoded_bytes().to_vec();
    let Ok(metadata) = std::fs::metadata(&place.path) else {
        out.extend_from_slice(b"\0missing");
        return out;
    };
    if matches!(place.entries, Entries::Itself) || !metadata.is_dir() {
        stamp(&mut out, &metadata);
        return out;
    }
    let Ok(entries) = std::fs::read_dir(&place.path) else {
        return out;
    };
    let mut entries: Vec<(Vec<u8>, Option<std::fs::Metadata>)> = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            !name.starts_with('.')
                && match place.entries {
                    Entries::Xcodes => name.starts_with("Xcode") && name.ends_with(".app"),
                    _ => true,
                }
        })
        .take(4_096)
        .map(|entry| {
            let stamped = !matches!(place.entries, Entries::Names);
            (
                entry.file_name().as_encoded_bytes().to_vec(),
                stamped
                    .then(|| std::fs::metadata(entry.path()).ok())
                    .flatten(),
            )
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, metadata) in entries {
        out.push(0);
        out.extend_from_slice(&name);
        if let Some(metadata) = metadata {
            stamp(&mut out, &metadata);
        }
    }
    out
}

fn stamp(out: &mut Vec<u8>, metadata: &std::fs::Metadata) {
    out.extend_from_slice(&metadata.size().to_le_bytes());
    out.extend_from_slice(&metadata.mtime().to_le_bytes());
    out.extend_from_slice(&metadata.mtime_nsec().to_le_bytes());
    out.extend_from_slice(&metadata.ino().to_le_bytes());
}

/// SHA-256 over named parts, each length-prefixed so no two inputs collide.
#[derive(Default)]
struct Hashed(Sha256);

impl Hashed {
    fn part(&mut self, name: &str, bytes: &[u8]) {
        for piece in [name.as_bytes(), bytes] {
            self.0.update((piece.len() as u64).to_le_bytes());
            self.0.update(piece);
        }
    }

    fn finish(self) -> String {
        self.0
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::drift::*;

    #[test]
    fn a_listing_changes_with_what_is_installed_and_not_otherwise() {
        let root = tempfile::tempdir().unwrap();
        let installs = root.path().join("installs");
        std::fs::create_dir_all(installs.join("node/22.19.0")).unwrap();
        let place = |path: &Path, entries| Place {
            name: "test",
            path: path.to_owned(),
            entries,
        };
        let first = listing(&place(&installs, Entries::All));
        assert_eq!(
            listing(&place(&installs, Entries::All)),
            first,
            "nothing changed"
        );
        std::fs::create_dir_all(installs.join("node/24.0.0")).unwrap();
        // The tool's folder changed; a coarse clock may need a moment.
        let set = std::fs::FileTimes::new()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5));
        std::fs::File::open(installs.join("node"))
            .unwrap()
            .set_times(set)
            .unwrap();
        assert_ne!(
            listing(&place(&installs, Entries::All)),
            first,
            "a version was added"
        );
        let missing = listing(&place(&root.path().join("absent"), Entries::All));
        assert!(missing.ends_with(b"\0missing"));

        let applications = root.path().join("Applications");
        std::fs::create_dir_all(applications.join("Xcode-26.2.0.app")).unwrap();
        let xcodes = listing(&place(&applications, Entries::Xcodes));
        std::fs::create_dir_all(applications.join("Other.app")).unwrap();
        std::fs::write(applications.join(".DS_Store"), "x").unwrap();
        assert_eq!(
            listing(&place(&applications, Entries::Xcodes)),
            xcodes,
            "other applications do not count"
        );
    }

    #[test]
    fn parts_are_framed() {
        let hash = |parts: &[(&str, &[u8])]| {
            let mut hashed = Hashed::default();
            for (name, bytes) in parts {
                hashed.part(name, bytes);
            }
            hashed.finish()
        };
        assert_ne!(hash(&[("a", b"bc")]), hash(&[("ab", b"c")]));
        assert_eq!(hash(&[("a", b"b")]).len(), 64);
    }

    #[test]
    fn workspace_pins_leave_out_when_each_last_ran() {
        let home = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(home.path()).unwrap();
        let workspace = home.join("acme");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("mise.toml"), "[tools]\nnode = \"22.19.0\"\n").unwrap();
        let recent = |at: &str| {
            vec![RecentWorkspace {
                root: workspace.clone(),
                last_run_at: Timestamp::parse_rfc3339(at).unwrap(),
            }]
        };
        let earlier = workspace_pins(&recent("2026-11-20T09:00:00Z"), &home);
        assert_eq!(
            workspace_pins(&recent("2026-11-20T10:00:00Z"), &home),
            earlier
        );
        std::fs::write(workspace.join("mise.toml"), "[tools]\nnode = \"24.0.0\"\n").unwrap();
        assert_ne!(
            workspace_pins(&recent("2026-11-20T10:00:00Z"), &home),
            earlier,
            "a pin changed"
        );
    }
}
