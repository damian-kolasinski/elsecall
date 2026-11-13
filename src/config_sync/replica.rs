//! This member's synced set on disk, whatever its role: issuing generations from
//! its own files when it is master, applying generations otherwise, planning,
//! and reporting its state. Every operation here is blocking file work; the
//! service runs it off the async runtime.
mod service_settings;

use crate::{
    config_sync::{
        compare::{self, Change, FileDifference},
        disk::{self, FingerprintCache, Home, LiveTree, Location},
        fingerprint::{Fingerprint, SyncKey},
        history::{Event, History, PathRecord, Record},
        manifest::{FileKey, FileNode, Manifest, Tree},
        pins::{Pin, Pins},
        report::{
            self, Applied, BackupView, FileReport, PathReport, PathState, SyncRole, SyncSummary,
        },
        state::{Backup, State},
        store::ReplicaStore,
        synced_path::SyncedPath,
    },
    failure::{Failure, Result},
    private_files,
    roster::MemberName,
    scheduling::timestamp::Timestamp,
    workspace::confined::Confined,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};
use zeroize::Zeroizing;

/// Where a member keeps what it needs to sync.
pub struct Layout {
    /// The member's home, where synced paths live.
    pub home: PathBuf,
    /// Sync's own directory in Elsecall's state.
    pub state: PathBuf,
    /// Where the member's override files go, mirroring paths below home.
    pub overrides: PathBuf,
    /// Further absolute locations synced paths must not touch, such as the
    /// workspace root.
    pub forbidden: Vec<PathBuf>,
}

impl Layout {
    pub fn for_home(home: &Path, data: &Path) -> Self {
        Self {
            home: home.to_owned(),
            state: data.join("config-sync"),
            overrides: home.join(".elsecall/sync-overrides"),
            forbidden: Vec::new(),
        }
    }
}

/// Who applies a generation: a master only lays its own overrides over its files
/// and never backs up or replaces its other files, which are the source.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ApplyAs {
    Master,
    Member,
}

/// A master's files for one synced path, ready to issue.
#[derive(Clone, Debug, PartialEq)]
pub struct Scanned {
    pub tree: Tree,
    pub skipped: Vec<String>,
    pub overridden: Vec<String>,
}

/// One synced path of an application, checked before anything changes.
struct Step {
    path: SyncedPath,
    desired: Tree,
    overridden: BTreeSet<FileKey>,
    location: Location,
    live: LiveTree,
}

pub struct Replica {
    home: Home,
    overrides: PathBuf,
    state_file: PathBuf,
    backups: PathBuf,
    backup_limit: usize,
    store: Arc<dyn ReplicaStore>,
    cache: FingerprintCache,
    state: Mutex<State>,
    history: History,
    /// Content pushes in progress still read, kept when a generation is issued.
    pins: Arc<Pins>,
    service_identity: Mutex<Option<MemberName>>,
    service_active: Mutex<Option<(Fingerprint, u64)>>,
}

fn missing_content() -> Failure {
    Failure::new(
        "sync_content_missing",
        "Synced content this generation needs is not on this member.",
        "Run elsecall sync push on the master; the previous synced state stays in place.",
    )
}

impl Replica {
    pub fn open(layout: Layout, store: Arc<dyn ReplicaStore>) -> Result<Self> {
        Self::open_with_backup_limit(layout, store, crate::config_sync::state::BACKUPS_TOTAL)
    }

    pub fn open_with_backup_limit(
        layout: Layout,
        store: Arc<dyn ReplicaStore>,
        backup_limit: usize,
    ) -> Result<Self> {
        private_files::directory(&layout.state)?;
        let state_file = layout.state.join("state.json");
        let mut state = State::load(&state_file)?;
        let backups = layout.state.join("backups");
        let removed = state.trim_backups(backup_limit);
        if !removed.is_empty() {
            retain_failed_backups(&mut state, remove_backups(&removed, &backups));
            state.save(&state_file)?;
        }
        remove_orphan_backups(&backups, &state.backups)?;
        let replica = Self {
            home: Home::new(&layout.home, &layout.forbidden),
            overrides: layout.overrides,
            backups,
            backup_limit,
            history: History::new(&layout.state.join("history.jsonl")),
            state_file,
            store,
            cache: FingerprintCache::default(),
            state: Mutex::new(state),
            pins: Arc::default(),
            service_identity: Mutex::new(None),
            service_active: Mutex::new(None),
        };
        replica.trim_service_backups()?;
        Ok(replica)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("sync state lock")
    }

    pub fn history(&self) -> &History {
        &self.history
    }

    /// The manifest this member holds.
    pub fn manifest(&self) -> Option<Manifest> {
        self.lock().manifest.clone()
    }

    /// The fleet's fingerprint key, created on the first master that needs one.
    pub fn key(&self) -> Result<SyncKey> {
        let mut state = self.lock();
        if let Some(key) = &state.key {
            return Ok(key.clone());
        }
        let key = SyncKey::generate().ok_or_else(|| {
            Failure::new(
                "random_unavailable",
                "The operating system's random number generator failed.",
                "Retry; restart the machine if it keeps failing.",
            )
        })?;
        state.key = Some(key.clone());
        state.save(&self.state_file)?;
        Ok(key)
    }

    /// The overrides directory for `path` on this member.
    pub fn overrides_of(&self, path: &SyncedPath) -> PathBuf {
        self.overrides.join(path.relative())
    }

    fn live(&self, path: &SyncedPath, key: &SyncKey) -> Result<(Option<Location>, LiveTree)> {
        match self.home.locate(path, false)? {
            None => Ok((None, LiveTree::default())),
            Some(location) => {
                let live = disk::read_tree(&location, key, &self.cache)?;
                Ok((Some(location), live))
            }
        }
    }

    /// This member's overrides for `path`. The overrides directory must be
    /// owner-only, since overrides hold secrets too.
    fn overrides(&self, path: &SyncedPath, key: &SyncKey) -> Result<Tree> {
        match std::fs::symlink_metadata(&self.overrides) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Tree::Absent),
            _ => private_files::check_directory(&self.overrides).map_err(|_| {
                Failure::new(
                    "unsafe_overrides",
                    "The sync overrides directory must be a real directory owned by you with mode 0700.",
                    format!("Run chmod 700 {}.", self.overrides.display()),
                )
            })?,
        }
        let root = Confined::open(&self.overrides).map_err(|e| Failure::io("Open overrides", e))?;
        match disk::locate_inside(&root, path)? {
            None => Ok(Tree::Absent),
            Some(location) => Ok(disk::read_tree(&location, key, &self.cache)?.tree),
        }
    }

    fn override_content(&self, path: &SyncedPath, key: &FileKey) -> Result<Zeroizing<Vec<u8>>> {
        let root = Confined::open(&self.overrides).map_err(|e| Failure::io("Open overrides", e))?;
        let location = disk::locate_inside(&root, path)?.ok_or_else(changed_meanwhile)?;
        disk::read_file(&location, key)
    }

    /// What this member should hold at `path` under `mirrored`.
    fn desired(
        &self,
        path: &SyncedPath,
        mirrored: &Tree,
        key: &SyncKey,
    ) -> Result<(Tree, BTreeSet<FileKey>)> {
        let overrides = self.overrides(path, key)?;
        Ok(compare::overlay(mirrored, &overrides))
    }

    /// Creates the overrides directory, owner-only, so operators find it.
    pub fn prepare_overrides(&self) -> Result<()> {
        private_files::directory(&self.overrides)
    }

    // Master side.

    /// The master's files for `paths`, as they would be issued now; where its own
    /// overrides decide, the previously issued entries stay.
    pub fn scan(&self, paths: &[SyncedPath]) -> Result<BTreeMap<SyncedPath, Scanned>> {
        let key = self.key()?;
        let (previous, written) = {
            let state = self.lock();
            (
                state.manifest.clone().map(|m| m.paths).unwrap_or_default(),
                state.written.clone(),
            )
        };
        let mut scanned = BTreeMap::new();
        for path in paths {
            self.home.check(path)?;
            let (_, live) = self.live(path, &key)?;
            if live.skipped.iter().any(String::is_empty) {
                return Err(Failure::new(
                    "unsupported_sync_path",
                    format!("{path} is a symbolic link or special file on the master."),
                    "Sync the directory or file it points to instead.",
                ));
            }
            let overrides = self.overrides(path, &key)?;
            let (_, mut decided) = compare::overlay(&Tree::Absent, &overrides);
            let before = previous.get(path).cloned().unwrap_or_default();
            // A file an override laid, whose override is gone but which nobody
            // has edited since, still holds the override's value: the fleet's
            // value is restored there, never issued from it.
            if let Some(laid) = written.get(path) {
                let fleet = before.files();
                let held = live.tree.files();
                for (file_key, file) in laid.files() {
                    if fleet.get(&file_key) != Some(&file) && held.get(&file_key) == Some(&file) {
                        decided.insert(file_key);
                    }
                }
            }
            scanned.insert(
                path.clone(),
                Scanned {
                    tree: compare::issued_from(&live.tree, &decided, &before),
                    skipped: live.skipped,
                    overridden: decided.iter().map(|k| report::file_name(path, k)).collect(),
                },
            );
        }
        Ok(scanned)
    }

    /// A manifest of `trees` under the fleet key, not yet issued, for planning.
    pub fn draft(&self, trees: BTreeMap<SyncedPath, Tree>, by: MemberName) -> Result<Manifest> {
        let generation = self.manifest().map(|m| m.generation).unwrap_or(0) + 1;
        let manifest = Manifest {
            generation,
            issued_by: by,
            key: self.key()?,
            paths: trees,
            service: self.manifest().and_then(|m| m.service),
        };
        manifest.validate().map_err(|why| {
            Failure::new(
                "sync_set_invalid",
                format!("The synced set cannot be issued: {why}."),
                "Disable a path or sync a smaller one.",
            )
        })?;
        Ok(manifest)
    }

    /// Issues `trees` as the next generation: their content goes into the store
    /// first, verified against what was scanned, so a file that changed meanwhile
    /// fails the issue until the next scan.
    pub fn issue(&self, trees: BTreeMap<SyncedPath, Tree>, by: MemberName) -> Result<Manifest> {
        self.issue_with_service(trees, by, self.manifest().and_then(|m| m.service))
    }

    pub fn issue_with_service(
        &self,
        trees: BTreeMap<SyncedPath, Tree>,
        by: MemberName,
        service: Option<FileNode>,
    ) -> Result<Manifest> {
        let mut manifest = self.draft(trees, by)?;
        manifest.service = service;
        manifest
            .validate()
            .map_err(crate::service_configuration::invalid)?;
        for (path, tree) in &manifest.paths {
            for (key, file) in tree.files() {
                if self.store.contains(&file.fingerprint) {
                    continue;
                }
                let location = self
                    .home
                    .locate(path, false)?
                    .ok_or_else(changed_meanwhile)?;
                let content = disk::read_file(&location, &key)?;
                self.store
                    .put(&manifest.key, &file, &content)
                    .map_err(|_| changed_meanwhile())?;
            }
        }
        let applied = self.apply(&manifest, ApplyAs::Master)?;
        let _ = self.history.append(&Record {
            at: Timestamp::now(),
            event: Event::Issued,
            generation: manifest.generation,
            manifest_id: manifest.id().short(),
            member: None,
            paths: applied.paths,
            error: None,
        });
        Ok(manifest)
    }

    /// Keeps `manifest`'s content in the store until the pin is dropped, even if
    /// a later generation no longer needs it: a push of it may be in progress.
    pub fn pin(&self, manifest: &Manifest) -> Pin {
        self.pins.pin(manifest.fingerprints())
    }

    // Member side.

    pub fn store(&self) -> &dyn ReplicaStore {
        &*self.store
    }

    /// The fingerprints of `manifest` this member's store lacks.
    pub fn missing(&self, manifest: &Manifest) -> Vec<Fingerprint> {
        manifest
            .fingerprints()
            .into_iter()
            .filter(|f| !self.store.contains(f))
            .collect()
    }

    /// How `offered` would change this member, without changing anything.
    pub fn plan(&self, offered: &Manifest) -> Vec<PathReport> {
        let written = {
            let state = self.lock();
            let same_key = state.key.as_ref() == Some(&offered.key) && !state.issued_here;
            if same_key {
                state.written.clone()
            } else {
                BTreeMap::new()
            }
        };
        offered
            .paths
            .iter()
            .map(|(path, mirrored)| {
                let compared = self.home.check(path).and_then(|()| {
                    let (desired, overridden) = self.desired(path, mirrored, &offered.key)?;
                    let (_, live) = self.live(path, &offered.key)?;
                    let differences =
                        compare::differences(&desired, &overridden, &live.tree, written.get(path));
                    Ok((desired, differences, live.skipped))
                });
                match compared {
                    Ok((desired, differences, skipped)) => PathReport {
                        path: path.clone(),
                        unchanged: desired.files().len().saturating_sub(
                            differences.iter().filter(|d| d.desired.is_some()).count(),
                        ),
                        files: differences
                            .iter()
                            .map(|d| FileReport::of(path, d))
                            .collect(),
                        skipped,
                        error: None,
                    },
                    Err(error) => PathReport {
                        path: path.clone(),
                        files: Vec::new(),
                        unchanged: 0,
                        skipped: Vec::new(),
                        error: Some(error),
                    },
                }
            })
            .collect()
    }

    /// Applies `manifest`. Every path is checked and every needed content is
    /// present before anything changes; each path is then replaced in one atomic
    /// step after this member's own content there is backed up. A failure leaves
    /// the application journaled, to finish at the next chance.
    pub fn apply(&self, manifest: &Manifest, role: ApplyAs) -> Result<Applied> {
        let mut state = self.lock();
        // A master's application is all or nothing: a failure must not leave what
        // it recorded as written ahead of the manifest, or the next scan would take
        // the master's own edits for laid overrides.
        let before = (role == ApplyAs::Master).then(|| state.clone());
        let result = self.apply_locked(&mut state, manifest, role);
        if let (Err(_), Some(before)) = (&result, before) {
            *state = before;
        }
        if role == ApplyAs::Member {
            let _ = self.history.append(&Record {
                at: Timestamp::now(),
                event: if result.is_ok() {
                    Event::Applied
                } else {
                    Event::ApplyFailed
                },
                generation: manifest.generation,
                manifest_id: manifest.id().short(),
                member: None,
                paths: result.as_ref().map(|a| a.paths.clone()).unwrap_or_default(),
                error: result.as_ref().err().map(|e| e.code.clone()),
            });
        }
        result
    }

    fn apply_locked(
        &self,
        state: &mut State,
        manifest: &Manifest,
        role: ApplyAs,
    ) -> Result<Applied> {
        if state.manifest.as_ref().is_some_and(|m| m.service.is_some())
            && manifest.service.is_none()
        {
            return Err(crate::service_configuration::invalid(
                "A managed service configuration cannot be removed by an ordinary file sync; restore or enable it on the master.",
            ));
        }
        let service = self.validate_service(manifest)?;
        let key = &manifest.key;
        // What sync wrote here can be trusted as not this member's own only if the
        // fleet key is the same and, for a member, another master wrote it.
        let same_key =
            state.key.as_ref() == Some(key) && (role == ApplyAs::Master || !state.issued_here);
        // Created owner-only so operators find where overrides go; reading
        // overrides reports a directory that is not.
        let _ = self.prepare_overrides();
        let mut steps = Vec::new();
        for (path, mirrored) in &manifest.paths {
            self.home.check(path)?;
            let (desired, overridden) = self.desired(path, mirrored, key)?;
            for (file_key, file) in desired.files() {
                if !overridden.contains(&file_key) && !self.store.contains(&file.fingerprint) {
                    return Err(missing_content());
                }
            }
            let location = self.home.locate(path, true)?.expect("created when missing");
            let live = disk::read_tree(&location, key, &self.cache)?;
            steps.push(Step {
                path: path.clone(),
                desired,
                overridden,
                location,
                live,
            });
        }
        if role == ApplyAs::Member {
            state.applying = Some(manifest.clone());
            state.save(&self.state_file)?;
        }
        let stamp = Timestamp::now();
        let mut records = Vec::new();
        let previous_fleet = state.manifest.clone().map(|m| m.paths).unwrap_or_default();
        for step in steps {
            let written = if same_key {
                state.written.get(&step.path).cloned()
            } else {
                None
            };
            let mut differences = compare::differences(
                &step.desired,
                &step.overridden,
                &step.live.tree,
                written.as_ref(),
            );
            let content = |file_key: &FileKey, node: &FileNode| -> Result<Zeroizing<Vec<u8>>> {
                if step.overridden.contains(file_key) {
                    let content = self.override_content(&step.path, file_key)?;
                    if key.fingerprint(&content) != node.fingerprint {
                        return Err(changed_meanwhile());
                    }
                    Ok(content)
                } else {
                    self.store.read(key, node)
                }
            };
            match role {
                // A master's own files are the source: it only writes, in place,
                // files its overrides decide now or laid before, never anything else.
                ApplyAs::Master => {
                    let fleet_before = previous_fleet
                        .get(&step.path)
                        .map(Tree::files)
                        .unwrap_or_default();
                    let laid_before: BTreeSet<FileKey> = written
                        .as_ref()
                        .map(Tree::files)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|(k, f)| fleet_before.get(k) != Some(f))
                        .map(|(k, _)| k)
                        .collect();
                    differences.retain(|d| {
                        d.live != d.desired
                            && (step.overridden.contains(&d.key) || laid_before.contains(&d.key))
                    });
                    for difference in &mut differences {
                        difference.backup = false;
                        let laid = match &difference.desired {
                            Some(node) => content(&difference.key, node).and_then(|bytes| {
                                disk::write_file(&step.location, &difference.key, node, &bytes)
                            }),
                            None => disk::remove_file(&step.location, &difference.key),
                        };
                        match laid {
                            Ok(()) => (),
                            // One override that does not fit the master's files is
                            // left unlaid, shown in status; the rest of the fleet
                            // keeps syncing.
                            Err(error) if error.code == "override_conflict" => crate::service_log!(
                                "{}",
                                serde_json::json!({
                                    "event": "config_sync_override_conflict",
                                    "file": report::file_name(&step.path, &difference.key),
                                })
                            ),
                            Err(error) => return Err(error),
                        }
                    }
                }
                ApplyAs::Member => {
                    if step.live.tree != step.desired || !step.live.skipped.is_empty() {
                        self.replace(state, &step, &differences, &content, stamp)?;
                    }
                }
            }
            records.push(report::record_of(&step.path, &differences));
            state
                .written
                .insert(step.path.clone(), step.desired.clone());
            if role == ApplyAs::Member {
                // Recorded as each path is replaced, so status never mistakes a
                // replaced path for local edits.
                state.save(&self.state_file)?;
            }
        }
        state
            .written
            .retain(|path, _| manifest.paths.contains_key(path));
        state.key = Some(key.clone());
        if let Some(bytes) = service {
            if role == ApplyAs::Member && !state.service_source_master {
                self.restore_service_source(&bytes, key)?;
            }
            if role == ApplyAs::Master {
                state.service_source_master = true;
            }
            state.service_rejection = None;
            state.service_member = self
                .service_identity
                .lock()
                .expect("service identity lock")
                .clone();
        }
        state.manifest = Some(manifest.clone());
        state.applying = None;
        state.issued_here = role == ApplyAs::Master;
        state.save(&self.state_file)?;
        let _ = self
            .store
            .retain(&self.pins.with_pinned(manifest.fingerprints()));
        Ok(Applied {
            generation: manifest.generation,
            manifest_id: manifest.id().short(),
            paths: records,
        })
    }

    /// Replaces one path on a member: the new tree is built, then this member's
    /// own content there is backed up and recorded, then the tree is swapped in. A
    /// tree holding entries sync cannot carry is kept whole as a backup.
    fn replace(
        &self,
        state: &mut State,
        step: &Step,
        differences: &[FileDifference],
        content: &disk::Content,
        stamp: Timestamp,
    ) -> Result<()> {
        let kept_at = self
            .backups
            .join(stamp.millis().to_string())
            .join(format!("{}.previous", step.path.relative()));
        let keep = !step.live.skipped.is_empty();
        if keep {
            private_files::directory(kept_at.parent().expect("inside backups"))?;
        }
        let mut before_swap = || -> Result<()> {
            for difference in differences.iter().filter(|d| d.backup) {
                self.back_up(state, &step.path, &step.location, difference, stamp)?;
            }
            state.save(&self.state_file)
        };
        let kept = disk::write_tree(
            &step.location,
            &step.desired,
            content,
            &mut before_swap,
            if keep {
                disk::Previous::KeepAt(&kept_at)
            } else {
                disk::Previous::Remove
            },
        )?;
        if let Some(location) = kept {
            let removed = state.add_backup_with_limit(
                Backup {
                    at: stamp,
                    path: step.path.clone(),
                    file: step.path.to_string(),
                    change: Change::Removed,
                    size: 0,
                    fingerprint: None,
                    location,
                },
                self.backup_limit,
            );
            retain_failed_backups(state, remove_backups(&removed, &self.backups));
        }
        Ok(())
    }

    /// Keeps this member's own version of a file before sync replaces it, unless
    /// its latest backup already holds the same content.
    fn back_up(
        &self,
        state: &mut State,
        path: &SyncedPath,
        location: &Location,
        difference: &FileDifference,
        stamp: Timestamp,
    ) -> Result<()> {
        if difference.live.is_none() {
            return Ok(());
        }
        let content = disk::read_file(location, &difference.key)?;
        let local = match &state.local_key {
            Some(key) => key.clone(),
            None => {
                let key = SyncKey::generate().ok_or_else(|| {
                    Failure::new(
                        "random_unavailable",
                        "The operating system's random number generator failed.",
                        "Retry; restart the machine if it keeps failing.",
                    )
                })?;
                state.local_key = Some(key.clone());
                key
            }
        };
        let fingerprint = local.fingerprint(&content);
        let file = report::file_name(path, &difference.key);
        let latest = state.backups.iter().rev().find(|b| b.file == file);
        if latest.is_some_and(|b| b.fingerprint == Some(fingerprint)) {
            return Ok(());
        }
        let mut target = self
            .backups
            .join(stamp.millis().to_string())
            .join(path.relative());
        if let Some(inner) = &difference.key {
            target.push(inner.as_str());
        }
        private_files::directory(target.parent().expect("inside backups"))?;
        private_files::replace(&target, &content).map_err(|e| Failure::io("Back up a file", e))?;
        let removed = state.add_backup_with_limit(
            Backup {
                at: stamp,
                path: path.clone(),
                file,
                change: difference.change,
                size: content.len() as u64,
                fingerprint: Some(fingerprint),
                location: target,
            },
            self.backup_limit,
        );
        retain_failed_backups(state, remove_backups(&removed, &self.backups));
        Ok(())
    }

    /// Before acting as master: a member that was not the source restores its
    /// replica first, backing up its own changes, so its local drift never
    /// becomes the fleet's truth. `None` when nothing needed doing.
    pub fn take_over(&self) -> Result<Option<Applied>> {
        let manifest = {
            let mut state = self.lock();
            if state.acting_as_source {
                return Ok(None);
            }
            // An application that had begun is finished first, not reverted.
            match state.applying.clone().or_else(|| state.manifest.clone()) {
                Some(manifest) => manifest,
                None => {
                    state.acting_as_source = true;
                    state.save(&self.state_file)?;
                    return Ok(None);
                }
            }
        };
        let applied = self.apply(&manifest, ApplyAs::Member)?;
        let mut state = self.lock();
        state.acting_as_source = true;
        state.save(&self.state_file)?;
        Ok(Some(applied))
    }

    /// Acting as a member again; the next role change to master restores first.
    pub fn stand_down(&self) -> Result<()> {
        let mut state = self.lock();
        if state.acting_as_source || state.service_source_master {
            state.acting_as_source = false;
            state.service_source_master = false;
            state.save(&self.state_file)?;
        }
        Ok(())
    }

    /// A master's own upkeep: lays its overrides again when they changed.
    pub fn reconcile_master(&self) -> Result<()> {
        let Some(manifest) = self.manifest() else {
            return Ok(());
        };
        let written = self.lock().written.clone();
        for (path, mirrored) in &manifest.paths {
            let (desired, _) = self.desired(path, mirrored, &manifest.key)?;
            if written.get(path) != Some(&desired) {
                self.apply(&manifest, ApplyAs::Master)?;
                break;
            }
        }
        Ok(())
    }

    /// This member's sync state.
    pub fn summary(&self, role: SyncRole) -> SyncSummary {
        let (manifest, applying, written, backups, pruned) = {
            let state = self.lock();
            let same_key = state.key.as_ref() == state.manifest.as_ref().map(|m| &m.key)
                && (role == SyncRole::Source || !state.issued_here);
            (
                state.manifest.clone(),
                state.applying.is_some(),
                if same_key {
                    state.written.clone()
                } else {
                    BTreeMap::new()
                },
                state.backups.iter().map(BackupView::from).collect(),
                state.pruned_backups,
            )
        };
        let paths = manifest
            .iter()
            .flat_map(|m| m.paths.iter().map(move |(p, t)| (m, p, t)))
            .map(|(manifest, path, mirrored)| {
                let mut state = PathState {
                    path: path.clone(),
                    files: mirrored.files().len(),
                    bytes: mirrored.bytes(),
                    overrides: self.overrides_of(path),
                    overridden: Vec::new(),
                    differences: Vec::new(),
                    skipped: Vec::new(),
                    error: None,
                };
                let compared = (|| {
                    let (desired, overridden) = self.desired(path, mirrored, &manifest.key)?;
                    let (_, live) = self.live(path, &manifest.key)?;
                    let mut differences =
                        compare::differences(&desired, &overridden, &live.tree, written.get(path));
                    if role == SyncRole::Source {
                        // The master's files are the source, not edits.
                        differences.retain(|d| overridden.contains(&d.key));
                    }
                    state.overridden = overridden
                        .iter()
                        .map(|k| report::file_name(path, k))
                        .collect();
                    state.differences = differences
                        .iter()
                        .filter(|d| d.change != Change::Overridden || d.live != d.desired)
                        .map(|d| FileReport::of(path, d))
                        .collect();
                    state.skipped = live.skipped;
                    Ok::<_, Failure>(())
                })();
                state.error = compared.err();
                state
            })
            .collect();
        SyncSummary {
            role,
            generation: manifest.as_ref().map(|m| m.generation),
            manifest_id: manifest.as_ref().map(|m| m.id().short()),
            issued_by: manifest.as_ref().map(|m| m.issued_by.clone()),
            applying,
            paths,
            backups,
            pruned_backups: pruned,
            paused: Vec::new(),
            error: None,
            service: self.service_summary(manifest.as_ref()),
        }
    }

    /// Records a push to `member`.
    pub fn record_push(&self, member: &MemberName, manifest: &Manifest, result: &Result<Applied>) {
        let _ = self.history.append(&Record {
            at: Timestamp::now(),
            event: if result.is_ok() {
                Event::Pushed
            } else {
                Event::PushFailed
            },
            generation: manifest.generation,
            manifest_id: manifest.id().short(),
            member: Some(member.clone()),
            paths: result
                .as_ref()
                .map(|a| a.paths.clone())
                .unwrap_or_default()
                .into_iter()
                .filter(|p: &PathRecord| !p.files.is_empty())
                .collect(),
            error: result.as_ref().err().map(|e| e.code.clone()),
        });
    }
}

fn retain_failed_backups(state: &mut State, failed: Vec<Backup>) {
    state.pruned_backups -= failed.len() as u64;
    state.backups.extend(failed);
}

/// Deletes backups pruned from the record: a file, or a whole kept tree, never
/// following links inside it.
fn remove_backups(removed: &[Backup], root: &Path) -> Vec<Backup> {
    let mut failed = Vec::new();
    for backup in removed {
        let internal = backup.location.strip_prefix(root).is_ok_and(|path| {
            path.components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
        });
        // A whole tree can be kept beside the live path when the state is on
        // another volume. This is an existing, recorded backup location.
        let fallback = backup.fingerprint.is_none()
            && backup
                .location
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|name| {
                    name.strip_prefix(&format!(
                        "{}.elsecall-kept-",
                        Path::new(backup.path.relative())
                            .file_name()
                            .expect("synced path")
                            .to_string_lossy()
                    ))
                })
                .is_some_and(|suffix| {
                    suffix.len() == 16 && suffix.bytes().all(|b| b.is_ascii_hexdigit())
                });
        if !internal && !fallback {
            failed.push(backup.clone());
            continue;
        }
        let Some(parent) = backup.location.parent() else {
            continue;
        };
        let Some(name) = backup.location.file_name() else {
            continue;
        };
        let result = (|| {
            let directory = Confined::open(parent)?;
            let name = std::ffi::CString::new(name.as_encoded_bytes()).expect("filesystem name");
            crate::workspace::confined::remove_at(directory.root(), &name)
        })();
        if result.is_err() && backup.location.try_exists().unwrap_or(true) {
            // Retry later instead of forgetting an undeleted secret backup.
            failed.push(backup.clone());
            continue;
        }
        // Timestamp/path scaffolding must not grow after content is pruned.
        let mut directory = parent;
        while directory.starts_with(root) && directory != root {
            if std::fs::remove_dir(directory).is_err() {
                break;
            }
            directory = directory.parent().expect("below backup root");
        }
    }
    failed
}

/// A crash can leave the only copy of local content before its backup record
/// is committed. Keep unregistered timestamp trees for seven days for recovery.
fn remove_orphan_backups(root: &Path, keep: &[Backup]) -> Result<()> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Failure::io("Inspect sync backups", e)),
    };
    let directory = Confined::open(root).map_err(|e| Failure::io("Open sync backups", e))?;
    let cutoff = Timestamp::now().millis().saturating_sub(7 * 86_400_000);
    for entry in entries {
        let entry = entry.map_err(|e| Failure::io("Inspect sync backup", e))?;
        let old = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u64>().ok())
            .is_some_and(|at| at < cutoff);
        if !old || keep.iter().any(|b| b.location.starts_with(entry.path())) {
            continue;
        }
        let name =
            std::ffi::CString::new(entry.file_name().as_encoded_bytes()).expect("filesystem name");
        crate::workspace::confined::remove_at(directory.root(), &name)
            .map_err(|e| Failure::io("Remove expired orphan sync backup", e))?;
    }
    Ok(())
}

fn changed_meanwhile() -> Failure {
    Failure::new(
        "sync_source_changed",
        "A synced file changed while it was read.",
        "Nothing was issued; the next scan picks the change up. Retry if you ran a command.",
    )
}
