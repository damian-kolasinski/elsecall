//! What placement knows about running work beyond what its member reported: how
//! long its command has run, and, when its member expects nothing of it, what
//! history expects of its whole run, by the chain new work's estimates take (the
//! label's runs on its member, converted from other members, then its family's),
//! warm or cold as its copy was.
//!
//! Learned once per running request on each member, before scoring needs it and
//! outside the book, since learning reads records; kept while the request holds
//! capacity there. History moves far more slowly than a request runs. A request
//! history knew nothing about is asked again after [`RELEARN_AFTER`], since its
//! label's or family's first runs may have ended meanwhile.
use super::{Book, Scheduler};
use crate::scheduling::{
    learning::RunEstimate,
    placed::{Reached, Side},
    placement::{LearnedRun, LearnedTime, Source},
    record::Record,
    request::Kind,
    run_id::RunId,
};
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use tokio::time::Instant;

/// Labels learned per round at most: each reads its history from records, and
/// rounds come every second. Work beyond it is learned in later rounds and
/// meanwhile scored as work without an estimate.
const LEARNED_PER_ROUND: usize = 8;
/// How long history that knew nothing about a running request is believed.
const RELEARN_AFTER: Duration = Duration::from_secs(60);

/// Running work, by the member it runs on and its run ID: a member's quote
/// names run IDs it chose, so they never stand for work elsewhere.
type Key = (String, RunId);

/// What was learned of each running request, and when.
#[derive(Default)]
pub(super) struct RunningEstimates {
    learned: HashMap<Key, (Instant, Option<LearnedRun>)>,
}

impl RunningEstimates {
    /// What history expects of the whole run of `id` on `member`, once learned.
    pub fn of(&self, member: &str, id: &RunId) -> Option<&LearnedRun> {
        self.learned
            .get(&(member.to_owned(), id.clone()))
            .and_then(|(_, learned)| learned.as_ref())
    }

    fn known(&self, key: &Key, now: Instant) -> bool {
        self.learned.get(key).is_some_and(|(at, learned)| {
            learned.is_some() || now.saturating_duration_since(*at) < RELEARN_AFTER
        })
    }
}

/// This master's record of the run `id` it placed on `member`, if it placed it
/// there. Anything else a member's quote names is that member's own work.
fn placed_here<'a>(book: &'a Book, member: &str, id: &RunId) -> Option<&'a Record> {
    book.entries.get(id).map(|e| &e.record).filter(|record| {
        record
            .placed
            .as_ref()
            .is_some_and(|p| p.side == Side::Caller && p.member == member)
    })
}

/// How long the command of work in `member`'s quote has run, which its member
/// counts from when it admitted the work: for a run this master placed there,
/// without its workspace sync (learned runs leave the sync out too), and none
/// before the command started.
pub(super) fn command_elapsed(
    book: &Book,
    member: &str,
    id: &RunId,
    running: Duration,
) -> Duration {
    let Some(placed) = placed_here(book, member, id).and_then(|r| r.placed.as_ref()) else {
        return running;
    };
    match placed.reached {
        Reached::Placed => Duration::ZERO,
        _ => running.saturating_sub(
            placed
                .sync
                .as_ref()
                .and_then(|s| s.duration_ms)
                .map_or(Duration::ZERO, Duration::from_millis),
        ),
    }
}

/// What learning one label's runs on one member needs; requests that share it
/// share what is learned.
#[derive(Clone, PartialEq, Eq)]
struct Question {
    kind: Kind,
    label: String,
    family: Option<String>,
    member: String,
    cold: bool,
    /// The units of the work's declared sequential plan.
    units: Option<u32>,
}

impl Scheduler {
    /// Learns what history expects of running work, here and on every member
    /// that answered a quote, that nothing expected yet, and forgets work that
    /// no longer holds capacity.
    pub(super) fn learn_running(&self) {
        let own = self.name();
        let now = Instant::now();
        let (questions, speeds) = {
            let mut book = self.book();
            let mut live = HashSet::new();
            let mut questions: Vec<(Question, Vec<Key>)> = Vec::new();
            let mut ask = |book: &Book, key: Key, question: Option<Question>| {
                if let Some(question) = question.filter(|_| !book.running.known(&key, now)) {
                    match questions.iter_mut().find(|(q, _)| *q == question) {
                        Some((_, keys)) => keys.push(key.clone()),
                        None => questions.push((question, vec![key.clone()])),
                    }
                }
                live.insert(key);
            };
            for work in self.holding(&book) {
                let record = book.entries.get(&work.run_id).map(|e| &e.record);
                let question = work
                    .label
                    .clone()
                    .filter(|_| work.expected_run.is_none())
                    .map(|label| Question {
                        kind: work.kind,
                        label,
                        family: family_of(record),
                        member: own.clone(),
                        cold: false,
                        units: units_of(record),
                    });
                ask(&book, (own.clone(), work.run_id.clone()), question);
            }
            for (member, work) in book.quotes.holding() {
                let record = placed_here(&book, member, &work.run_id);
                // A run placed here whose copy's warmth is not in yet is
                // learned once it is.
                let warmth = match record.and_then(|r| r.placed.as_ref()) {
                    Some(placed) => placed.warmth.map(|_| placed.cold()),
                    None => Some(false),
                };
                let question = work
                    .label
                    .clone()
                    .filter(|_| work.expected_remaining_ms.is_none())
                    .zip(warmth)
                    .map(|(label, cold)| Question {
                        kind: work.kind,
                        label,
                        family: family_of(record),
                        member: member.to_owned(),
                        cold,
                        units: units_of(record),
                    });
                ask(&book, (member.to_owned(), work.run_id.clone()), question);
            }
            book.running.learned.retain(|key, _| live.contains(key));
            let mut speeds: Vec<(String, f64)> = book
                .quotes
                .speeds()
                .map(|(member, speed)| (member.to_owned(), speed))
                .collect();
            speeds.extend(self.speed().map(|speed| (own.clone(), speed)));
            questions.truncate(LEARNED_PER_ROUND);
            (questions, speeds)
        };
        if questions.is_empty() {
            return;
        }
        let speed_of = |member: &str| speeds.iter().find(|(m, _)| m == member).map(|(_, s)| *s);
        let at = self.clock.now();
        let time = |estimate: RunEstimate| LearnedTime {
            time: estimate.run,
            source: Source::of(estimate.basis),
        };
        let learned: Vec<(Key, (Instant, Option<LearnedRun>))> = questions
            .into_iter()
            .flat_map(|(question, keys)| {
                let history = self
                    .run_history(
                        question.kind,
                        &question.label,
                        question.family.as_deref(),
                        at,
                    )
                    .with_plan(question.units);
                let estimates = history.estimates_on(&question.member, question.cold, &speed_of);
                let run = LearnedRun::for_copy(
                    question.cold,
                    estimates.warm.map(time),
                    estimates.cold.map(time),
                );
                keys.into_iter().map(move |key| (key, (now, run.clone())))
            })
            .collect();
        self.book().running.learned.extend(learned);
    }
}

/// The units of `record`'s declared sequential plan.
fn units_of(record: Option<&Record>) -> Option<u32> {
    record?.workload.as_ref()?.planned_units()
}

/// The label rule's pattern that names `record`'s family.
fn family_of(record: Option<&Record>) -> Option<String> {
    record
        .and_then(|r| r.settings.as_ref())
        .and_then(|s| s.rule.as_ref())
        .map(|rule| rule.pattern.clone())
}
