//! Placement by pins: a placeable request goes to a member that has the tools
//! its workspace pins when one can start it in time; a member lacking some is
//! used only when none that has them can, and the run then carries a warning.
//! See `docs/fleet-provisioning.md`, "Placement by pins".
//!
//! What each member lacks comes from the master's provisioning loop through
//! [`crate::placed::Members::lacking`]; a member it knows nothing about lacks
//! nothing here, so placement is never worse off than without it.
use crate::placement::{self, Observation, Unreachable, Verdict, Wanted};
use std::collections::BTreeMap;

/// At most this many tools are named for a member; the rest are counted.
const NAMED: usize = 3;

/// The members that lack tools a workspace pins, each with what it lacks, as
/// `node 22.19.0`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lacking(pub BTreeMap<String, Vec<String>>);

impl Lacking {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn of(&self, member: &str) -> Option<&Vec<String>> {
        self.0.get(member).filter(|tools| !tools.is_empty())
    }

    /// What `member` lacks, in a sentence's words: `node 22.19.0 and bun 1.3.5`.
    fn named(tools: &[String]) -> String {
        let mut named: Vec<&str> = tools.iter().take(NAMED).map(String::as_str).collect();
        let more = tools.len().saturating_sub(NAMED);
        let rest = (more > 0).then(|| format!("{more} more"));
        if let Some(rest) = &rest {
            named.push(rest);
        }
        match named.as_slice() {
            [] => String::new(),
            [one] => (*one).to_owned(),
            [init @ .., last] => format!("{} and {last}", init.join(", ")),
        }
    }

    /// Why placement passed `member` over.
    fn why(&self, member: &str) -> Option<String> {
        self.of(member)
            .map(|tools| format!("It lacks {}, which the workspace pins.", Self::named(tools)))
    }

    /// What `member` lacks, and what that means for a run there.
    fn lacks(&self, member: &str) -> Option<String> {
        self.of(member).map(|tools| {
            format!(
                "{member} lacks {}, which this workspace pins, so the run may fail there; the master is installing them.",
                Self::named(tools)
            )
        })
    }

    /// The warning a run placed on `member` because no member that has the
    /// pinned tools could start it in time carries.
    pub fn fallback(&self, member: &str) -> Option<String> {
        self.lacks(member).map(|lacks| {
            format!(
                "{lacks} Elsecall placed it there because no member that has them could start it in time."
            )
        })
    }

    /// The warning a run that named `member` carries.
    pub fn named_member(&self, member: &str) -> Option<String> {
        self.lacks(member)
            .map(|lacks| format!("{lacks} It runs there because it was asked to."))
    }
}

/// Scores the candidates as [`placement::decide`] does, first without the
/// members that lack what the workspace pins, then, only if none of the rest
/// can start the request in time, with them, saying so in the reason. The
/// calling member is never passed over: the work comes from there.
pub fn decide(
    observations: Vec<Observation>,
    unreachable: Vec<Unreachable>,
    wanted: &Wanted,
    lacking: &Lacking,
) -> Verdict {
    let passed_over: Vec<Unreachable> = observations
        .iter()
        .filter(|observation| !observation.caller)
        .filter_map(|observation| {
            let member = &observation.state.member;
            lacking.why(member).map(|why| Unreachable {
                peer_io: None,
                member: member.clone(),
                why,
            })
        })
        .collect();
    if passed_over.is_empty() {
        return placement::decide(observations, unreachable, wanted);
    }
    let fitting: Vec<Observation> = observations
        .iter()
        .filter(|observation| {
            !passed_over
                .iter()
                .any(|passed| passed.member == observation.state.member)
        })
        .cloned()
        .collect();
    let mut preferred = placement::decide(fitting, unreachable.clone(), wanted);
    if preferred.winner.is_some() {
        preferred.reason += &placement::unreachable_left_out(&passed_over);
        preferred.passed_over = passed_over;
        return preferred;
    }
    let mut fallback = placement::decide(observations, unreachable, wanted);
    if let Some(warning) = fallback
        .winner
        .as_deref()
        .and_then(|winner| lacking.fallback(winner))
    {
        fallback.reason = format!("{} {warning}", fallback.reason);
    }
    fallback
}

#[cfg(test)]
mod tests {
    use crate::{
        pin_fit::*,
        placement::{
            Copy,
            tests::{defaults, member, warm},
        },
    };
    use elsecall_contract::scheduling_model::capacity::Cores;
    use std::time::Duration;

    fn bravo_lacks_node() -> Lacking {
        Lacking(BTreeMap::from([(
            "bravo".to_owned(),
            vec!["node 22.19.0".to_owned()],
        )]))
    }

    fn verdict(observations: Vec<Observation>, hold_left: u64) -> Verdict {
        decide(
            observations,
            Vec::new(),
            &Wanted {
                size: Cores::from_f64(2.0).unwrap(),
                hold_left: Duration::from_secs(hold_left),
                parameters: &defaults(),
            },
            &bravo_lacks_node(),
        )
    }

    #[test]
    fn a_member_with_the_pinned_tools_is_preferred() {
        // The caller is busy; bravo is faster than charlie but lacks node.
        let chosen = verdict(
            vec![
                member("mini", true, 0.0, 1.0, Copy::Caller),
                member("bravo", false, 10.0, 4.0, warm()),
                member("charlie", false, 10.0, 1.0, warm()),
            ],
            600,
        );
        assert_eq!(
            chosen.winner.as_deref(),
            Some("charlie"),
            "{}",
            chosen.reason
        );
        assert!(
            chosen
                .reason
                .contains("bravo was left out: it lacks node 22.19.0"),
            "{}",
            chosen.reason
        );
        assert!(chosen.unreachable.is_empty(), "bravo answered");
        assert_eq!(chosen.passed_over[0].member, "bravo");
    }

    #[test]
    fn a_member_lacking_them_is_used_only_when_no_other_can_start_in_time() {
        let fallback = verdict(
            vec![
                member("mini", true, 0.0, 1.0, Copy::Caller),
                member("bravo", false, 10.0, 1.0, warm()),
            ],
            30,
        );
        assert_eq!(
            fallback.winner.as_deref(),
            Some("bravo"),
            "{}",
            fallback.reason
        );
        assert!(
            fallback.reason.contains("bravo lacks node 22.19.0"),
            "{}",
            fallback.reason
        );
        let nothing_lacking = decide(
            vec![member("bravo", false, 10.0, 1.0, warm())],
            Vec::new(),
            &Wanted {
                size: Cores::from_f64(2.0).unwrap(),
                hold_left: Duration::from_secs(600),
                parameters: &defaults(),
            },
            &Lacking::default(),
        );
        assert!(!nothing_lacking.reason.contains("lacks"));
    }

    #[test]
    fn what_a_member_lacks_reads_as_a_sentence() {
        let lacking = Lacking(BTreeMap::from([
            ("bravo".to_owned(), vec!["node 22.19.0".to_owned()]),
            (
                "charlie".to_owned(),
                ["a 1", "b 2", "c 3", "d 4", "e 5"]
                    .map(str::to_owned)
                    .to_vec(),
            ),
            ("delta".to_owned(), Vec::new()),
        ]));
        assert_eq!(
            lacking.why("bravo").unwrap(),
            "It lacks node 22.19.0, which the workspace pins."
        );
        assert!(
            lacking
                .fallback("charlie")
                .unwrap()
                .starts_with("charlie lacks a 1, b 2, c 3 and 2 more, which this workspace pins")
        );
        assert_eq!(
            lacking.fallback("delta"),
            None,
            "an empty list lacks nothing"
        );
        assert_eq!(lacking.fallback("echo"), None);
        assert!(
            lacking
                .named_member("bravo")
                .unwrap()
                .ends_with("It runs there because it was asked to.")
        );
    }
}
