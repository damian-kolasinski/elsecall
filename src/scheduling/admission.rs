//! The admission policy of one member: which work holds capacity and which waiting
//! request starts next. Pure state; time, load and processes are the caller's.
use crate::scheduling::{capacity::Cores, run_id::RunId};
use std::collections::{BTreeSet, HashMap, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Interactive,
    Deferrable,
}

/// A grant and the nested grants that reuse it. Its capacity stays in use until
/// every member of the family has ended, so nested work never runs uncounted.
#[derive(Debug)]
struct Family {
    size: Cores,
    live: BTreeSet<RunId>,
}

#[derive(Debug, Clone)]
struct Waiting {
    id: RunId,
    size: Cores,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueueCounts {
    pub interactive: usize,
    pub deferrable: usize,
}

#[derive(Debug)]
pub struct Admission {
    capacity: Cores,
    in_use: Cores,
    families: HashMap<RunId, Family>,
    root_of: HashMap<RunId, RunId>,
    /// Deferrable work whose deadline passed with "start", ahead of interactive work.
    promoted: VecDeque<Waiting>,
    interactive: VecDeque<Waiting>,
    deferrable: VecDeque<Waiting>,
}

impl Admission {
    pub fn new(capacity: Cores) -> Self {
        Self {
            capacity,
            in_use: Cores::ZERO,
            families: HashMap::new(),
            root_of: HashMap::new(),
            promoted: VecDeque::new(),
            interactive: VecDeque::new(),
            deferrable: VecDeque::new(),
        }
    }
    pub fn capacity(&self) -> Cores {
        self.capacity
    }
    pub fn in_use(&self) -> Cores {
        self.in_use
    }
    pub fn free(&self) -> Cores {
        self.capacity.saturating_sub(self.in_use)
    }
    /// Whether the request could ever start here.
    pub fn fits(&self, size: Cores) -> bool {
        size <= self.capacity
    }
    pub fn queued(&self) -> QueueCounts {
        QueueCounts {
            interactive: self.promoted.len() + self.interactive.len(),
            deferrable: self.deferrable.len(),
        }
    }
    /// Waiting requests with their sizes, in the order they would start.
    pub fn waiting(&self) -> impl Iterator<Item = (&RunId, Cores)> {
        self.queues().map(|w| (&w.id, w.size))
    }
    /// Waiting requests that start before new interactive work, in order:
    /// deferrable work waits behind it.
    pub fn waiting_interactive(&self) -> impl Iterator<Item = (&RunId, Cores)> {
        self.promoted
            .iter()
            .chain(&self.interactive)
            .map(|w| (&w.id, w.size))
    }
    pub fn is_waiting(&self, id: &RunId) -> bool {
        self.queues().any(|w| &w.id == id)
    }
    pub fn is_live(&self, id: &RunId) -> bool {
        self.root_of.contains_key(id)
    }
    /// The grant whose capacity `id` uses: its own, or its outermost parent's.
    pub fn root(&self, id: &RunId) -> Option<&RunId> {
        self.root_of.get(id)
    }
    /// The capacity a live root grant holds for its family.
    pub fn size_of(&self, root: &RunId) -> Option<Cores> {
        self.families.get(root).map(|family| family.size)
    }
    /// Live grants whose capacity is in use, with their sizes.
    pub fn holding(&self) -> impl Iterator<Item = (&RunId, Cores)> {
        self.families.iter().map(|(id, family)| (id, family.size))
    }

    fn queues(&self) -> impl Iterator<Item = &Waiting> {
        self.promoted
            .iter()
            .chain(&self.interactive)
            .chain(&self.deferrable)
    }

    /// Queues a request that fits. Within a class, requests start in arrival order.
    pub fn enqueue(&mut self, id: RunId, size: Cores, class: Class) {
        debug_assert!(self.fits(size));
        let waiting = Waiting { id, size };
        match class {
            Class::Interactive => self.interactive.push_back(waiting),
            Class::Deferrable => self.deferrable.push_back(waiting),
        }
    }

    /// Removes a waiting request. False if it was not waiting.
    pub fn withdraw(&mut self, id: &RunId) -> bool {
        [
            &mut self.promoted,
            &mut self.interactive,
            &mut self.deferrable,
        ]
        .into_iter()
        .any(|queue| match queue.iter().position(|w| &w.id == id) {
            Some(index) => {
                queue.remove(index);
                true
            }
            None => false,
        })
    }

    /// Moves waiting deferrable work ahead of all interactive work that was not
    /// promoted before it.
    pub fn promote(&mut self, id: &RunId) -> bool {
        match self.deferrable.iter().position(|w| &w.id == id) {
            Some(index) => {
                let waiting = self.deferrable.remove(index).expect("index is in range");
                self.promoted.push_back(waiting);
                true
            }
            None => false,
        }
    }

    /// The request that starts next, if one may start now. Interactive work goes
    /// first; within a class the oldest request goes first, and a head that does
    /// not fit blocks everything behind it so large requests are never starved.
    /// Deferrable work starts only while no interactive work waits.
    pub fn ready(&self, load_admits: bool) -> Option<&RunId> {
        if !load_admits {
            return None;
        }
        let head = self
            .promoted
            .front()
            .or(self.interactive.front())
            .or(self.deferrable.front())?;
        (head.size <= self.free()).then_some(&head.id)
    }

    /// Starts the request `ready` returned, charging its size.
    pub fn start(&mut self, id: &RunId) {
        let queue = [
            &mut self.promoted,
            &mut self.interactive,
            &mut self.deferrable,
        ]
        .into_iter()
        .find(|queue| queue.front().is_some_and(|w| &w.id == id))
        .expect("only the head of a queue starts");
        let waiting = queue.pop_front().expect("queue has a head");
        self.adopt(waiting.id, waiting.size);
    }

    /// Records a grant that already holds capacity, as after a service restart.
    pub fn adopt(&mut self, id: RunId, size: Cores) {
        self.in_use = self.in_use.saturating_add(size);
        self.root_of.insert(id.clone(), id.clone());
        self.families.insert(
            id.clone(),
            Family {
                size,
                live: BTreeSet::from([id]),
            },
        );
    }

    /// Admits `id` within the live grant `outer`, using no further capacity.
    /// Returns the grant whose capacity it shares.
    pub fn nest(&mut self, id: RunId, outer: &RunId) -> RunId {
        let root = self.root_of[outer].clone();
        self.families
            .get_mut(&root)
            .expect("live grants have a family")
            .live
            .insert(id.clone());
        self.root_of.insert(id, root.clone());
        root
    }

    /// Ends a live grant. Its family's capacity frees once all members ended.
    pub fn end(&mut self, id: &RunId) {
        let Some(root) = self.root_of.remove(id) else {
            return;
        };
        let family = self
            .families
            .get_mut(&root)
            .expect("live grants have a family");
        family.live.remove(id);
        if family.live.is_empty() {
            self.in_use = self.in_use.saturating_sub(family.size);
            self.families.remove(&root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> RunId {
        RunId::parse(&format!("r-{n:012x}")).unwrap()
    }
    fn cores(n: u32) -> Cores {
        Cores::from_millis(n * 1000)
    }
    /// Starts everything that may start, returning the order.
    fn drain(admission: &mut Admission) -> Vec<RunId> {
        let mut started = Vec::new();
        while let Some(next) = admission.ready(true).cloned() {
            admission.start(&next);
            started.push(next);
        }
        started
    }

    #[test]
    fn never_overcommits_and_starts_in_arrival_order() {
        let mut admission = Admission::new(cores(4));
        for n in 1..=5 {
            admission.enqueue(id(n), cores(1), Class::Interactive);
        }
        assert_eq!(drain(&mut admission), vec![id(1), id(2), id(3), id(4)]);
        assert_eq!(admission.free(), Cores::ZERO);
        admission.end(&id(2));
        assert_eq!(drain(&mut admission), vec![id(5)]);
        assert_eq!(admission.in_use(), cores(4));
    }

    #[test]
    fn a_large_head_blocks_smaller_requests_behind_it() {
        let mut admission = Admission::new(cores(4));
        admission.enqueue(id(1), cores(3), Class::Interactive);
        drain(&mut admission);
        admission.enqueue(id(2), cores(4), Class::Interactive);
        admission.enqueue(id(3), cores(1), Class::Interactive);
        assert!(drain(&mut admission).is_empty(), "small request waits too");
        admission.end(&id(1));
        assert_eq!(drain(&mut admission), vec![id(2)]);
        admission.end(&id(2));
        assert_eq!(drain(&mut admission), vec![id(3)]);
    }

    #[test]
    fn interactive_goes_first_and_deferrable_waits_while_interactive_waits() {
        let mut admission = Admission::new(cores(2));
        admission.enqueue(id(1), cores(2), Class::Interactive);
        drain(&mut admission);
        admission.enqueue(id(2), cores(1), Class::Deferrable);
        admission.enqueue(id(3), cores(2), Class::Interactive);
        admission.enqueue(id(4), cores(1), Class::Interactive);
        admission.end(&id(1));
        assert_eq!(drain(&mut admission), vec![id(3)]);
        admission.end(&id(3));
        assert_eq!(drain(&mut admission), vec![id(4), id(2)]);
    }

    #[test]
    fn promoted_deferrable_work_goes_to_the_front_without_overcommitting() {
        let mut admission = Admission::new(cores(2));
        admission.enqueue(id(1), cores(2), Class::Interactive);
        drain(&mut admission);
        admission.enqueue(id(2), cores(1), Class::Interactive);
        admission.enqueue(id(3), cores(1), Class::Deferrable);
        admission.enqueue(id(4), cores(1), Class::Deferrable);
        assert!(admission.promote(&id(4)));
        assert!(
            !admission.promote(&id(2)),
            "only deferrable work is promoted"
        );
        assert!(drain(&mut admission).is_empty());
        admission.end(&id(1));
        assert_eq!(drain(&mut admission), vec![id(4), id(2)]);
        assert_eq!(admission.queued().deferrable, 1);
    }

    #[test]
    fn load_blocks_new_work_and_withdrawn_requests_never_start() {
        let mut admission = Admission::new(cores(2));
        admission.enqueue(id(1), cores(1), Class::Interactive);
        admission.enqueue(id(2), cores(1), Class::Interactive);
        assert_eq!(admission.ready(false), None);
        assert!(admission.withdraw(&id(1)));
        assert!(!admission.withdraw(&id(1)));
        assert_eq!(drain(&mut admission), vec![id(2)]);
    }

    #[test]
    fn nested_grants_share_their_root_and_keep_its_capacity_until_all_end() {
        let mut admission = Admission::new(cores(2));
        admission.enqueue(id(1), cores(2), Class::Interactive);
        drain(&mut admission);
        assert_eq!(admission.nest(id(2), &id(1)), id(1));
        assert_eq!(
            admission.nest(id(3), &id(2)),
            id(1),
            "nesting is transitive"
        );
        assert_eq!(admission.in_use(), cores(2));
        admission.end(&id(1));
        assert_eq!(admission.in_use(), cores(2), "nested work still runs");
        admission.end(&id(3));
        admission.end(&id(2));
        assert_eq!(admission.in_use(), Cores::ZERO);
        admission.end(&id(2));
        assert_eq!(admission.in_use(), Cores::ZERO, "ending twice is harmless");
    }
}
