//! Which copies a housekeeping pass removes, and in which order disk pressure
//! evicts them. Pure decisions over what the pass observed.
use crate::{
    housekeeping::{catalog::Reason, settings::Settings},
    scheduling::timestamp::Timestamp,
};
use serde::Serialize;
use std::path::PathBuf;

/// What the calling member said about a copy's workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallerSays {
    /// The workspace still exists.
    Present,
    /// It no longer exists, as of the time the member asked.
    Gone { asked_at: Timestamp },
    /// The caller does not know the workspace, or did not answer.
    Unknown,
}

/// A copy as a pass sees it.
#[derive(Clone, Debug)]
pub struct Observed {
    pub root: PathBuf,
    pub last_used: Timestamp,
    /// A run uses it, or one was admitted and is about to attach.
    pub held: bool,
    pub caller: CallerSays,
}

/// Why a copy that would otherwise go stays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Spared {
    /// A run uses it or is about to attach; the next pass judges it again.
    InUse,
    /// Another copy lives inside its directory, as same-path copies can.
    ContainsCopy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Keep,
    Remove(Reason),
    /// It would be removed for this reason, but must not be now.
    Spare(Reason, Spared),
}

/// The verdict on one copy, before disk pressure.
pub fn judge(copy: &Observed, now: Timestamp, settings: &Settings) -> Verdict {
    let reason = match copy.caller {
        // A use after the question means the workspace was used since, perhaps
        // recreated: the answer no longer speaks for it.
        CallerSays::Gone { asked_at }
            if settings.remove_deleted_workspaces && copy.last_used < asked_at =>
        {
            Some(Reason::CallerDeleted)
        }
        _ if now.since(copy.last_used) >= settings.idle_after => Some(Reason::Idle),
        _ => None,
    };
    match (reason, copy.held) {
        (None, _) => Verdict::Keep,
        (Some(reason), false) => Verdict::Remove(reason),
        (Some(reason), true) => Verdict::Spare(reason, Spared::InUse),
    }
}

/// The copies disk pressure may evict, least recently used first. Copies in use
/// are never evicted.
pub fn eviction_order(copies: &[Observed]) -> Vec<PathBuf> {
    let mut idle: Vec<&Observed> = copies.iter().filter(|c| !c.held).collect();
    idle.sort_by(|a, b| a.last_used.cmp(&b.last_used).then(a.root.cmp(&b.root)));
    idle.into_iter().map(|c| c.root.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const DAY: u64 = 24 * 60 * 60 * 1000;

    fn copy(name: &str, last_used: u64, held: bool, caller: CallerSays) -> Observed {
        Observed {
            root: name.into(),
            last_used: Timestamp::from_millis(last_used),
            held,
            caller,
        }
    }

    #[test]
    fn idle_copies_go_exactly_at_the_idle_period() {
        let settings = Settings::default();
        let used = 100 * DAY;
        let fresh = copy("a", used, false, CallerSays::Present);
        let just_before = Timestamp::from_millis(used + 14 * DAY - 1);
        let at = Timestamp::from_millis(used + 14 * DAY);
        assert_eq!(judge(&fresh, just_before, &settings), Verdict::Keep);
        assert_eq!(judge(&fresh, at, &settings), Verdict::Remove(Reason::Idle));
        let held = copy("a", used, true, CallerSays::Present);
        assert_eq!(
            judge(&held, at, &settings),
            Verdict::Spare(Reason::Idle, Spared::InUse)
        );
        let longer = Settings {
            idle_after: Duration::from_secs(30 * 24 * 60 * 60),
            ..Settings::default()
        };
        assert_eq!(judge(&fresh, at, &longer), Verdict::Keep);
    }

    #[test]
    fn a_deleted_workspace_goes_unless_used_since_it_was_asked_about() {
        let settings = Settings::default();
        let now = Timestamp::from_millis(DAY);
        let asked = Timestamp::from_millis(DAY - 1000);
        let gone = copy("a", DAY - 5000, false, CallerSays::Gone { asked_at: asked });
        assert_eq!(
            judge(&gone, now, &settings),
            Verdict::Remove(Reason::CallerDeleted)
        );
        let used_since = copy("a", DAY - 500, false, CallerSays::Gone { asked_at: asked });
        assert_eq!(judge(&used_since, now, &settings), Verdict::Keep);
        let in_use = copy("a", DAY - 5000, true, CallerSays::Gone { asked_at: asked });
        assert_eq!(
            judge(&in_use, now, &settings),
            Verdict::Spare(Reason::CallerDeleted, Spared::InUse)
        );
        let off = Settings {
            remove_deleted_workspaces: false,
            ..Settings::default()
        };
        assert_eq!(judge(&gone, now, &off), Verdict::Keep);
        for answer in [CallerSays::Present, CallerSays::Unknown] {
            assert_eq!(
                judge(&copy("a", DAY - 5000, false, answer), now, &settings),
                Verdict::Keep
            );
        }
    }

    #[test]
    fn eviction_takes_the_least_recently_used_idle_copies_first() {
        let copies = [
            copy("newest", 30, false, CallerSays::Present),
            copy("busy-oldest", 1, true, CallerSays::Present),
            copy("oldest", 10, false, CallerSays::Present),
            copy("middle", 20, false, CallerSays::Unknown),
        ];
        assert_eq!(
            eviction_order(&copies),
            vec![
                PathBuf::from("oldest"),
                PathBuf::from("middle"),
                PathBuf::from("newest")
            ]
        );
    }
}
