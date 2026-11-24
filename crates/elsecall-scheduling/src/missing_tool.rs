//! A placed run that failed because a tool it called is missing on its member:
//! the one fact Elsecall keeps about it, and which of those facts still stand.
//!
//! The caller recognises the tool while the run's output passes through it and
//! reports only the tool's name, validated here, never the output itself
//! (`docs/fleet-provisioning.md`, "When a job needs something unpinned"). The
//! fleet view turns standing facts into `missing_tool_pin` attention items.
use crate::placed::Side;
use crate::record::{Outcome, Record};
use elsecall_contract::scheduling_model::{run_id::RunId, timestamp::Timestamp};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, time::Duration};

/// How long a missing tool stays worth attention after the run that hit it.
pub const STANDS_FOR: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The longest tool name kept.
const MAXIMUM_NAME: usize = 64;

/// A tool's name as a command or a manager calls it: `node`, `xcodebuild`,
/// `npm:@scope/pkg`. Letters, digits and `-_.+@:/`, starting with a letter or
/// digit, and never shaped like a credential, so a secret a script ran as a
/// command by mistake (`$TOKEN` unquoted) is never kept.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ToolName(String);

impl ToolName {
    pub fn new(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        let valid = !bytes.is_empty()
            && bytes.len() <= MAXIMUM_NAME
            && bytes[0].is_ascii_alphanumeric()
            && bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.+@:/".contains(b))
            && !text.contains("..")
            && !elsecall_contract::secret_shape::looks_like_token(text);
        valid.then(|| Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl TryFrom<String> for ToolName {
    type Error = String;
    fn try_from(text: String) -> Result<Self, String> {
        Self::new(&text).ok_or_else(|| "not a tool name".to_owned())
    }
}
impl From<ToolName> for String {
    fn from(name: ToolName) -> String {
        name.0
    }
}

/// Which pin source owns the tool: the Xcode version for Xcode and the
/// utilities only Xcode carries, mise for everything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Manager {
    Mise,
    Xcodes,
}

impl Manager {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mise => "mise",
            Self::Xcodes => "xcodes",
        }
    }
}

/// The tool a run's member lacked. For Xcode, `tool` is `xcode`, the pin's
/// name, and `called` the utility that needed it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MissingTool {
    pub manager: Manager,
    pub tool: ToolName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub called: Option<ToolName>,
}

/// A stored missing tool, or none when it no longer validates.
pub fn lenient<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<MissingTool>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| serde_json::from_value(value).ok()))
}

/// A missing tool that still stands: the latest run that hit it on a member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Standing {
    pub run_id: RunId,
    pub member: String,
    pub missing: MissingTool,
    /// The calling member's working tree the run came from, where a pin goes.
    pub workspace: Option<String>,
    pub ended_at: Timestamp,
}

/// The missing tools among `records` (any order) that still stand at `now`.
///
/// Only runs this member placed on another one count. A run that hit a missing
/// tool stands unless it ended more than `STANDS_FOR` ago, or a later run of
/// the same job (same label, which an unlabelled run has none of) from the same
/// working tree exited 0 on that member: the tool was installed by hand, or
/// the job no longer needs it. For each member and tool, the latest run that
/// still stands is named. A pin clears it too; the fleet view checks pins,
/// which live in files.
pub fn standing(records: &[Record], now: Timestamp) -> Vec<Standing> {
    let caller_side: Vec<(&Record, &crate::placed::PlacedRecord)> = records
        .iter()
        .filter_map(|record| {
            let placed = record.placed.as_ref()?;
            (placed.side == Side::Caller).then_some((record, placed))
        })
        .collect();
    let passed_later = |hit: &Record, placed: &crate::placed::PlacedRecord| {
        hit.label.is_some()
            && caller_side.iter().any(|(later, other)| {
                later.ended_at > hit.ended_at
                    && other.member == placed.member
                    && other.caller_workspace == placed.caller_workspace
                    && later.label == hit.label
                    && later.outcome == Some(Outcome::Exited { exit_code: 0 })
            })
    };
    let mut latest: BTreeMap<(String, Manager, ToolName), Standing> = BTreeMap::new();
    for (record, placed) in &caller_side {
        let (Some(missing), Some(ended_at)) = (&placed.missing_tool, record.ended_at) else {
            continue;
        };
        if now.since(ended_at) > STANDS_FOR || passed_later(record, placed) {
            continue;
        }
        let key = (placed.member.clone(), missing.manager, missing.tool.clone());
        if latest.get(&key).is_none_or(|kept| kept.ended_at < ended_at) {
            latest.insert(
                key,
                Standing {
                    run_id: record.run_id.clone(),
                    member: placed.member.clone(),
                    missing: missing.clone(),
                    workspace: placed.caller_workspace.clone(),
                    ended_at,
                },
            );
        }
    }
    latest.into_values().collect()
}

#[cfg(test)]
mod tests {
    use crate::fixtures::{Run, placed};
    use crate::missing_tool::*;

    fn tool(name: &str) -> MissingTool {
        MissingTool {
            manager: Manager::Mise,
            tool: ToolName::new(name).unwrap(),
            called: None,
        }
    }

    /// A run on `devbox`, `n` seconds into the period, that exited `code`.
    fn run(n: u64, label: &'static str, code: i32, missing: Option<&str>) -> Record {
        let mut record = placed(Run {
            n,
            label,
            member: "devbox",
            ..Run::default()
        });
        record.ended_at = Some(record.requested_at.after(Duration::from_secs(5)));
        record.outcome = Some(Outcome::Exited { exit_code: code });
        record.placed.as_mut().unwrap().missing_tool = missing.map(tool);
        record
    }

    #[test]
    fn tool_names_are_validated_and_never_credential_shaped() {
        for name in [
            "node",
            "xcodebuild",
            "sentry-cli",
            "npm:@scope/pkg",
            "python3.10",
        ] {
            assert_eq!(ToolName::new(name).unwrap().as_str(), name);
        }
        let token = format!("gh{}", "p_aB3dE5fG7hJ9".repeat(3));
        for refused in [
            "",
            "-rf",
            "../bin/x",
            "a b",
            "caf\u{e9}",
            "x;rm",
            token.as_str(),
            &"a".repeat(65),
        ] {
            assert!(ToolName::new(refused).is_none(), "{refused}");
        }
        assert!(serde_json::from_str::<ToolName>(&format!("{token:?}")).is_err());
    }

    #[test]
    fn the_latest_hit_stands_until_a_later_run_of_the_job_passes_or_it_ages() {
        let now = run(100, "test", 0, None).ended_at.unwrap();
        let records = [
            run(1, "test", 127, Some("bun")),
            run(2, "test", 127, Some("bun")),
            run(3, "lint", 1, Some("swiftlint")),
        ];
        let found = standing(&records, now);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].missing.tool.as_str(), "bun");
        assert_eq!(found[0].run_id, records[1].run_id, "the latest hit");
        assert_eq!(found[0].workspace.as_deref(), Some("/w/app"));

        // Another job passing says nothing about this one.
        let mut later = records.to_vec();
        later.push(run(4, "build", 0, None));
        assert_eq!(standing(&later, now).len(), 2);
        // The same job passing on that member clears it.
        later.push(run(5, "test", 0, None));
        let left = standing(&later, now);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].missing.tool.as_str(), "swiftlint");

        let a_week_on = now.after(STANDS_FOR + Duration::from_secs(60));
        assert!(standing(&records, a_week_on).is_empty(), "aged out");

        // Another job that hit the same tool still stands when this one clears.
        let mut both = vec![
            run(1, "lint", 1, Some("bun")),
            run(2, "test", 127, Some("bun")),
        ];
        both.push(run(3, "test", 0, None));
        let left = standing(&both, now);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].run_id, both[0].run_id);

        // An unlabelled run passing says nothing about an unlabelled one failing.
        let mut unlabelled = vec![run(1, "x", 127, Some("tuist")), run(2, "x", 0, None)];
        for record in &mut unlabelled {
            record.label = None;
        }
        assert_eq!(standing(&unlabelled, now).len(), 1);
    }

    #[test]
    fn a_stored_name_a_later_build_refuses_reads_as_none() {
        let mut record = run(1, "test", 127, Some("bun"));
        let mut value = serde_json::to_value(&record).unwrap();
        value["placed"]["missing_tool"]["tool"] = "not a tool".into();
        let read: Record = serde_json::from_value(value).unwrap();
        assert_eq!(read.placed.unwrap().missing_tool, None);
        record.placed.as_mut().unwrap().missing_tool = None;
        let value = serde_json::to_value(&record).unwrap();
        assert!(value["placed"].get("missing_tool").is_none());
        let read: Record = serde_json::from_value(value).unwrap();
        assert_eq!(read.placed.unwrap().missing_tool, None);
    }
}
