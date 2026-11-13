//! Virtual fleet resource state: family charges, copies, leftovers and fixed
//! observations. Scheduling policy remains in the real member services.
use super::model::{self, Envelope};
use crate::{
    scheduling::{clock::Clock, placed::Warmth},
    workload::trace::Trace,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub(super) type References = Arc<BTreeMap<String, (f64, bool)>>;
#[derive(Clone)]
pub(super) struct CopyState {
    pub(super) warmth: Warmth,
    pub(super) busy: bool,
}
#[derive(Clone)]
pub(super) struct Occupancy {
    pub(super) member: String,
    pub(super) demand: f64,
    pub(super) declared: f64,
    pub(super) nested: bool,
    /// The outermost real admission grant, shared across nested requests.
    pub(super) family: String,
    pub(super) live: bool,
    pub(super) workspace: Option<String>,
}

pub(super) struct FamilyEnd {
    pub(super) still_live: bool,
    pub(super) uncertain: bool,
    pub(super) released_copy: Option<(String, String)>,
}
pub(super) struct World {
    pub(super) trace: Arc<Trace>,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) envelope: Envelope,
    pub(super) occupancy: BTreeMap<String, Occupancy>,
    pub(super) copies: BTreeMap<(String, String), CopyState>,
    pub(super) background: BTreeMap<(String, u64), f64>,
    pub(super) leftovers: BTreeMap<String, (String, f64, u64)>,
    pub(super) leftover_copies: BTreeMap<String, (String, String)>,
    pub(super) references: References,
    pub(super) pending_pre_run: BTreeMap<(String, String), u64>,
    pub(super) pre_run_uses: BTreeMap<(String, String), usize>,
    pub(super) missing_pre_run: BTreeSet<String>,
}
impl World {
    #[cfg(test)]
    pub(super) fn new(trace: Arc<Trace>, clock: Arc<dyn Clock>, envelope: Envelope) -> Self {
        let work = Arc::new(
            trace
                .requests
                .iter()
                .filter_map(|r| model::reference(&trace, r).map(|v| (r.fact.run_id.clone(), v)))
                .collect(),
        );
        Self::with_work(trace, clock, envelope, work)
    }
    pub(super) fn with_work(
        trace: Arc<Trace>,
        clock: Arc<dyn Clock>,
        envelope: Envelope,
        work: References,
    ) -> Self {
        let mut background = BTreeMap::new();
        for member in &trace.members {
            for b in &member.load {
                if let Some(outside) =
                    model::outside(&trace, &member.member, b.at.millis(), envelope)
                {
                    background.insert((member.member.clone(), b.at.millis()), outside);
                }
            }
        }
        Self {
            trace,
            clock,
            envelope,
            occupancy: BTreeMap::new(),
            copies: BTreeMap::new(),
            background,
            leftovers: BTreeMap::new(),
            leftover_copies: BTreeMap::new(),
            references: work,
            pending_pre_run: BTreeMap::new(),
            pre_run_uses: BTreeMap::new(),
            missing_pre_run: BTreeSet::new(),
        }
    }
    pub(super) fn next_pre_run_at(&self) -> Option<u64> {
        self.pending_pre_run.values().copied().min()
    }

    /// One effective resource charge per admission family. A finished root is
    /// retained as an anchor until the last nested request releases its grant.
    pub(super) fn family_resources<'a>(
        &'a self,
        member: &'a str,
    ) -> impl Iterator<Item = &'a Occupancy> {
        self.occupancy
            .values()
            .filter(move |o| o.member == member && !o.nested)
    }

    pub(super) fn deferred_family(&self, member: &str) -> bool {
        self.family_resources(member).any(|o| !o.live)
    }

    pub(super) fn end_occupancy(&mut self, id: &str) -> FamilyEnd {
        let Some(mut ended) = self.occupancy.remove(id) else {
            return FamilyEnd {
                still_live: false,
                uncertain: false,
                released_copy: None,
            };
        };
        let already_deferred = self
            .occupancy
            .values()
            .any(|o| o.family == ended.family && !o.nested && !o.live);
        let still_live = self
            .occupancy
            .values()
            .any(|o| o.family == ended.family && o.live);
        if still_live {
            if !ended.nested {
                ended.live = false;
                self.occupancy.insert(id.to_owned(), ended);
                return FamilyEnd {
                    still_live: true,
                    uncertain: true,
                    released_copy: None,
                };
            }
            return FamilyEnd {
                still_live: true,
                uncertain: already_deferred,
                released_copy: None,
            };
        }
        let root = if ended.nested {
            self.occupancy.remove(&ended.family)
        } else {
            Some(ended)
        };
        let released_copy = root.and_then(|root| root.workspace.map(|s| (root.member, s)));
        FamilyEnd {
            still_live: false,
            uncertain: already_deferred,
            released_copy,
        }
    }

    pub(super) fn outside(&self, member: &str) -> Option<f64> {
        let now = self.clock.now().millis();
        self.background
            .get(&(member.to_owned(), now / 60_000 * 60_000))
            .copied()
    }
    pub(super) fn cpu(&self, member: &str) -> Option<f64> {
        let cores = model::bucket(&self.trace, member, self.clock.now().millis())?
            .weighted_cores
            .as_f64();
        let active: f64 = self.family_resources(member).map(|o| o.demand).sum();
        let leftover: f64 = self
            .leftovers
            .values()
            .filter(|(m, _, _)| m == member)
            .map(|(_, d, _)| d)
            .sum();
        Some(((self.outside(member)? + active + leftover) / cores).clamp(0.0, 1.0))
    }
}

#[cfg(test)]
mod family_tests {
    use super::*;
    use crate::scheduling::clock::VirtualClock;

    #[test]
    fn finished_root_retains_one_family_resource_charge_until_last_nested_member_ends() {
        let trace = Arc::new(crate::simulation::engine::tests::trace());
        let clock = Arc::new(VirtualClock::new(trace.since));
        let mut world = World::new(trace, clock, Envelope::Point);
        let root = "r-000000000001".to_string();
        let child = "r-000000000002".to_string();
        let workspace = "/virtual/shared".to_string();
        world.occupancy.insert(
            root.clone(),
            Occupancy {
                member: "a".into(),
                demand: 1.0,
                declared: 1.0,
                nested: false,
                family: root.clone(),
                live: true,
                workspace: Some(workspace.clone()),
            },
        );
        world.occupancy.insert(
            child.clone(),
            Occupancy {
                member: "a".into(),
                demand: 99.0,
                declared: 99.0,
                nested: true,
                family: root.clone(),
                live: true,
                workspace: None,
            },
        );
        assert_eq!(world.cpu("a"), Some(0.5));
        let first_second = world.family_resources("a").map(|r| r.demand).sum::<f64>();
        let ended = world.end_occupancy(&root);
        assert!(ended.still_live && ended.uncertain);
        assert!(ended.released_copy.is_none());
        assert!(world.deferred_family("a"));
        assert_eq!(world.cpu("a"), Some(0.5));
        assert_eq!(
            world.family_resources("a").map(|r| r.declared).sum::<f64>(),
            1.0
        );
        let next_nine_seconds = world
            .family_resources("a")
            .map(|r| r.demand * 9.0)
            .sum::<f64>();
        assert_eq!(first_second + next_nine_seconds, 10.0);
        let ended = world.end_occupancy(&child);
        assert!(!ended.still_live && ended.uncertain);
        assert_eq!(ended.released_copy, Some(("a".into(), workspace)));
        assert!(world.occupancy.is_empty());
        assert_eq!(world.cpu("a"), Some(0.0));
    }
}
