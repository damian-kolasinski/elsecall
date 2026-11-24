//! How missing tools affected this caller's work, from the same bounded read
//! of recent requests used by the fleet window.
use crate::{missing_tool, record::Record};
use elsecall_contract::scheduling_model::{run_id::RunId, timestamp::Timestamp};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const RECORD_LIMIT: u32 = 500;

pub struct ToolHistory {
    pub missing: Vec<missing_tool::Standing>,
    pub exclusions: BTreeMap<String, PinExclusions>,
}

/// Placement decisions that passed this member over for missing workspace
/// pins. One decision can have several missing tools.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinExclusions {
    pub decisions: usize,
    pub first_at: Timestamp,
    pub last_at: Timestamp,
    pub last_run_id: RunId,
    pub last_reason: String,
}

impl ToolHistory {
    pub(crate) fn from_records(records: &[Record], now: Timestamp) -> Self {
        let mut exclusions = BTreeMap::<String, PinExclusions>::new();
        for record in records.iter().filter(|r| !r.synthetic && !r.internal) {
            let Some(scoring) = record.decision.as_ref().and_then(|d| d.scoring.as_ref()) else {
                continue;
            };
            for passed in &scoring.passed_over {
                let at = scoring.scored_at;
                let impact =
                    exclusions
                        .entry(passed.member.clone())
                        .or_insert_with(|| PinExclusions {
                            decisions: 0,
                            first_at: at,
                            last_at: at,
                            last_run_id: record.run_id.clone(),
                            last_reason: passed.why.clone(),
                        });
                impact.decisions += 1;
                impact.first_at = impact.first_at.min(at);
                if at > impact.last_at {
                    impact.last_at = at;
                    impact.last_run_id = record.run_id.clone();
                    impact.last_reason.clone_from(&passed.why);
                }
            }
        }
        Self {
            missing: missing_tool::standing(records, now),
            exclusions,
        }
    }
}
