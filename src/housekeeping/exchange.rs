//! What a member holding copies and the calling member that owns them tell each
//! other over the authenticated member channel.
//!
//! The member asks only the owner of each copy whether its workspace still exists,
//! and tells it which of its copies it removed. The owner answers only for
//! workspaces it pushed to the member that asks, so no other member can learn
//! about its paths or cause a copy's removal.
use crate::{
    failure::{Failure, Result},
    scheduling::timestamp::Timestamp,
};
use serde::{Deserialize, Serialize};

pub const ROUTE: &str = "/peer/v1/workspaces/check";
/// Most paths in one question, with the request size limit the real bound.
pub const MAXIMUM_PATHS: usize = 4096;
/// The longest caller path, as placement validates it.
pub const MAXIMUM_PATH: usize = 1024;
/// Paths go in questions of at most this many bytes, under the member API's limit.
pub const BATCH_BYTES: usize = 48 * 1024;

/// A member's question to the calling member that owns some of its copies.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    /// A dry run: the owner answers and changes nothing.
    #[serde(default)]
    pub dry_run: bool,
    /// Workspaces the member holds copies of, by the owner's own path.
    #[serde(default)]
    pub held: Vec<String>,
    /// Copies the member removed since it last told the owner.
    #[serde(default)]
    pub removed: Vec<RemovedCopy>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemovedCopy {
    pub path: String,
    pub removed_at: Timestamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceState {
    /// The workspace exists on the owner.
    Present,
    /// It no longer exists.
    Gone,
    /// The owner never pushed it to the member that asks, or cannot tell.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceAnswer {
    pub path: String,
    pub state: WorkspaceState,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub workspaces: Vec<WorkspaceAnswer>,
    /// Sync records of removed copies the owner dropped.
    pub forgotten: usize,
}

/// A caller's absolute, normalized path, as placement accepts it.
pub fn valid_path(path: &str) -> bool {
    path.len() > 1
        && path.len() <= MAXIMUM_PATH
        && path.starts_with('/')
        && !path.contains('\0')
        && path[1..]
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

impl Question {
    /// Checks a question that arrived from another member.
    pub fn validate(&self) -> Result<()> {
        let paths = self.held.iter().chain(self.removed.iter().map(|r| &r.path));
        if self.held.len() + self.removed.len() > MAXIMUM_PATHS
            || paths.into_iter().any(|p| !valid_path(p))
        {
            return Err(Failure::new(
                "invalid_request",
                "The workspace question is invalid.",
                "Install the same Elsecall version on every member.",
            ));
        }
        Ok(())
    }

    /// Splits held paths and removals into questions small enough to send.
    pub fn batches(dry_run: bool, held: Vec<String>, removed: Vec<RemovedCopy>) -> Vec<Self> {
        let mut batches = Vec::new();
        let mut current = Self {
            dry_run,
            ..Self::default()
        };
        let mut bytes = 0;
        // As JSON escapes it, with room for the removal time.
        let cost =
            |path: &str| serde_json::to_string(path).map_or(path.len() * 6, |s| s.len()) + 64;
        let full = |current: &Self, bytes: usize, next: usize| {
            bytes + next > BATCH_BYTES
                || current.held.len() + current.removed.len() >= MAXIMUM_PATHS
        };
        for path in held {
            if full(&current, bytes, cost(&path)) {
                batches.push(std::mem::replace(
                    &mut current,
                    Self {
                        dry_run,
                        ..Self::default()
                    },
                ));
                bytes = 0;
            }
            bytes += cost(&path);
            current.held.push(path);
        }
        for removal in removed {
            if full(&current, bytes, cost(&removal.path)) {
                batches.push(std::mem::replace(
                    &mut current,
                    Self {
                        dry_run,
                        ..Self::default()
                    },
                ));
                bytes = 0;
            }
            bytes += cost(&removal.path);
            current.removed.push(removal);
        }
        if !current.held.is_empty() || !current.removed.is_empty() {
            batches.push(current);
        }
        batches
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_absolute_and_normalized() {
        for valid in ["/a", "/Users/me/src/app wt"] {
            assert!(valid_path(valid), "{valid}");
        }
        for invalid in ["", "/", "a/b", "/a/../b", "/a/./b", "/a/", "/a//b", "/a\0b"] {
            assert!(!valid_path(invalid), "{invalid:?}");
        }
        assert!(!valid_path(&format!("/{}", "a".repeat(MAXIMUM_PATH))));
    }

    #[test]
    fn questions_fit_the_member_api_and_lose_nothing() {
        let held: Vec<String> = (0..2000)
            .map(|i| format!("/Users/me/src/{i:0>60}"))
            .collect();
        let removed: Vec<RemovedCopy> = (0..300)
            .map(|i| RemovedCopy {
                path: format!("/gone/{i}"),
                removed_at: Timestamp::from_millis(i),
            })
            .collect();
        let batches = Question::batches(true, held.clone(), removed.clone());
        assert!(batches.len() > 1);
        for batch in &batches {
            assert!(batch.dry_run);
            let body = serde_json::to_vec(batch).unwrap();
            assert!(
                body.len() < crate::peer_api::REQUEST_LIMIT,
                "{}",
                body.len()
            );
            batch.validate().unwrap();
        }
        let all_held: Vec<String> = batches.iter().flat_map(|b| b.held.clone()).collect();
        let all_removed: Vec<RemovedCopy> =
            batches.iter().flat_map(|b| b.removed.clone()).collect();
        assert_eq!(all_held, held);
        assert_eq!(all_removed, removed);
        assert!(Question::batches(false, vec![], vec![]).is_empty());
    }

    #[test]
    fn paths_json_must_escape_still_fit() {
        let held: Vec<String> = (0..200)
            .map(|i| format!("/{i}{}", "\"\u{1}\\".repeat(300)))
            .collect();
        for batch in Question::batches(false, held, Vec::new()) {
            let body = serde_json::to_vec(&batch).unwrap();
            assert!(
                body.len() < crate::peer_api::REQUEST_LIMIT,
                "{}",
                body.len()
            );
            batch.validate().unwrap();
        }
    }

    #[test]
    fn invalid_questions_are_refused() {
        let question = Question {
            held: vec!["relative".into()],
            ..Question::default()
        };
        assert_eq!(question.validate().unwrap_err().code, "invalid_request");
        assert!(serde_json::from_str::<Question>(r#"{"held":[],"extra":1}"#).is_err());
    }
}
