//! Declared outputs, or paths inside them, that a placed run did not bring back,
//! and why. Both members record them; the protocol carries them grouped by output
//! and cause, so a directory of thousands of links is one entry.
use crate::workspace::{
    outputs::{Changed, sanitized},
    path::WorkspacePath,
};
use serde::{Deserialize, Serialize};
use std::io;

/// Why a path was not returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// It disappeared while the snapshot at exit was being taken.
    Vanished,
    /// It could not be read on the member.
    Unreadable,
    /// A directory on its way is a link, which would lead outside the workspace.
    OutsideWorkspace,
    /// A link or special file, which is never returned.
    Link,
    /// Returning it would exceed what Elsecall returns from one run.
    TooLarge,
    /// Sending, receiving or placing it failed.
    TransferError,
}

impl Cause {
    fn reason(self, many: bool) -> &'static str {
        match (self, many) {
            (Cause::Vanished, false) => "it vanished while the snapshot at exit was taken",
            (Cause::Vanished, true) => "they vanished while the snapshot at exit was taken",
            (Cause::Unreadable, false) => "it could not be read on the member",
            (Cause::Unreadable, true) => "they could not be read on the member",
            (Cause::OutsideWorkspace, _) => {
                "a directory on its way is a link, which leads outside the workspace"
            }
            (Cause::Link, false) => "it is a link or special file, which is never returned",
            (Cause::Link, true) => "they are links or special files, which are never returned",
            (Cause::TooLarge, _) => "the run's outputs would exceed what Elsecall returns",
            (Cause::TransferError, _) => "the transfer failed",
        }
    }
}

/// The longest detail kept, such as an I/O error's text.
const DETAIL: usize = 200;
/// At most one entry per declared output and cause.
pub const MAXIMUM: usize = crate::workspace::outputs::MAXIMUM_DECLARED * 6;

/// The first path of one output that was not returned for one cause, and how many
/// more of that output were not returned for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotReturned {
    pub path: WorkspacePath,
    pub cause: Cause,
    #[serde(default)]
    pub others: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl NotReturned {
    /// The record's warning, made safe to show even when the member wrote it.
    pub fn warning(&self) -> String {
        let path = sanitized(self.path.as_str(), 200);
        let subject = match self.others {
            0 => format!("{path} was not returned"),
            n => format!("{path} and {n} more were not returned"),
        };
        let reason = self.cause.reason(self.others > 0);
        match &self.detail {
            Some(detail) => format!("{subject}: {reason} ({}).", sanitized(detail, DETAIL)),
            None => format!("{subject}: {reason}."),
        }
    }
}

/// Paths not returned, grouped by declared output and cause.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shortfall {
    groups: Vec<(WorkspacePath, NotReturned)>,
}

impl Shortfall {
    /// Notes that `path`, inside the declared `output`, was not returned.
    pub fn add(
        &mut self,
        output: &WorkspacePath,
        path: &WorkspacePath,
        cause: Cause,
        detail: Option<String>,
    ) {
        if let Some((_, group)) = self
            .groups
            .iter_mut()
            .find(|(o, group)| o == output && group.cause == cause)
        {
            group.others = group.others.saturating_add(1);
            return;
        }
        self.groups.push((
            output.clone(),
            NotReturned {
                path: path.clone(),
                cause,
                others: 0,
                detail: detail.map(|d| d.chars().take(DETAIL).collect()),
            },
        ));
    }

    pub fn add_error(
        &mut self,
        output: &WorkspacePath,
        path: &WorkspacePath,
        cause: Cause,
        error: &io::Error,
    ) {
        self.add(output, path, cause, Some(error.to_string()));
    }

    /// Takes in entries another member reported, already checked by [`accept`].
    pub fn extend(&mut self, declared: &[WorkspacePath], entries: Vec<NotReturned>) {
        for entry in entries {
            let Some(output) = declared.iter().find(|o| entry.path.is_within(o)) else {
                continue;
            };
            match self
                .groups
                .iter_mut()
                .find(|(o, group)| o == output && group.cause == entry.cause)
            {
                Some((_, group)) => {
                    group.others = group.others.saturating_add(1).saturating_add(entry.others)
                }
                None => self.groups.push((output.clone(), entry)),
            }
        }
    }

    /// Notes the links and special files a description lists: never returned.
    pub fn add_skipped(&mut self, changes: &[Changed]) {
        for change in changes {
            for path in change.content.iter().flat_map(|c| &c.skipped) {
                self.add(&change.output, path, Cause::Link, None);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn entries(&self) -> Vec<NotReturned> {
        self.groups.iter().map(|(_, group)| group.clone()).collect()
    }

    pub fn warnings(&self) -> Vec<String> {
        self.groups
            .iter()
            .map(|(_, group)| group.warning())
            .collect()
    }
}

/// What a member records about outputs that did not come back: what it knows
/// itself, until the caller reports the whole of it.
#[derive(Debug, Default)]
pub struct Account {
    own: Shortfall,
    reported: Option<Vec<NotReturned>>,
    /// Why nothing came back at all, from this member or the caller.
    nothing: Option<String>,
}

impl Account {
    pub fn new(own: Shortfall) -> Self {
        Self {
            own,
            ..Self::default()
        }
    }

    /// No outputs could be offered, for `reason`.
    pub fn unavailable(reason: String) -> Self {
        Self {
            nothing: Some(reason),
            ..Self::default()
        }
    }

    /// Notes a path this member could not send.
    pub fn add(
        &mut self,
        output: &WorkspacePath,
        path: &WorkspacePath,
        cause: Cause,
        detail: Option<String>,
    ) {
        self.own.add(output, path, cause, detail);
    }

    /// What the caller reports; it replaces this member's own view. Entries
    /// outside the declared outputs make the report count for nothing.
    pub fn reported(
        &mut self,
        declared: &[WorkspacePath],
        entries: Vec<NotReturned>,
        nothing: Option<String>,
    ) {
        if accept(&entries, declared).is_ok() {
            self.reported = Some(entries);
        }
        if let Some(reason) = nothing {
            self.nothing = Some(sanitized(&reason, DETAIL));
        }
    }

    pub fn return_reported(&self) -> bool {
        self.reported.is_some()
    }

    pub fn warnings(self, declared: &[WorkspacePath]) -> Vec<String> {
        let mut warnings: Vec<String> = self
            .nothing
            .iter()
            .map(|reason| format!("Outputs were not returned: {reason}."))
            .collect();
        let shortfall = match self.reported {
            Some(reported) => {
                let mut shortfall = Shortfall::default();
                shortfall.extend(declared, reported);
                shortfall
            }
            None => self.own,
        };
        warnings.extend(shortfall.warnings());
        warnings
    }
}

/// Checks entries the other member sent: each inside a declared output, bounded.
pub fn accept(entries: &[NotReturned], declared: &[WorkspacePath]) -> Result<(), String> {
    if entries.len() > MAXIMUM {
        return Err("the member listed too many outputs that were not returned".into());
    }
    if entries
        .iter()
        .any(|entry| !declared.iter().any(|output| entry.path.is_within(output)))
    {
        return Err(
            "the member listed an output that was not returned outside the declared ones".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> WorkspacePath {
        WorkspacePath::parse(text).unwrap()
    }

    #[test]
    fn groups_by_output_and_cause_and_reads_as_warnings() {
        let mut shortfall = Shortfall::default();
        let logs = path("build/logs");
        shortfall.add(&logs, &path("build/logs/a"), Cause::Vanished, None);
        shortfall.add(&logs, &path("build/logs/b"), Cause::Vanished, None);
        shortfall.add(
            &logs,
            &path("build/logs/c"),
            Cause::Unreadable,
            Some("Permission denied\n(os error 13)".into()),
        );
        shortfall.add(&path("out.txt"), &path("out.txt"), Cause::TooLarge, None);
        assert_eq!(
            shortfall.warnings(),
            [
                "build/logs/a and 1 more were not returned: they vanished while the snapshot at exit was taken.",
                "build/logs/c was not returned: it could not be read on the member (Permission denied\\n(os error 13)).",
                "out.txt was not returned: the run's outputs would exceed what Elsecall returns.",
            ]
        );

        let mut caller = Shortfall::default();
        caller.add(&logs, &path("build/logs/d"), Cause::Vanished, None);
        caller.extend(&[logs.clone(), path("out.txt")], shortfall.entries());
        assert_eq!(caller.entries()[0].others, 2);
        assert_eq!(caller.entries().len(), 3);
    }

    #[test]
    fn a_member_records_the_callers_report_over_its_own_view() {
        let declared = [path("logs")];
        let mut own = Shortfall::default();
        own.add(&declared[0], &path("logs/a"), Cause::Unreadable, None);
        let account = Account::new(own.clone());
        assert_eq!(
            account.warnings(&declared),
            ["logs/a was not returned: it could not be read on the member."]
        );

        let mut account = Account::new(own.clone());
        let mut caller = own.clone();
        caller.add(&declared[0], &path("logs/b"), Cause::TransferError, None);
        account.reported(&declared, caller.entries(), None);
        assert_eq!(account.warnings(&declared).len(), 2);

        let mut account = Account::new(own);
        account.reported(
            &declared,
            Vec::new(),
            Some("the caller\nwas interrupted".into()),
        );
        assert_eq!(
            account.warnings(&declared),
            ["Outputs were not returned: the caller\\nwas interrupted."]
        );
    }

    #[test]
    fn refuses_entries_outside_the_declared_outputs() {
        let declared = [path("build/logs")];
        let entry = |p: &str| NotReturned {
            path: path(p),
            cause: Cause::Link,
            others: 0,
            detail: None,
        };
        assert!(accept(&[entry("build/logs/x")], &declared).is_ok());
        assert!(accept(&[entry("build/other")], &declared).is_err());
        assert!(accept(&vec![entry("build/logs"); MAXIMUM + 1], &declared).is_err());
    }
}
