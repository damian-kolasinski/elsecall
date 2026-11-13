//! Processes placed commands left running after they exited, per label: how many
//! runs left some, how much CPU time they used afterwards and how often the cap
//! stopped them. That work is not admitted, so a label that leaves heavy work
//! behind loads members outside admission's view; reports make it visible.
use crate::scheduling::{
    leftovers::{Ending, Leftovers},
    placed::Side,
    record::Record,
    timestamp::Timestamp,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The longest leftovers can have run: as long as records are kept.
const MOST_MS: u64 = crate::scheduling::records::RETENTION.as_millis() as u64;
/// More cores than any member has, to bound the CPU time a member reports.
pub const MOST_CORES: u64 = 1024;

/// The leftovers of one placed run, as the member that hosted it reports them.
/// Like a [`Fact`](crate::reporting::facts::Fact), it carries no paths,
/// processes or warnings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LeftoverFact {
    pub run_id: String,
    pub label: Option<String>,
    /// The member they ran on, which reports them.
    pub member: String,
    /// The calling member, which made the request.
    pub calling_member: String,
    pub requested_at: Timestamp,
    /// From the command's exit to the end of the last of them, when known.
    pub duration_ms: Option<u64>,
    /// CPU time they used after the exit, when known.
    pub cpu_ms: Option<u64>,
    /// The cap stopped them.
    pub capped: bool,
}

impl LeftoverFact {
    /// The fact of a placed run this member hosted, once its leftovers ended.
    pub fn of(record: &Record) -> Option<LeftoverFact> {
        let placed = record.placed.as_ref().filter(|p| p.side == Side::Member)?;
        let Some(Leftovers::Ended {
            duration_ms,
            cpu_ms,
            ending,
            ..
        }) = &placed.leftovers
        else {
            return None;
        };
        Some(LeftoverFact {
            run_id: record.run_id.to_string(),
            label: record.label.clone(),
            member: placed.member.clone(),
            calling_member: placed.calling_member.clone(),
            requested_at: record.requested_at,
            duration_ms: *duration_ms,
            cpu_ms: *cpu_ms,
            capped: *ending == Ending::Capped,
        })
    }

    /// Whether a fact `member` sent is plausible enough to count: its own, with
    /// a run ID, bounded printable names and label, and times a machine can
    /// have: no longer than records are kept, and no more CPU time than
    /// [`MOST_CORES`] cores give in that time.
    pub fn believable_from(&self, member: &str) -> bool {
        let text = |t: &str, most: usize| {
            !t.is_empty() && t.chars().count() <= most && !t.chars().any(char::is_control)
        };
        let longest = self.duration_ms.unwrap_or(MOST_MS);
        longest <= MOST_MS
            && self
                .cpu_ms
                .is_none_or(|cpu| cpu <= longest.saturating_mul(MOST_CORES))
            && self.member == member
            && crate::scheduling::run_id::RunId::parse(&self.run_id).is_some()
            && text(&self.calling_member, 255)
            && self
                .label
                .as_deref()
                .is_none_or(|l| text(l, crate::scheduling::request::MAXIMUM_LABEL))
    }
}

/// Leftovers over the report's period, filters applied.
#[derive(Debug, Default, PartialEq, Serialize)]
pub struct LeftoverMetrics {
    /// Placed runs that left processes running after they exited.
    pub runs: usize,
    /// Of those, the runs whose leftovers the cap stopped.
    pub capped: usize,
    /// CPU time all of them used after their commands exited.
    pub cpu_seconds: f64,
    /// Per label, the heaviest CPU time first.
    pub by_label: Vec<LabelLeftovers>,
}

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct LabelLeftovers {
    pub label: Option<String>,
    pub runs: usize,
    pub capped: usize,
    pub cpu_seconds: f64,
    /// The longest any of them ran after its command exited.
    pub max_seconds: f64,
}

impl LabelLeftovers {
    /// The share of the label's runs with leftovers that the cap stopped.
    pub fn capped_share(&self) -> f64 {
        if self.runs == 0 {
            0.0
        } else {
            self.capped as f64 / self.runs as f64
        }
    }
}

fn seconds(ms: Option<u64>) -> f64 {
    ms.unwrap_or(0) as f64 / 1000.0
}

pub fn leftovers(facts: &[LeftoverFact]) -> LeftoverMetrics {
    let mut by_label: BTreeMap<Option<String>, LabelLeftovers> = BTreeMap::new();
    for fact in facts {
        let label = by_label
            .entry(fact.label.clone())
            .or_insert_with(|| LabelLeftovers {
                label: fact.label.clone(),
                ..LabelLeftovers::default()
            });
        label.runs += 1;
        label.capped += usize::from(fact.capped);
        label.cpu_seconds += seconds(fact.cpu_ms);
        label.max_seconds = label.max_seconds.max(seconds(fact.duration_ms));
    }
    let mut by_label: Vec<LabelLeftovers> = by_label.into_values().collect();
    by_label.sort_by(|a, b| b.cpu_seconds.total_cmp(&a.cpu_seconds));
    LeftoverMetrics {
        runs: facts.len(),
        capped: facts.iter().filter(|f| f.capped).count(),
        cpu_seconds: facts.iter().map(|f| seconds(f.cpu_ms)).sum(),
        by_label,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::fixtures::{self, Run};

    fn hosted(n: u64, label: &'static str, ending: Ending, cpu_ms: u64) -> Record {
        let mut record = fixtures::placed(Run {
            n,
            label,
            member: "devbox",
            ..Run::default()
        });
        let placed = record.placed.as_mut().unwrap();
        placed.side = Side::Member;
        placed.leftovers = Some(Leftovers::Ended {
            since: Timestamp::from_millis(0),
            cap_ms: 600_000,
            duration_ms: Some(cpu_ms * 2),
            cpu_ms: Some(cpu_ms),
            ending,
        });
        record
    }

    #[test]
    fn only_ended_leftovers_of_hosted_runs_are_facts() {
        let record = hosted(1, "test", Ending::Capped, 4_000);
        let fact = LeftoverFact::of(&record).unwrap();
        assert!(fact.capped);
        assert_eq!((fact.cpu_ms, fact.duration_ms), (Some(4_000), Some(8_000)));
        assert_eq!(fact.member, "devbox");
        assert!(fact.believable_from("devbox"));
        assert!(!fact.believable_from("mini"), "only a member's own");
        let mut caller_side = record.clone();
        caller_side.placed.as_mut().unwrap().side = Side::Caller;
        assert_eq!(LeftoverFact::of(&caller_side), None);
        let mut running = record.clone();
        running.placed.as_mut().unwrap().leftovers = Some(Leftovers::Running {
            since: Timestamp::from_millis(0),
            cap_ms: 1,
            group: 2,
            processes: Vec::new(),
        });
        assert_eq!(LeftoverFact::of(&running), None);
        let mut none = record;
        none.placed.as_mut().unwrap().leftovers = None;
        assert_eq!(LeftoverFact::of(&none), None);
        for forged in [
            LeftoverFact {
                label: Some("a\nb".into()),
                ..fact.clone()
            },
            LeftoverFact {
                duration_ms: Some(u64::MAX),
                ..fact.clone()
            },
            LeftoverFact {
                cpu_ms: Some(8_000 * MOST_CORES + 1),
                ..fact.clone()
            },
            LeftoverFact {
                duration_ms: None,
                cpu_ms: Some(u64::MAX),
                ..fact
            },
        ] {
            assert!(!forged.believable_from("devbox"), "{forged:?}");
        }
    }

    #[test]
    fn leftovers_add_up_per_label_heaviest_first() {
        let facts: Vec<LeftoverFact> = [
            hosted(1, "test", Ending::Capped, 4_000),
            hosted(2, "test", Ending::Finished, 2_000),
            hosted(3, "build", Ending::Finished, 9_000),
        ]
        .iter()
        .filter_map(LeftoverFact::of)
        .collect();
        let metrics = leftovers(&facts);
        assert_eq!((metrics.runs, metrics.capped), (3, 1));
        assert_eq!(metrics.cpu_seconds, 15.0);
        assert_eq!(metrics.by_label[0].label.as_deref(), Some("build"));
        let test = &metrics.by_label[1];
        assert_eq!((test.runs, test.capped), (2, 1));
        assert_eq!(test.cpu_seconds, 6.0);
        assert_eq!(test.max_seconds, 8.0);
        assert_eq!(test.capped_share(), 0.5);
    }
}
