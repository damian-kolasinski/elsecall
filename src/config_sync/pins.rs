//! Content an exchange in progress still reads from the master's store. Issuing a
//! generation drops content no longer needed, but a push of an earlier generation
//! that started before it must still be able to send every file it offered, so
//! each push pins its manifest's content until it ends.
use crate::config_sync::fingerprint::Fingerprint;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

#[derive(Default)]
pub struct Pins {
    /// How many pins hold each fingerprint.
    held: Mutex<BTreeMap<Fingerprint, usize>>,
}

/// Keeps its fingerprints in the store until dropped.
pub struct Pin {
    pins: Arc<Pins>,
    fingerprints: BTreeSet<Fingerprint>,
}

impl Pins {
    pub fn pin(self: &Arc<Self>, fingerprints: BTreeSet<Fingerprint>) -> Pin {
        let mut held = self.held.lock().expect("pins");
        for fingerprint in &fingerprints {
            *held.entry(*fingerprint).or_default() += 1;
        }
        Pin {
            pins: self.clone(),
            fingerprints,
        }
    }

    /// `keep` and everything pinned: what the store must retain.
    pub fn with_pinned(&self, mut keep: BTreeSet<Fingerprint>) -> BTreeSet<Fingerprint> {
        keep.extend(self.held.lock().expect("pins").keys().copied());
        keep
    }
}

impl Drop for Pin {
    fn drop(&mut self) {
        let mut held = self.pins.held.lock().expect("pins");
        for fingerprint in &self.fingerprints {
            if let Some(count) = held.get_mut(fingerprint) {
                *count -= 1;
                if *count == 0 {
                    held.remove(fingerprint);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(byte: u8) -> Fingerprint {
        Fingerprint([byte; 32])
    }

    #[test]
    fn pinned_content_is_kept_until_its_last_pin_ends() {
        let pins = Arc::new(Pins::default());
        let first = pins.pin(BTreeSet::from([f(1), f(2)]));
        let second = pins.pin(BTreeSet::from([f(2)]));
        let keep = |base: &[u8]| pins.with_pinned(base.iter().map(|b| f(*b)).collect());
        assert_eq!(keep(&[3]), BTreeSet::from([f(1), f(2), f(3)]));
        drop(first);
        assert_eq!(
            keep(&[3]),
            BTreeSet::from([f(2), f(3)]),
            "2 is still pinned"
        );
        drop(second);
        assert_eq!(keep(&[3]), BTreeSet::from([f(3)]));
    }
}
