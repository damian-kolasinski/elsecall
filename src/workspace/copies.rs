//! Which copies are in use on this member: one run at a time per copy, and nothing
//! removes a copy or its object store while a run holds it.
//!
//! A run claims its copy from the moment the member admits it until the run ends,
//! so a copy is never removed while a run uses it or one is about to attach.
//! Housekeeping reserves a copy before removing it and locks an object store before
//! removing or compacting it; while it does, runs of that copy, or of any copy of
//! that store, are refused as busy and wait or go elsewhere.
//!
//! Processes a run left running may still write in its copy after the run ended
//! and released it. The copy is free for the next run, which gets them with its
//! claim and waits for them or stops them before it syncs; housekeeping does not
//! reserve the copy while they run.
use crate::workspace::location::CopyLocation;
use futures_util::future::BoxFuture;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

/// Processes an earlier run left running in a copy.
pub trait Lingering: Send + Sync {
    /// Whether all of them have ended.
    fn ended(&self) -> bool;
    /// Waits up to `wait` for them to end, then stops them for `next_run`, and
    /// resolves once they have ended.
    fn settle(&self, wait: Duration, next_run: &str) -> BoxFuture<'static, ()>;
}

/// Where a run's use of a copy is noted, so housekeeping knows when each copy was
/// last used.
pub trait UseLog: Send + Sync {
    fn used(&self, location: &CopyLocation);
}

struct Forgetful;
impl UseLog for Forgetful {
    fn used(&self, _: &CopyLocation) {}
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Holder {
    Run,
    Housekeeping,
}

#[derive(Default)]
struct Held {
    /// Copies claimed by runs or reserved by housekeeping.
    copies: HashMap<PathBuf, Holder>,
    /// How many claimed or reserved copies use each object store.
    stores: HashMap<PathBuf, usize>,
    /// Stores housekeeping is removing or compacting.
    locked: HashSet<PathBuf>,
    /// What ended runs left running in their copies, until it ends.
    lingering: HashMap<PathBuf, Arc<dyn Lingering>>,
}

#[derive(Clone)]
pub struct Copies {
    held: Arc<Mutex<Held>>,
    log: Arc<dyn UseLog>,
}

impl Default for Copies {
    fn default() -> Self {
        Self::new(Arc::new(Forgetful))
    }
}

/// Exclusive use of one copy by a run, until dropped.
pub struct Claim {
    _hold: Hold,
    location: CopyLocation,
    log: Arc<dyn UseLog>,
    in_use: bool,
    /// What the previous run left running in the copy, when the claim was made.
    lingering: Option<Arc<dyn Lingering>>,
}

/// Exclusive hold of one copy by housekeeping, until dropped.
pub struct Reservation {
    _hold: Hold,
}

struct Hold {
    held: Arc<Mutex<Held>>,
    root: PathBuf,
    store: PathBuf,
}

/// Exclusive hold of one object store by housekeeping, until dropped.
pub struct StoreLock {
    held: Arc<Mutex<Held>>,
    store: PathBuf,
}

impl Copies {
    pub fn new(log: Arc<dyn UseLog>) -> Self {
        Self {
            held: Arc::default(),
            log,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn hold(&self, root: &Path, store: &Path, holder: Holder) -> Option<Hold> {
        let mut held = self.lock();
        held.lingering.retain(|_, lingering| !lingering.ended());
        let nests = |other: &Path| other.starts_with(root) || root.starts_with(other);
        // Same-path copies can nest, one workspace inside another's directory:
        // housekeeping never holds a copy while a run holds one nested in it or
        // around it, nor the other way round; nor while what a run left running
        // may still write in one.
        let conflicts = held.copies.iter().any(|(other, other_holder)| {
            other == root
                || ((holder == Holder::Housekeeping || *other_holder == Holder::Housekeeping)
                    && nests(other))
        }) || (holder == Holder::Housekeeping
            && held.lingering.keys().any(|other| nests(other)));
        if held.locked.contains(store) || conflicts {
            return None;
        }
        held.copies.insert(root.to_owned(), holder);
        *held.stores.entry(store.to_owned()).or_default() += 1;
        Some(Hold {
            held: self.held.clone(),
            root: root.to_owned(),
            store: store.to_owned(),
        })
    }

    /// Claims the copy at `location` for a run; `None` while another run or
    /// housekeeping holds it, or housekeeping holds its object store.
    pub fn claim(&self, location: &CopyLocation) -> Option<Claim> {
        let hold = self.hold(&location.root, &location.store, Holder::Run)?;
        let lingering = self.lock().lingering.get(&location.root).cloned();
        Some(Claim {
            _hold: hold,
            location: location.clone(),
            log: self.log.clone(),
            in_use: false,
            lingering,
        })
    }

    /// Reserves the copy at `root` for housekeeping; `None` while a run holds it.
    pub fn reserve(&self, root: &Path, store: &Path) -> Option<Reservation> {
        self.hold(root, store, Holder::Housekeeping)
            .map(|hold| Reservation { _hold: hold })
    }

    /// Locks `store` for housekeeping; `None` while any copy of it is held.
    pub fn lock_store(&self, store: &Path) -> Option<StoreLock> {
        let mut held = self.lock();
        if held.stores.get(store).is_some_and(|n| *n > 0) || !held.locked.insert(store.to_owned()) {
            return None;
        }
        Some(StoreLock {
            held: self.held.clone(),
            store: store.to_owned(),
        })
    }

    /// Whether a run or housekeeping holds the copy at `root`.
    pub fn is_held(&self, root: &Path) -> bool {
        self.lock().copies.contains_key(root)
    }
}

impl Claim {
    /// The member admitted the run: the copy counts as used now and again when the
    /// claim ends. A claim for an offer the member refused is never a use.
    pub fn begin_use(&mut self) {
        self.in_use = true;
        self.log.used(&self.location);
    }

    /// What the previous run left running in the copy, if it still runs: the run
    /// waits for it or stops it before it syncs the copy.
    pub fn lingering(&self) -> Option<Arc<dyn Lingering>> {
        self.lingering
            .clone()
            .filter(|lingering| !lingering.ended())
    }

    /// Ends the claim, leaving `lingering` in the copy for the next claim to find
    /// and keeping housekeeping out until it ends.
    pub fn release_with(self, lingering: Arc<dyn Lingering>) {
        self._hold
            .held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .lingering
            .insert(self.location.root.clone(), lingering);
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        // Fields drop after this body, so the hold is released only once the use
        // is noted: housekeeping never sees a released copy with its old last use.
        if self.in_use {
            self.log.used(&self.location);
        }
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        held.copies.remove(&self.root);
        if let Some(count) = held.stores.get_mut(&self.store) {
            *count -= 1;
            if *count == 0 {
                held.stores.remove(&self.store);
            }
        }
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        self.held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .locked
            .remove(&self.store);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::location::Setting;

    fn location(root: &str, store: &str) -> CopyLocation {
        CopyLocation {
            setting: Setting::Mapped,
            root: root.into(),
            store: store.into(),
            state_file: "/s".into(),
            calling_member: "alpha".into(),
            caller_root: "/w".into(),
        }
    }

    #[derive(Default)]
    struct Uses(Mutex<Vec<PathBuf>>);
    impl UseLog for Uses {
        fn used(&self, location: &CopyLocation) {
            self.0.lock().unwrap().push(location.root.clone());
        }
    }

    #[test]
    fn one_claim_per_copy() {
        let copies = Copies::default();
        let a = location("/w/a", "/s/1");
        let first = copies.claim(&a).unwrap();
        assert!(copies.claim(&a).is_none());
        assert!(copies.reserve(&a.root, &a.store).is_none());
        assert!(copies.claim(&location("/w/b", "/s/1")).is_some());
        drop(first);
        assert!(copies.claim(&a).is_some());
    }

    #[test]
    fn housekeeping_and_runs_exclude_each_other() {
        let copies = Copies::default();
        let a = location("/w/a", "/s/1");
        let reserved = copies.reserve(&a.root, &a.store).unwrap();
        assert!(copies.claim(&a).is_none(), "a reserved copy is busy");
        assert!(copies.is_held(&a.root));
        assert!(
            copies.lock_store(&a.store).is_none(),
            "a held copy keeps its store"
        );
        drop(reserved);
        let lock = copies.lock_store(&a.store).unwrap();
        assert!(copies.lock_store(&a.store).is_none());
        assert!(
            copies.claim(&location("/w/c", "/s/1")).is_none(),
            "no copy of a locked store can be claimed"
        );
        assert!(copies.claim(&location("/w/d", "/s/2")).is_some());
        drop(lock);
        assert!(copies.claim(&location("/w/c", "/s/1")).is_some());
    }

    #[test]
    fn nested_copies_exclude_housekeeping_but_not_each_other() {
        let copies = Copies::default();
        let outer = location("/u/app", "/s/1");
        let inner = location("/u/app/.worktrees/x", "/s/1");
        let running = copies.claim(&inner).unwrap();
        assert!(copies.claim(&outer).is_some(), "two runs may nest");
        assert!(
            copies.reserve(&outer.root, &outer.store).is_none(),
            "the outer copy holds a running one"
        );
        drop(running);
        let reserved = copies.reserve(&outer.root, &outer.store).unwrap();
        assert!(
            copies.claim(&inner).is_none(),
            "a copy inside one being removed"
        );
        drop(reserved);
        let reserved = copies.reserve(&inner.root, &inner.store).unwrap();
        assert!(copies.claim(&outer).is_none());
        drop(reserved);
    }

    struct Left(std::sync::atomic::AtomicBool);
    impl Lingering for Left {
        fn ended(&self) -> bool {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn settle(&self, _: Duration, _: &str) -> BoxFuture<'static, ()> {
            Box::pin(async {})
        }
    }

    #[test]
    fn leftovers_free_the_copy_for_runs_but_keep_housekeeping_out() {
        let copies = Copies::default();
        let a = location("/w/a", "/s/1");
        let left = Arc::new(Left(Default::default()));
        copies.claim(&a).unwrap().release_with(left.clone());
        assert!(!copies.is_held(&a.root), "free for placement");
        assert!(copies.reserve(&a.root, &a.store).is_none());
        assert!(
            copies.reserve("/w".as_ref(), &a.store).is_none(),
            "around it"
        );
        let next = copies.claim(&a).unwrap();
        assert!(next.lingering().is_some(), "the next run gets them");
        drop(next);
        assert!(
            copies.claim(&a).unwrap().lingering().is_some(),
            "a refused offer leaves them for the next claim"
        );
        left.0.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(copies.claim(&a).unwrap().lingering().is_none());
        assert!(
            copies.reserve(&a.root, &a.store).is_some(),
            "once they ended"
        );
    }

    #[test]
    fn only_admitted_runs_count_as_uses() {
        let uses = Arc::new(Uses::default());
        let copies = Copies::new(uses.clone());
        let a = location("/w/a", "/s/1");
        drop(copies.claim(&a).unwrap());
        assert!(uses.0.lock().unwrap().is_empty(), "a refused offer");
        let mut claim = copies.claim(&a).unwrap();
        claim.begin_use();
        drop(claim);
        assert_eq!(uses.0.lock().unwrap().len(), 2, "admitted, then ended");
    }
}
