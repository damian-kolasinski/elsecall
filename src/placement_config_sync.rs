//! Placement's way to bring a member's synced configuration up to date before a
//! run starts there: the adapter of `scheduling::pre_run_sync::PreRunSync` over
//! configuration sync.
use crate::{
    config_sync::{
        compare::Change,
        report::Applied,
        service::{ConfigSync, MemberSync},
    },
    failure::{Failure, Result},
    roster::MemberName,
    scheduling::pre_run_sync::{PreRunSync, PushedPath, Synced},
};
use futures_util::future::BoxFuture;
use std::sync::Arc;

pub struct PlacementConfigSync {
    /// The member's configuration sync, or why it could not be opened; then no
    /// run is placed elsewhere, since its configuration could not be ensured.
    sync: std::result::Result<Arc<ConfigSync>, Failure>,
}

impl PlacementConfigSync {
    pub fn new(sync: std::result::Result<Arc<ConfigSync>, Failure>) -> Self {
        Self { sync }
    }
}

impl PreRunSync for PlacementConfigSync {
    fn bring_up_to_date(&self, member: &str) -> BoxFuture<'static, Result<Synced>> {
        let sync = self.sync.clone();
        let member = member.to_owned();
        Box::pin(async move {
            let sync = sync?;
            let name: MemberName = member
                .parse()
                .map_err(|_| crate::scheduling::placed::not_in_fleet(&member))?;
            Ok(synced(sync.bring_up_to_date(&name).await?))
        })
    }
}

fn synced(sync: Option<MemberSync>) -> Synced {
    let Some(sync) = sync else {
        return Synced::NothingSynced;
    };
    let paths = sync.paths.iter().map(ToString::to_string).collect();
    match sync.applied {
        None => Synced::Current {
            generation: sync.generation,
            paths,
        },
        Some(applied) => Synced::Pushed {
            generation: sync.generation,
            paths,
            pushed: pushed(&applied),
        },
    }
}

/// Per synced path, how many files the push changed and the bytes the member
/// received. The member's own overrides are left out: their sizes are its own.
fn pushed(applied: &Applied) -> Vec<PushedPath> {
    applied
        .paths
        .iter()
        .map(|path| {
            let files = path.files.iter().filter(|f| f.change != Change::Overridden);
            PushedPath {
                path: path.path.to_string(),
                files: files.clone().count(),
                bytes: files.filter_map(|f| f.size).sum(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_sync::{
        history::{FileRecord, PathRecord},
        synced_path::SyncedPath,
    };

    fn file(change: Change, size: Option<u64>) -> FileRecord {
        FileRecord {
            file: "~/.config/demo/x".into(),
            change,
            size,
            fingerprint: Some("0123456789abcdef".into()),
            backed_up: false,
        }
    }

    #[test]
    fn pushes_are_summarised_by_counts_and_sizes() {
        let path = SyncedPath::parse("~/.config/demo").unwrap();
        let applied = Applied {
            generation: 4,
            manifest_id: "m".into(),
            paths: vec![PathRecord {
                path: path.clone(),
                files: vec![
                    file(Change::Added, Some(10)),
                    file(Change::Changed, Some(5)),
                    file(Change::Removed, None),
                    file(Change::Overridden, Some(1000)),
                ],
            }],
        };
        let synced = synced(Some(MemberSync {
            generation: 4,
            paths: vec![path],
            applied: Some(applied),
        }));
        assert_eq!(
            synced,
            Synced::Pushed {
                generation: 4,
                paths: vec!["~/.config/demo".into()],
                pushed: vec![PushedPath {
                    path: "~/.config/demo".into(),
                    files: 3,
                    bytes: 15,
                }],
            }
        );
        assert_eq!(super::synced(None), Synced::NothingSynced);
    }
}
