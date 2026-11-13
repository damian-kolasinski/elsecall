//! What a member tells its master before placement decides: its admission state
//! and load, the work holding and waiting for its capacity, and the state of its
//! copies of the workspaces being placed. A quote crosses members, so the master
//! bounds what it believes.
use crate::scheduling::{
    capacity::Cores,
    messages::MemberState,
    placed::{Warmth, WorkspaceRef},
    request::Kind,
    run_id::RunId,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Workspaces one quote may ask about.
pub const MAXIMUM_WORKSPACES: usize = 16;
const MAXIMUM_WORK: usize = 256;
/// Longer times than this are not believed: no hold limit or run is that long.
const MAXIMUM_MS: u64 = 31 * 24 * 60 * 60 * 1000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuoteRequest {
    pub workspaces: Vec<WorkspaceRef>,
}

/// Work holding a member's capacity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HeldWork {
    pub run_id: RunId,
    pub kind: Kind,
    pub label: Option<String>,
    pub size: Cores,
    pub running_ms: u64,
    pub expected_remaining_ms: Option<u64>,
}

/// A request waiting in a member's own queue, in order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WaitingWork {
    pub size: Cores,
    pub expected_run_ms: Option<u64>,
}

/// A member's copy of one asked-about workspace.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CopyQuote {
    /// The caller's workspace root, as asked.
    pub workspace: String,
    pub warmth: Warmth,
    /// Another run uses the copy now.
    pub busy: bool,
    /// The member cannot host this workspace, as when a same-path copy would
    /// overlap its own directories.
    #[serde(default)]
    pub unavailable: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Quote {
    pub state: MemberState,
    /// Average CPU utilization over the saturation window.
    pub utilization: Option<f64>,
    pub holding: Vec<HeldWork>,
    pub waiting: Vec<WaitingWork>,
    pub copies: Vec<CopyQuote>,
    /// The thresholds the member's gate uses; absent from earlier versions.
    #[serde(default)]
    pub admission: Option<crate::scheduling::parameters::Admission>,
}

fn label_ok(label: &str) -> bool {
    !label.is_empty() && label.chars().count() <= 200 && !label.chars().any(char::is_control)
}

impl Quote {
    /// The quote as the master believes it: implausible values dropped, lists
    /// bounded, copies only for the workspaces asked about.
    pub fn believed(mut self, asked: &[WorkspaceRef]) -> Self {
        self.utilization = self
            .utilization
            .filter(|u| u.is_finite() && (0.0..=1.0).contains(u));
        self.holding.truncate(MAXIMUM_WORK);
        for work in &mut self.holding {
            work.label = work.label.take().filter(|l| label_ok(l));
            work.running_ms = work.running_ms.min(MAXIMUM_MS);
            work.expected_remaining_ms = work.expected_remaining_ms.filter(|ms| *ms <= MAXIMUM_MS);
        }
        self.waiting.truncate(MAXIMUM_WORK);
        for work in &mut self.waiting {
            work.expected_run_ms = work.expected_run_ms.filter(|ms| *ms <= MAXIMUM_MS);
        }
        self.copies
            .retain(|copy| asked.iter().any(|w| w.root == copy.workspace));
        self.copies.truncate(MAXIMUM_WORKSPACES);
        self
    }

    pub fn copy_of(&self, workspace: &WorkspaceRef) -> Option<&CopyQuote> {
        self.copies.iter().find(|c| c.workspace == workspace.root)
    }
}

pub fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduling::{admission::QueueCounts, capacity::Capacity, load::Gate};

    #[test]
    fn the_master_believes_only_plausible_bounded_answers() {
        let asked = WorkspaceRef {
            root: "/w/app".into(),
            common_dir: "/w/app/.git".into(),
        };
        let quote = Quote {
            state: MemberState {
                member: "devbox".into(),
                capacity: Capacity {
                    weighted_cores: Cores::ONE,
                    reserved: Cores::ZERO,
                    capacity: Cores::ONE,
                },
                in_use: Cores::ZERO,
                free: Cores::ONE,
                queued: QueueCounts::default(),
                load: None,
                gate: Gate::Open,
                speed: None,
            },
            utilization: Some(f64::NAN),
            holding: vec![
                HeldWork {
                    run_id: RunId::parse("r-000000000001").unwrap(),
                    kind: Kind::Run,
                    label: Some("bad\nlabel".into()),
                    size: Cores::ONE,
                    running_ms: u64::MAX,
                    expected_remaining_ms: Some(u64::MAX),
                };
                300
            ],
            waiting: Vec::new(),
            admission: None,
            copies: vec![
                CopyQuote {
                    workspace: "/w/app".into(),
                    warmth: Warmth::Warm,
                    busy: false,
                    unavailable: false,
                },
                CopyQuote {
                    workspace: "/elsewhere".into(),
                    warmth: Warmth::Warm,
                    busy: false,
                    unavailable: false,
                },
            ],
        }
        .believed(std::slice::from_ref(&asked));
        assert_eq!(quote.utilization, None);
        assert_eq!(quote.holding.len(), MAXIMUM_WORK);
        assert_eq!(quote.holding[0].label, None);
        assert_eq!(quote.holding[0].running_ms, MAXIMUM_MS);
        assert_eq!(quote.holding[0].expected_remaining_ms, None);
        assert_eq!(quote.copies.len(), 1);
        assert!(quote.copy_of(&asked).is_some());
    }
}
