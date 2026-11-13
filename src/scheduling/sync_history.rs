//! What history says about workspace syncs: how long making a member's copy match
//! the caller's tree takes, warm or cold. Pure: the records come from the caller.
//!
//! Syncs are learned from the calling side's records of placed runs requested
//! within the recency of `[estimates]`, at most the latest `window` of each kind
//! (see `parameters`; the counts below are the defaults):
//! - **Warm:** the median of this workspace's warm syncs to the member, from two
//!   samples on; otherwise of every workspace's warm syncs to the member, from
//!   three. A warm sync costs about the same whatever the run.
//! - **Cold:** the median of first syncs of the same repository to any member, in
//!   the same state (with or without the repository's history already there),
//!   from one sample on. A first sync happens once per copy, so a single sample of
//!   the same repository is the best evidence there will be; it depends on the
//!   repository's size far more than on the member.
use crate::scheduling::{
    parameters::Estimates,
    placed::{Side, Warmth, WorkspaceRef},
    record::Record,
    timestamp::Timestamp,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// One placed run's sync.
#[derive(Clone, Debug, PartialEq)]
pub struct SyncSample {
    pub member: String,
    pub workspace: String,
    pub repository: Option<String>,
    pub warmth: Warmth,
    pub sync: Duration,
}

/// Where a sync estimate came from, for `explain`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum SyncBasis {
    /// This workspace's warm syncs to the member.
    Workspace { samples: usize },
    /// Every workspace's warm syncs to the member.
    Member { samples: usize },
    /// First syncs of the same repository.
    Repository { samples: usize },
}

#[derive(Clone, Debug, PartialEq)]
pub struct SyncEstimate {
    pub sync: Duration,
    pub basis: SyncBasis,
}

/// The syncs in `records`, newest first: the calling side of placed runs requested
/// within `recency` before `now` whose sync time and warmth are known.
pub fn samples(records: &[Record], now: Timestamp, recency: Duration) -> Vec<SyncSample> {
    let since = Timestamp::from_millis(now.millis().saturating_sub(recency.as_millis() as u64));
    let mut found: Vec<(Timestamp, SyncSample)> = records
        .iter()
        .filter(|r| r.requested_at >= since)
        .filter(|r| !r.synthetic && !crate::rehearsal::synthetic(r.label.as_deref()))
        .filter_map(|record| {
            let placed = record.placed.as_ref().filter(|p| p.side == Side::Caller)?;
            Some((
                record.requested_at,
                SyncSample {
                    member: placed.member.clone(),
                    workspace: placed.caller_workspace.clone(),
                    repository: placed.repository.clone(),
                    warmth: placed.warmth?,
                    sync: Duration::from_millis(placed.sync.as_ref()?.duration_ms?),
                },
            ))
        })
        .collect();
    found.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    found.into_iter().map(|(_, sample)| sample).collect()
}

#[derive(Clone, Debug)]
pub struct SyncHistory {
    samples: Vec<SyncSample>,
    trust: Estimates,
}
impl Default for SyncHistory {
    fn default() -> Self {
        Self::new(
            Vec::new(),
            &crate::scheduling::parameters::Parameters::default().estimates,
        )
    }
}

fn median(values: &mut [Duration]) -> Duration {
    values.sort();
    values[values.len() / 2]
}

impl SyncHistory {
    pub fn new(samples: Vec<SyncSample>, estimates: &Estimates) -> Self {
        Self {
            samples,
            trust: estimates.clone(),
        }
    }

    /// How long syncing `workspace` to `member`, whose copy is in state `warmth`,
    /// is expected to take; `None` without trusted history.
    pub fn sync_to(
        &self,
        member: &str,
        workspace: &WorkspaceRef,
        warmth: Warmth,
    ) -> Option<SyncEstimate> {
        let pick = |filter: &dyn Fn(&SyncSample) -> bool, minimum: usize| {
            let mut found: Vec<Duration> = self
                .samples
                .iter()
                .filter(|s| s.warmth == warmth && filter(s))
                .take(self.trust.window)
                .map(|s| s.sync)
                .collect();
            (found.len() >= minimum).then(|| (median(&mut found), found.len()))
        };
        if warmth == Warmth::Warm {
            if let Some((sync, samples)) = pick(
                &|s| s.member == member && s.workspace == workspace.root,
                self.trust.minimum_workspace_syncs,
            ) {
                return Some(SyncEstimate {
                    sync,
                    basis: SyncBasis::Workspace { samples },
                });
            }
            let (sync, samples) = pick(&|s| s.member == member, self.trust.minimum_member_syncs)?;
            return Some(SyncEstimate {
                sync,
                basis: SyncBasis::Member { samples },
            });
        }
        let (sync, samples) = pick(
            &|s| s.repository.as_deref() == Some(workspace.common_dir.as_str()),
            self.trust.minimum_repository_syncs,
        )?;
        Some(SyncEstimate {
            sync,
            basis: SyncBasis::Repository { samples },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(member: &str, workspace: &str, warmth: Warmth, ms: u64) -> SyncSample {
        SyncSample {
            member: member.into(),
            workspace: workspace.into(),
            repository: Some("/r/.git".into()),
            warmth,
            sync: Duration::from_millis(ms),
        }
    }
    fn workspace(root: &str) -> WorkspaceRef {
        WorkspaceRef {
            root: root.into(),
            common_dir: "/r/.git".into(),
        }
    }

    #[test]
    fn warm_syncs_prefer_the_workspace_then_the_member() {
        let samples = vec![
            sample("devbox", "/w/a", Warmth::Warm, 400),
            sample("devbox", "/w/b", Warmth::Warm, 900),
            sample("devbox", "/w/b", Warmth::Warm, 1000),
            sample("other", "/w/a", Warmth::Warm, 50),
        ];
        let defaults = crate::scheduling::parameters::Parameters::default().estimates;
        let stricter = Estimates {
            minimum_workspace_syncs: 3,
            minimum_member_syncs: 4,
            ..defaults.clone()
        };
        assert_eq!(
            SyncHistory::new(samples.clone(), &stricter).sync_to(
                "devbox",
                &workspace("/w/b"),
                Warmth::Warm
            ),
            None,
            "stricter trust needs more syncs"
        );
        let history = SyncHistory::new(samples, &defaults);
        assert_eq!(
            history.sync_to("devbox", &workspace("/w/b"), Warmth::Warm),
            Some(SyncEstimate {
                sync: Duration::from_millis(1000),
                basis: SyncBasis::Workspace { samples: 2 }
            })
        );
        assert_eq!(
            history
                .sync_to("devbox", &workspace("/w/a"), Warmth::Warm)
                .unwrap()
                .basis,
            SyncBasis::Member { samples: 3 }
        );
        assert_eq!(
            history.sync_to("other", &workspace("/w/b"), Warmth::Warm),
            None
        );
    }

    #[test]
    fn cold_syncs_come_from_the_same_repository_in_the_same_state() {
        let history = SyncHistory::new(
            vec![
                sample("devbox", "/w/a", Warmth::Cold, 43_000),
                sample("devbox", "/w/b", Warmth::ColdWithHistory, 5_800),
            ],
            &crate::scheduling::parameters::Parameters::default().estimates,
        );
        let new = workspace("/w/c");
        assert_eq!(
            history.sync_to("laptop", &new, Warmth::Cold).unwrap().sync,
            Duration::from_secs(43)
        );
        assert_eq!(
            history
                .sync_to("devbox", &new, Warmth::ColdWithHistory)
                .unwrap()
                .sync,
            Duration::from_millis(5_800)
        );
        let elsewhere = WorkspaceRef {
            root: "/x".into(),
            common_dir: "/x/.git".into(),
        };
        assert_eq!(history.sync_to("devbox", &elsewhere, Warmth::Cold), None);
    }
}
