//! One housekeeping pass over this member's copies, and the listing of them.
//!
//! A pass finishes deletions an earlier pass started, adopts copies it does not
//! track yet, measures copies used since they were last measured, and asks each
//! copy's calling member whether its workspace still exists. It then removes copies
//! the policy condemns, evicts idle copies while a volume is under pressure,
//! removes object stores no copy uses and compacts the others when that is cheap.
//! Finally it tells each calling member which of its copies went, and tidies this
//! member's own caller-side sync state.
//! Every removal holds the copy, or the store, exclusively, so it never races a run
//! or a sync; a copy used between the judgement and its removal is judged again
//! next pass.
use crate::{
    failure::{Failure, Result},
    housekeeping::{
        adoption,
        caller_state::CallerState,
        callers::Callers,
        catalog::{Catalog, Entry, Measured, Reason, Removal, Removed},
        exchange::{Question, RemovedCopy as Notice, WorkspaceState},
        measure,
        policy::{self, CallerSays, Observed, Verdict},
        report::{
            CopyState, CopyView, DiskView, HousekeepingView, KeptStore, Listing, OwnerAsked,
            PassSummary, Pressure, Relief, RemovedCopy, Report, SparedCopy, StoreChange, StoreView,
            Summary,
        },
        settings::Settings,
        stores::{self, Store},
        trash::{Moved, Trash},
        volumes::Volumes,
    },
    workspace::{copies::Copies, copy, location::Locations},
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

/// What `elsecall copies` lists as recent removals.
const LISTED_REMOVALS: u32 = 50;

pub struct Housekeeper {
    catalog: Arc<Catalog>,
    copies: Copies,
    locations: Locations,
    volumes: Arc<dyn Volumes>,
    trash: Trash,
    settings: Settings,
    member: Box<dyn Fn() -> String + Send + Sync>,
    callers: Arc<dyn Callers>,
    /// This member's own sync state as a caller.
    caller_state: Arc<CallerState>,
    /// One pass at a time.
    serial: tokio::sync::Mutex<()>,
    last_pass: Mutex<Option<PassSummary>>,
}

/// What the calling members said, by calling member and caller path.
type Answers = HashMap<(String, String), CallerSays>;

/// What a pass has decided so far; a dry run decides without acting.
struct Pass {
    dry_run: bool,
    report: Report,
    /// Copies removed, or planned for removal in a dry run.
    removed: HashSet<PathBuf>,
}

fn log(event: &str, detail: impl serde::Serialize) {
    crate::service_log!("{}", serde_json::json!({"event": event, "detail": detail}));
}

/// Whether `directory` exists and is not a mount point, so what is missing from it
/// is missing and not on a volume that is away.
fn on_its_parents_volume(directory: &Path) -> bool {
    let (Ok(meta), Some(parent)) = (std::fs::metadata(directory), directory.parent()) else {
        return false;
    };
    std::fs::metadata(parent).is_ok_and(|parent| parent.dev() == meta.dev())
}

/// The device of the nearest existing ancestor of `path`, so a volume is known
/// even before its first copy.
fn device(path: &Path) -> Option<(u64, PathBuf)> {
    path.ancestors().find_map(|ancestor| {
        std::fs::metadata(ancestor)
            .ok()
            .map(|meta| (meta.dev(), ancestor.to_owned()))
    })
}

/// The parts a housekeeper works with.
pub struct Parts {
    pub catalog: Arc<Catalog>,
    pub copies: Copies,
    pub locations: Locations,
    pub volumes: Arc<dyn Volumes>,
    pub settings: Settings,
    /// This member's name, for listings.
    pub member: Box<dyn Fn() -> String + Send + Sync>,
    pub callers: Arc<dyn Callers>,
    pub caller_state: Arc<CallerState>,
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(work)
        .await
        .expect("housekeeping does not panic")
}

impl Housekeeper {
    pub fn new(parts: Parts) -> Self {
        Self {
            trash: Trash::new(parts.catalog.clone(), &parts.locations.root),
            catalog: parts.catalog,
            copies: parts.copies,
            locations: parts.locations,
            volumes: parts.volumes,
            settings: parts.settings,
            member: parts.member,
            callers: parts.callers,
            caller_state: parts.caller_state,
            serial: tokio::sync::Mutex::new(()),
            last_pass: Mutex::new(None),
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn forget_rehearsal_callers(&self, session: &str) -> Result<()> {
        self.caller_state.forget_rehearsal(session)
    }

    /// Remove only this synthetic session's copies and now-unreferenced stores.
    /// Uses the same identity checks, reservations and confined trash as a pass.
    pub async fn clean_rehearsal(self: &Arc<Self>, session: String) -> Result<()> {
        if !crate::rehearsal::protocol::valid_session(&session) {
            return Err(crate::rehearsal::invalid(
                "Invalid rehearsal cleanup identity.",
            ));
        }
        let _serial = self.serial.lock().await;
        let this = self.clone();
        blocking(move || {
            let mut pass = Pass {
                dry_run: false,
                report: Report::new(false, this.catalog.now()),
                removed: HashSet::new(),
            };
            let mut stores = BTreeMap::new();
            for entry in this.catalog.entries()? {
                if crate::rehearsal::protocol::scratch_of(&session, &entry.location.caller_root) {
                    stores.insert(
                        entry.location.store.clone(),
                        entry.location.calling_member.clone(),
                    );
                    if !this.remove(&entry, Reason::CallerDeleted, &mut pass)
                        && pass.report.problems.is_empty()
                    {
                        return Err(crate::rehearsal::invalid(
                            "A synthetic copy is still in use; cleanup must be retried.",
                        ));
                    }
                }
            }
            for removed in this.catalog.rehearsal_removals(&session)? {
                if let Removed::Copy {
                    calling_member,
                    caller_path,
                    ..
                } = removed.removed
                    && crate::rehearsal::protocol::scratch_of(&session, &caller_path)
                {
                    let location = this.locations.locate(
                        &calling_member,
                        &caller_path,
                        &format!("{caller_path}/.git"),
                    )?;
                    stores.insert(location.store, calling_member);
                }
            }
            for (path, calling_member) in stores {
                // Removal history survives successful cleanup. A watchdog and
                // the controller can both clean the same session; do not ask
                // Git about a store an earlier pass already removed.
                match std::fs::symlink_metadata(&path) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(Failure::io("Inspect rehearsal object store", error));
                    }
                    Ok(_) => (),
                }
                this.remove_store_if_unused(
                    &Store {
                        path: path.clone(),
                        calling_member,
                    },
                    &mut pass,
                );
                if path.exists() && pass.report.problems.is_empty() {
                    return Err(crate::rehearsal::invalid(
                        "A synthetic object store is still in use; cleanup must be retried.",
                    ));
                }
            }
            if let Some(error) = pass.report.problems.into_iter().next() {
                return Err(error);
            }
            this.trash.recover().into_iter().next().map_or(Ok(()), Err)
        })
        .await
    }

    /// Runs a pass now, after any pass already running.
    pub async fn pass(self: &Arc<Self>, dry_run: bool) -> Report {
        let _serial = self.serial.lock().await;
        let clock = Instant::now();
        let this = self.clone();
        let (pass, live) = blocking(move || {
            let mut pass = Pass {
                dry_run,
                report: Report::new(dry_run, this.catalog.now()),
                removed: HashSet::new(),
            };
            if !dry_run {
                pass.report.problems.extend(this.trash.recover());
            }
            this.adopt(&mut pass);
            let live = match this.catalog.entries() {
                Ok(entries) => this.reconcile(entries, &mut pass),
                Err(error) => {
                    pass.report.problems.push(error);
                    Vec::new()
                }
            };
            (pass, live)
        })
        .await;
        let (mut pass, answers) = self.ask(pass, &live).await;
        let this = self.clone();
        pass = blocking(move || {
            let remaining = this.judge(live, &answers, &mut pass);
            this.relieve(remaining, &mut pass);
            this.tidy_stores(&mut pass);
            pass
        })
        .await;
        if !dry_run {
            pass = self.tell(pass).await;
        }
        let (this, members) = (self.clone(), self.callers.members());
        pass = blocking(move || {
            pass.report.caller_state =
                this.caller_state
                    .tidy(members.as_deref(), this.settings.idle_after, dry_run);
            pass
        })
        .await;
        pass.report.duration_ms = clock.elapsed().as_millis() as u64;
        let summary = pass.report.summary();
        if !dry_run {
            *self.last_pass.lock().unwrap_or_else(|e| e.into_inner()) = Some(summary.clone());
        }
        log("housekeeping_pass", &summary);
        pass.report
    }

    /// Asks each calling member about the workspaces of its copies here. Only the
    /// member that owns a copy is asked about it, and only its answer counts.
    async fn ask(&self, mut pass: Pass, live: &[Entry]) -> (Pass, Answers) {
        let mut answers = Answers::new();
        if !self.settings.remove_deleted_workspaces {
            return (pass, answers);
        }
        let mut held: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for entry in live {
            let paths = held.entry(&entry.location.calling_member).or_default();
            if !paths.contains(&entry.location.caller_root) {
                paths.push(entry.location.caller_root.clone());
            }
        }
        // Owners are asked at once, so unreachable ones do not add up.
        let dry_run = pass.dry_run;
        let asking = held.into_iter().map(|(member, paths)| async move {
            let mut owner = OwnerAsked {
                member: member.to_owned(),
                asked: paths.len(),
                gone: 0,
                error: None,
            };
            let mut said = Vec::new();
            for question in Question::batches(dry_run, paths, Vec::new()) {
                // A use after this moment speaks against the answer.
                let asked_at = self.catalog.now();
                let asked: HashSet<String> = question.held.iter().cloned().collect();
                match self.callers.ask(member, question).await {
                    Ok(answer) => {
                        for workspace in answer.workspaces {
                            // Only what this owner was asked counts.
                            if !asked.contains(&workspace.path) {
                                continue;
                            }
                            let says = match workspace.state {
                                WorkspaceState::Present => CallerSays::Present,
                                WorkspaceState::Gone => {
                                    owner.gone += 1;
                                    CallerSays::Gone { asked_at }
                                }
                                WorkspaceState::Unknown => CallerSays::Unknown,
                            };
                            said.push(((member.to_owned(), workspace.path), says));
                        }
                    }
                    // The other batches are still asked.
                    Err(error) => owner.error = Some(error),
                }
            }
            (owner, said)
        });
        for (owner, said) in futures_util::future::join_all(asking).await {
            answers.extend(said);
            pass.report.owners.push(owner);
        }
        (pass, answers)
    }

    /// Tells each calling member which of its copies were removed, so it drops
    /// what it pushed to them. A member not reached is told next pass.
    async fn tell(&self, mut pass: Pass) -> Pass {
        let pending = match self.catalog.unnotified() {
            Ok(pending) => pending,
            Err(error) => {
                pass.report.problems.push(error);
                return pass;
            }
        };
        let members = self.callers.members();
        // Owners this pass could not ask are told next pass.
        let unreachable: HashSet<String> = pass
            .report
            .owners
            .iter()
            .filter(|o| o.error.is_some())
            .map(|o| o.member.clone())
            .collect();
        for (member, removed) in pending {
            if members.as_ref().is_some_and(|m| !m.contains(&member))
                || unreachable.contains(&member)
            {
                continue;
            }
            let until = removed.iter().map(|r| r.id).max();
            let notices = removed
                .into_iter()
                .map(|r| Notice {
                    path: r.path,
                    removed_at: r.removed_at,
                })
                .collect();
            let mut told = true;
            for question in Question::batches(false, Vec::new(), notices) {
                if let Err(error) = self.callers.ask(&member, question).await {
                    pass.report.problems.push(error);
                    told = false;
                    break;
                }
            }
            if let (true, Some(until)) = (told, until)
                && let Err(error) = self.catalog.notified(&member, until)
            {
                pass.report.problems.push(error);
            }
        }
        pass
    }

    fn adopt(&self, pass: &mut Pass) {
        for (location, last_written) in adoption::discover(&self.locations) {
            match self.catalog.adopt(&location, last_written) {
                Ok(true) => pass.report.adopted += 1,
                Ok(false) => (),
                Err(error) => pass.report.problems.push(error),
            }
        }
    }

    /// Forgets copies whose directory is gone and measures copies used since they
    /// were last measured. Returns the copies still on disk.
    fn reconcile(&self, entries: Vec<Entry>, pass: &mut Pass) -> Vec<Entry> {
        let mut live = Vec::new();
        for mut entry in entries {
            let root = &entry.location.root;
            let held = self.copies.is_held(root);
            match std::fs::symlink_metadata(root) {
                // Only a copy whose parent is still there is gone: a missing
                // parent may be an unmounted volume.
                Err(e)
                    if e.kind() == io::ErrorKind::NotFound
                        && !held
                        && root.parent().is_some_and(on_its_parents_volume) =>
                {
                    self.remove(&entry, Reason::Missing, pass);
                    continue;
                }
                _ => (),
            }
            let stale = entry.size.is_none_or(|size| size.at < entry.last_used);
            if stale && !held {
                match measure::allocated(root) {
                    Ok(bytes) => {
                        let measured = Measured {
                            bytes,
                            at: self.catalog.now(),
                        };
                        if let Err(error) = self.catalog.set_size(root, measured) {
                            pass.report.problems.push(error);
                        }
                        entry.size = Some(measured);
                    }
                    Err(e) => pass
                        .report
                        .problems
                        .push(Failure::io(&format!("Measuring {}", root.display()), e)),
                }
            }
            live.push(entry);
        }
        live
    }

    fn observe(&self, entry: &Entry, caller: CallerSays) -> Observed {
        Observed {
            root: entry.location.root.clone(),
            last_used: entry.last_used,
            held: self.copies.is_held(&entry.location.root),
            caller,
        }
    }

    /// Removes what the policy condemns; returns the copies that stay.
    fn judge(&self, live: Vec<Entry>, answers: &Answers, pass: &mut Pass) -> Vec<Entry> {
        let now = self.catalog.now();
        let mut remaining = Vec::new();
        for entry in live {
            let caller = answers
                .get(&(
                    entry.location.calling_member.clone(),
                    entry.location.caller_root.clone(),
                ))
                .copied()
                .unwrap_or(CallerSays::Unknown);
            match policy::judge(&self.observe(&entry, caller), now, &self.settings) {
                Verdict::Keep => remaining.push(entry),
                Verdict::Remove(reason) => {
                    if !self.remove(&entry, reason, pass) {
                        remaining.push(entry);
                    }
                }
                Verdict::Spare(reason, spared) => {
                    pass.report.spared.push(SparedCopy {
                        location: entry.location.root.clone(),
                        reason,
                        spared,
                    });
                    remaining.push(entry);
                }
            }
        }
        remaining
    }

    /// Removes one copy for `reason`; returns whether it is gone, or would be in a
    /// dry run.
    fn remove(&self, entry: &Entry, reason: Reason, pass: &mut Pass) -> bool {
        let location = &entry.location;
        let removed = RemovedCopy {
            calling_member: location.calling_member.clone(),
            caller_path: location.caller_root.clone(),
            location: location.root.clone(),
            reason,
            size_bytes: entry.size.map(|s| s.bytes),
            last_used_at: entry.last_used,
        };
        if pass.dry_run {
            pass.removed.insert(location.root.clone());
            pass.report.removed.push(removed);
            return true;
        }
        let spare = |pass: &mut Pass| {
            pass.report.spared.push(SparedCopy {
                location: location.root.clone(),
                reason,
                spared: policy::Spared::InUse,
            })
        };
        // A same-path copy can hold another copy inside its directory; removing it
        // would remove that one too.
        match self.catalog.entries() {
            Ok(entries)
                if entries.iter().any(|other| {
                    other.location.root != location.root
                        && other.location.root.starts_with(&location.root)
                }) =>
            {
                pass.report.spared.push(SparedCopy {
                    location: location.root.clone(),
                    reason,
                    spared: policy::Spared::ContainsCopy,
                });
                return false;
            }
            Ok(_) => (),
            Err(error) => {
                pass.report.problems.push(error);
                return false;
            }
        }
        let Some(reservation) = self.copies.reserve(&location.root, &location.store) else {
            spare(pass);
            return false;
        };
        // A run that used the copy after it was judged released it before the
        // reservation; it is judged again next pass.
        match self.catalog.last_used(&location.root) {
            Ok(Some(last_used)) if last_used == entry.last_used => (),
            Ok(_) => return false,
            Err(error) => {
                pass.report.problems.push(error);
                return false;
            }
        }
        let aside = match self
            .trash
            .move_aside(&location.root, |moved| copy::is_copy(moved, location))
        {
            Ok(Moved::Aside(aside)) => Some(aside),
            Ok(Moved::Missing) => None,
            Err(error) => {
                if error.code == "housekeeping_refused"
                    && let Err(error) = self.catalog.forget(&location.root)
                {
                    pass.report.problems.push(error);
                }
                pass.report.problems.push(error);
                return false;
            }
        };
        // Elsecall's own state of the copy goes with it.
        match std::fs::remove_file(&location.state_file) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => pass
                .report
                .problems
                .push(Failure::io("Removing a copy's record", e)),
            _ => (),
        }
        // A copy found missing may be on a volume that comes back, still reading
        // its store: its references stay, and so does the store.
        if aside.is_some()
            && location.store.exists()
            && let Err(error) = copy::release_store(&location.store, &location.root)
        {
            pass.report.problems.push(error);
        }
        // A copy already gone when it was to be removed is recorded as such.
        let reason = if aside.is_some() {
            reason
        } else {
            Reason::Missing
        };
        let recorded = self.catalog.forget(&location.root).and_then(|_| {
            self.catalog.record_removal(&Removal {
                removed_at: self.catalog.now(),
                removed: Removed::Copy {
                    calling_member: location.calling_member.clone(),
                    caller_path: location.caller_root.clone(),
                    location: location.root.clone(),
                    setting: location.setting,
                },
                reason,
                size_bytes: removed.size_bytes,
                last_used_at: Some(entry.last_used),
            })
        });
        if let Err(error) = recorded {
            pass.report.problems.push(error);
        }
        drop(reservation);
        if let Some(aside) = aside
            && let Err(error) = self.trash.empty(&aside)
        {
            pass.report.problems.push(error);
        }
        pass.removed.insert(location.root.clone());
        pass.report.removed.push(RemovedCopy { reason, ..removed });
        true
    }

    /// Evicts idle copies, least recently used first, from each volume whose free
    /// space is below the threshold, until it recovers or what was evicted should
    /// have been enough.
    fn relieve(&self, remaining: Vec<Entry>, pass: &mut Pass) {
        let mut volumes: BTreeMap<u64, (PathBuf, Vec<Entry>)> = BTreeMap::new();
        if let Some((device, path)) = device(&self.locations.root) {
            volumes.entry(device).or_insert((path, Vec::new()));
        }
        for entry in remaining {
            if let Some((device, path)) = device(&entry.location.root) {
                volumes
                    .entry(device)
                    .or_insert((path, Vec::new()))
                    .1
                    .push(entry);
            }
        }
        for (path, entries) in volumes.into_values() {
            self.relieve_volume(&path, entries, pass);
        }
    }

    fn relieve_volume(&self, path: &Path, entries: Vec<Entry>, pass: &mut Pass) {
        let space = match self.volumes.space(path) {
            Ok(space) => space,
            Err(e) => {
                pass.report.problems.push(Failure::io(
                    &format!("Reading free space of {}", path.display()),
                    e,
                ));
                return;
            }
        };
        let threshold = self.settings.threshold(space.total_bytes);
        if space.available_bytes >= threshold {
            return;
        }
        let deficit = threshold - space.available_bytes;
        let observed: Vec<Observed> = entries
            .iter()
            .map(|e| self.observe(e, CallerSays::Unknown))
            .collect();
        let order = policy::eviction_order(&observed);
        let mut available = space.available_bytes;
        let mut pressure = Pressure {
            path: path.to_owned(),
            total_bytes: space.total_bytes,
            threshold_bytes: threshold,
            available_before_bytes: space.available_bytes,
            available_after_bytes: available,
            evictable_bytes: self.evictable(&entries, &order, pass),
            evicted: 0,
            outcome: Relief::Unrecoverable,
        };
        // Warm copies are evicted only when that can bring the volume back above
        // its threshold; when even all of them could not, they stay.
        if pressure.evictable_bytes < deficit {
            log("disk_pressure_unrelieved", &pressure);
            pass.report.disk.push(pressure);
            return;
        }
        let mut freed = 0u64;
        let mut evicted = 0;
        for root in order {
            // A real pass stops when the volume shows it recovered; sizes count
            // clones in full and may promise more than removal frees. A dry run
            // has only the estimate.
            if available >= threshold || (pass.dry_run && freed >= deficit) {
                break;
            }
            let entry = entries
                .iter()
                .find(|e| e.location.root == root)
                .expect("ordered from these entries");
            if !self.remove(entry, Reason::DiskPressure, pass) {
                continue;
            }
            evicted += 1;
            freed += entry.size.map_or(0, |s| s.bytes);
            if let Some(store) = stores::list(&self.locations.root)
                .into_iter()
                .find(|s| s.path == entry.location.store)
            {
                freed += self.remove_store_if_unused(&store, pass);
            }
            available = if pass.dry_run {
                space.available_bytes.saturating_add(freed)
            } else {
                self.volumes
                    .space(path)
                    .map_or(available, |space| space.available_bytes)
            };
        }
        pressure.outcome = if available >= threshold {
            Relief::Recovered
        } else if freed >= deficit {
            Relief::Pending
        } else {
            Relief::Unrecoverable
        };
        pressure.available_after_bytes = available;
        pressure.evicted = evicted;
        if pressure.outcome == Relief::Unrecoverable {
            log("disk_pressure_unrelieved", &pressure);
        }
        pass.report.disk.push(pressure);
    }

    /// What evicting every copy in `idle` would free: the copies, and the stores
    /// whose every copy is among them.
    fn evictable(&self, entries: &[Entry], idle: &[PathBuf], pass: &mut Pass) -> u64 {
        let idle: HashSet<&PathBuf> = idle.iter().collect();
        let evicted: Vec<&Entry> = entries
            .iter()
            .filter(|e| idle.contains(&e.location.root))
            .collect();
        let mut bytes: u64 = evicted.iter().filter_map(|e| e.size).map(|s| s.bytes).sum();
        let all = match self.catalog.entries() {
            Ok(all) => all,
            Err(error) => {
                pass.report.problems.push(error);
                return bytes;
            }
        };
        let stores: HashSet<&PathBuf> = evicted.iter().map(|e| &e.location.store).collect();
        for store in stores {
            let freed_with_them = all
                .iter()
                .filter(|e| &e.location.store == store && !pass.removed.contains(&e.location.root))
                .all(|e| idle.contains(&e.location.root));
            if freed_with_them {
                bytes += match self.catalog.store_size(store).ok().flatten() {
                    Some(measured) => measured.bytes,
                    None => measure::allocated(store).unwrap_or(0),
                };
            }
        }
        bytes
    }

    /// Copies the catalog says use `store`, less those this pass removed.
    fn users(&self, store: &Path, pass: &Pass) -> Result<Vec<PathBuf>> {
        Ok(self
            .catalog
            .entries()?
            .into_iter()
            .filter(|e| e.location.store == store && !pass.removed.contains(&e.location.root))
            .map(|e| e.location.root)
            .collect())
    }

    /// References in the store for copies the member does not track.
    fn unknown_references(&self, store: &Path, users: &[PathBuf], pass: &Pass) -> Result<usize> {
        let known: HashSet<String> = users
            .iter()
            .chain(pass.removed.iter())
            .map(|root| copy::copy_id(root))
            .collect();
        Ok(copy::referencing_copies(store)?
            .into_iter()
            .filter(|id| !known.contains(id))
            .count())
    }

    /// Removes `store` if no copy uses it; returns the bytes it held.
    fn remove_store_if_unused(&self, store: &Store, pass: &mut Pass) -> u64 {
        let checked = (|| -> Result<Option<usize>> {
            let users = self.users(&store.path, pass)?;
            if !users.is_empty() {
                return Ok(None);
            }
            Ok(Some(self.unknown_references(&store.path, &users, pass)?))
        })();
        match checked {
            Ok(None) => 0,
            Ok(Some(0)) => self.remove_store(store, pass),
            Ok(Some(unknown_copies)) => {
                pass.report.stores_kept.push(KeptStore {
                    path: store.path.clone(),
                    unknown_copies,
                });
                0
            }
            Err(error) => {
                pass.report.problems.push(error);
                0
            }
        }
    }

    fn remove_store(&self, store: &Store, pass: &mut Pass) -> u64 {
        let size = measure::allocated(&store.path).ok();
        let change = StoreChange {
            path: store.path.clone(),
            size_bytes: size,
            packs_before: None,
            size_after_bytes: None,
        };
        if pass.dry_run {
            if let Some(bytes) = size {
                let measured = Measured {
                    bytes,
                    at: self.catalog.now(),
                };
                if let Err(error) = self.catalog.set_store_size(&store.path, measured) {
                    pass.report.problems.push(error);
                }
            }
            pass.report.stores_removed.push(change);
            return size.unwrap_or(0);
        }
        let Some(lock) = self.copies.lock_store(&store.path) else {
            return 0;
        };
        // Under the lock no copy of the store is in use: what the catalog and the
        // store's own references say now is final.
        let unused = self.users(&store.path, pass).and_then(|users| {
            Ok(users.is_empty() && self.unknown_references(&store.path, &users, pass)? == 0)
        });
        match unused {
            Ok(true) => (),
            Ok(false) => return 0,
            Err(error) => {
                pass.report.problems.push(error);
                return 0;
            }
        }
        if !stores::is_store_of(&self.locations.root, &store.calling_member, &store.path) {
            return 0;
        }
        let aside = match self.trash.move_aside(&store.path, stores::is_repository) {
            Ok(Moved::Aside(aside)) => aside,
            Ok(Moved::Missing) => return 0,
            Err(error) => {
                pass.report.problems.push(error);
                return 0;
            }
        };
        let recorded = self.catalog.forget_store(&store.path).and_then(|_| {
            self.catalog.record_removal(&Removal {
                removed_at: self.catalog.now(),
                removed: Removed::Store {
                    calling_member: store.calling_member.clone(),
                    path: store.path.clone(),
                },
                reason: Reason::Unreferenced,
                size_bytes: size,
                last_used_at: None,
            })
        });
        if let Err(error) = recorded {
            pass.report.problems.push(error);
        }
        drop(lock);
        if let Err(error) = self.trash.empty(&aside) {
            pass.report.problems.push(error);
        }
        pass.report.stores_removed.push(change);
        size.unwrap_or(0)
    }

    /// Removes stores no copy uses, and measures and compacts the others.
    fn tidy_stores(&self, pass: &mut Pass) {
        let decided = |pass: &Pass, path: &Path| {
            pass.report.stores_removed.iter().any(|s| s.path == path)
                || pass.report.stores_kept.iter().any(|s| s.path == path)
        };
        for store in stores::list(&self.locations.root) {
            if decided(pass, &store.path) {
                continue;
            }
            self.remove_store_if_unused(&store, pass);
            if decided(pass, &store.path) {
                continue;
            }
            self.measure_store(&store.path, pass);
            self.compact(&store.path, pass);
        }
    }

    fn measure_store(&self, store: &Path, pass: &mut Pass) {
        let measured = self.catalog.store_size(store).ok().flatten();
        let used_since = self.catalog.entries().ok().is_none_or(|entries| {
            entries
                .iter()
                .any(|e| e.location.store == store && measured.is_none_or(|m| m.at < e.last_used))
        });
        if measured.is_some() && !used_since {
            return;
        }
        match measure::allocated(store) {
            Ok(bytes) => {
                let measured = Measured {
                    bytes,
                    at: self.catalog.now(),
                };
                if let Err(error) = self.catalog.set_store_size(store, measured) {
                    pass.report.problems.push(error);
                }
            }
            Err(e) => pass
                .report
                .problems
                .push(Failure::io(&format!("Measuring {}", store.display()), e)),
        }
    }

    /// Repacks a store with many packs, while no copy of it is in use.
    fn compact(&self, store: &Path, pass: &mut Pass) {
        let threshold = self.settings.compact_store_packs as usize;
        let packs = stores::packs(store);
        if threshold == 0 || packs < threshold {
            return;
        }
        let before = measure::allocated(store).ok();
        let mut change = StoreChange {
            path: store.to_owned(),
            size_bytes: before,
            packs_before: Some(packs),
            size_after_bytes: None,
        };
        if pass.dry_run {
            pass.report.stores_compacted.push(change);
            return;
        }
        let Some(_lock) = self.copies.lock_store(store) else {
            return;
        };
        if let Err(error) = stores::compact(store) {
            pass.report.problems.push(error);
            return;
        }
        let after = measure::allocated(store).ok();
        if let Some(bytes) = after {
            let measured = Measured {
                bytes,
                at: self.catalog.now(),
            };
            if let Err(error) = self.catalog.set_store_size(store, measured) {
                pass.report.problems.push(error);
            }
        }
        change.size_after_bytes = after;
        pass.report.stores_compacted.push(change);
    }

    /// This member's copies and stores as last measured; quick, measures nothing.
    pub fn listing(&self) -> Result<Listing> {
        let entries = self.catalog.entries()?;
        let copies: Vec<CopyView> = entries
            .iter()
            .map(|entry| CopyView {
                calling_member: entry.location.calling_member.clone(),
                caller_path: entry.location.caller_root.clone(),
                location: entry.location.root.clone(),
                setting: entry.location.setting,
                state: if self.copies.is_held(&entry.location.root) {
                    CopyState::InUse
                } else {
                    CopyState::Idle
                },
                last_used_at: entry.last_used,
                idle_expires_at: entry.last_used.after(self.settings.idle_after),
                size_bytes: entry.size.map(|s| s.bytes),
                measured_at: entry.size.map(|s| s.at),
                store: entry.location.store.clone(),
            })
            .collect();
        let mut stores_view = Vec::new();
        for store in stores::list(&self.locations.root) {
            let measured = self.catalog.store_size(&store.path)?;
            stores_view.push(StoreView {
                copies: entries
                    .iter()
                    .filter(|e| e.location.store == store.path)
                    .count(),
                calling_member: store.calling_member,
                path: store.path,
                size_bytes: measured.map(|m| m.bytes),
                measured_at: measured.map(|m| m.at),
            });
        }
        let total_bytes = copies
            .iter()
            .filter_map(|c| c.size_bytes)
            .chain(stores_view.iter().filter_map(|s| s.size_bytes))
            .sum();
        let (disk, disk_error) = match self.disk() {
            Ok(disk) => (Some(disk), None),
            Err(error) => (None, Some(error)),
        };
        Ok(Listing {
            member: (self.member)(),
            workspace_root: self.locations.root.clone(),
            copies,
            stores: stores_view,
            total_bytes,
            disk,
            disk_error,
            housekeeping: HousekeepingView {
                settings: self.settings.clone(),
                last_pass: self
                    .last_pass
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            },
            removals: self.catalog.removals(LISTED_REMOVALS)?,
        })
    }

    /// Counts and bytes of copies and stores as last measured, and the workspace
    /// volume's space: a quick summary for other members.
    pub fn summary(&self) -> Result<Summary> {
        let entries = self.catalog.entries()?;
        let stores = stores::list(&self.locations.root);
        let mut stores_bytes = 0;
        for store in &stores {
            stores_bytes += self.catalog.store_size(&store.path)?.map_or(0, |m| m.bytes);
        }
        Ok(Summary {
            copies: entries.len(),
            copies_bytes: entries.iter().filter_map(|e| e.size).map(|s| s.bytes).sum(),
            unmeasured_copies: entries.iter().filter(|e| e.size.is_none()).count(),
            stores: stores.len(),
            stores_bytes,
            disk: self.disk().ok(),
            last_pass_at: self
                .last_pass
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|p| p.started_at),
        })
    }

    /// The volume of the workspace root.
    fn disk(&self) -> Result<DiskView> {
        let (_, path) = device(&self.locations.root).ok_or_else(|| {
            Failure::new(
                "housekeeping_failed",
                "The workspace root's volume could not be found.",
                "Check [workspace] root in the service configuration.",
            )
        })?;
        let space = self
            .volumes
            .space(&path)
            .map_err(|e| Failure::io("Read free space", e))?;
        let threshold = self.settings.threshold(space.total_bytes);
        Ok(DiskView {
            path: self.locations.root.clone(),
            total_bytes: space.total_bytes,
            available_bytes: space.available_bytes,
            threshold_bytes: threshold,
            pressure: space.available_bytes < threshold,
        })
    }
}
