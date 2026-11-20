//! Current capacity holders as seen by fleet status and scheduler diagnostics.
use super::{Book, Scheduler};
use crate::{
    active_grants::{ActiveGrant, ActiveGrants, GrantSnapshot, REPORTED_GRANTS},
    busy::Work,
};
use elsecall_contract::scheduling_model::capacity::Cores;

impl ActiveGrants for Scheduler {
    fn active_grants(&self) -> GrantSnapshot {
        let mut held = self.holding(&self.book());
        let total = held.len();
        let assigned = held
            .iter()
            .fold(Cores::ZERO, |sum, work| sum.saturating_add(work.size));
        held.sort_by_key(|work| std::cmp::Reverse(work.size));
        let operations = held
            .into_iter()
            .take(REPORTED_GRANTS)
            .map(|work| ActiveGrant {
                run_id: work.run_id,
                kind: work.kind,
                label: work.label,
                size: work.size,
            })
            .collect();
        GrantSnapshot {
            total,
            assigned,
            operations,
        }
    }
}

impl Scheduler {
    /// Work holding this member's capacity, for a `fleet_busy` report.
    pub(super) fn holding(&self, book: &Book) -> Vec<Work> {
        let mut holding: Vec<_> = book
            .admission
            .holding()
            .filter_map(|(root, size)| {
                // A root that ended still holds capacity for its nested work.
                let work = book.entries.get(root).or_else(|| {
                    book.entries
                        .values()
                        .filter(|e| e.record.shares_capacity_of.as_ref() == Some(root))
                        .min_by_key(|e| &e.record.run_id)
                })?;
                Some(Work {
                    run_id: work.record.run_id.clone(),
                    kind: work.record.kind,
                    label: work.record.label.clone(),
                    size,
                    started_at: work.record.started_at,
                    expected_run: work.expected_run,
                })
            })
            .collect();
        holding.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then(a.run_id.cmp(&b.run_id))
        });
        holding
    }
}
