//! `elsecall replay`: recorded placement decisions scored again under changed
//! parameters, so a change can be judged against real decisions before it goes
//! into `config.toml`.
//!
//! Pure: the caller reads the records; replay re-scores each placement decision
//! from what its record holds, once, from the scoring that decided where its
//! request went first (see `recorded`):
//! - its **arrival** scoring, when the record holds that scoring's inputs;
//! - else its **last scoring**, when the record holds its inputs (records from
//!   before arrival inputs were kept; for a request that started at once, the
//!   last scoring is the arrival scoring);
//! - else, for records from before any inputs were kept, the choice between the
//!   **recorded estimates**, which only the keep-local margin can change.
//!
//! **Exact by construction.** Every decision is first scored again under the
//! parameters it recorded, and must reproduce its recorded winner and every
//! candidate's estimate exactly. One that does not is reported as not
//! reproduced and never replayed, so a flip is never an artefact of replay.
//!
//! **Not simulated.** A flipped decision would have changed what later
//! decisions saw: queues, waits, load and copies. Each decision is re-scored
//! against what it actually saw, and the output says so.
mod change;
mod recorded;
mod summary;

pub use change::Change;
pub use summary::Replay;

use crate::scheduling::record::Record;
use recorded::{RecordedScoring, Unreplayable};
use summary::{Replayed, Summary};

/// Replays the placement decisions among `records`, oldest first, under
/// `change`. Records that hold no placement decision are left out.
pub fn replay(records: &[Record], change: &Change) -> Replay {
    let mut summary = Summary::new(change);
    for record in records {
        let Some(scoring) = RecordedScoring::of(record) else {
            continue;
        };
        summary.add(record, replay_one(&scoring, change));
    }
    summary.finish()
}

fn replay_one(scoring: &RecordedScoring, change: &Change) -> Result<Replayed, Unreplayable> {
    if !scoring.replays(change) {
        return Err(Unreplayable::RecordedBeforeInputs);
    }
    let recorded = scoring.reproduce(change.changes_admission())?;
    let parameters = change
        .apply(&scoring.parameters)
        .map_err(Unreplayable::InvalidParameters)?;
    let replayed = scoring.score(&parameters, change)?;
    Ok(Replayed {
        origin: scoring.origin(),
        scored_at: scoring.scored_at,
        recorded,
        replayed,
    })
}

#[cfg(test)]
mod tests;
