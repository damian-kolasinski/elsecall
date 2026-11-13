//! What the master last heard from each other member: its quote, and each copy it
//! reported, kept apart so that answers about different workspaces, asked at
//! different times, merge rather than erase one another. An answer older than
//! the one kept is ignored.
use crate::{
    failure::Failure,
    scheduling::{
        messages::MemberState,
        quote::{CopyQuote, HeldWork, Quote},
    },
};
use std::{collections::HashMap, time::Duration};
use tokio::time::Instant;

#[derive(Default)]
pub(super) struct Quotes {
    members: HashMap<String, Heard>,
}

struct Heard {
    /// When the request this answer answers was sent.
    asked_at: Instant,
    /// The member's state, without copies; or why it could not be asked.
    answer: Result<Quote, Failure>,
    copies: HashMap<String, (Instant, CopyQuote)>,
}

impl Quotes {
    /// Keeps `member`'s answer to a request sent at `asked_at`.
    pub fn record(&mut self, member: &str, asked_at: Instant, answer: Result<Quote, Failure>) {
        let heard = self
            .members
            .entry(member.to_owned())
            .or_insert_with(|| Heard {
                asked_at,
                answer: Err(Failure::new("not_asked", "Not asked yet.", "")),
                copies: HashMap::new(),
            });
        let answer = answer.map(|mut quote| {
            for copy in std::mem::take(&mut quote.copies) {
                let newer = heard
                    .copies
                    .get(&copy.workspace)
                    .is_none_or(|(at, _)| *at <= asked_at);
                if newer {
                    heard
                        .copies
                        .insert(copy.workspace.clone(), (asked_at, copy));
                }
            }
            quote
        });
        if heard.asked_at <= asked_at || heard.answer.is_err() && answer.is_ok() {
            heard.asked_at = asked_at;
            heard.answer = answer;
        }
    }

    /// Forgets members no longer in the fleet.
    pub fn retain(&mut self, members: &[String]) {
        self.members.retain(|member, _| members.contains(member));
    }

    pub fn answer(&self, member: &str) -> Option<Result<&Quote, &Failure>> {
        self.members.get(member).map(|h| h.answer.as_ref())
    }

    pub fn copy(&self, member: &str, workspace: &str) -> Option<&CopyQuote> {
        self.members
            .get(member)?
            .copies
            .get(workspace)
            .map(|(_, copy)| copy)
    }

    /// Whether asking `member` failed within `within`.
    pub fn failed_recently(&self, member: &str, within: Duration) -> bool {
        self.members
            .get(member)
            .is_some_and(|h| h.answer.is_err() && h.asked_at.elapsed() < within)
    }

    /// Takes in the state `member` reported while answering an offer; an admitted
    /// request now holds its capacity.
    pub fn heard(&mut self, member: &str, state: &MemberState, admitted: Option<HeldWork>) {
        if let Some(Heard {
            answer: Ok(quote), ..
        }) = self.members.get_mut(member)
        {
            let mut state = state.clone();
            state.member = member.to_owned();
            quote.state = state;
            quote.holding.extend(admitted);
        }
    }

    /// `member` could not be reached while offering it work.
    pub fn lost(&mut self, member: &str, failure: Failure) {
        self.record(member, Instant::now(), Err(failure));
    }

    /// Work running on each member that answered.
    pub fn holding(&self) -> impl Iterator<Item = (&str, &HeldWork)> {
        let mut holding: Vec<_> = self
            .members
            .iter()
            .flat_map(|(member, heard)| {
                heard
                    .answer
                    .as_ref()
                    .map(|q| q.holding.iter())
                    .into_iter()
                    .flatten()
                    .map(move |work| (member.as_str(), work))
            })
            .collect();
        holding.sort_by(|(am, a), (bm, b)| am.cmp(bm).then(a.run_id.cmp(&b.run_id)));
        holding.into_iter()
    }

    /// The measured speed of each member that answered with one.
    pub fn speeds(&self) -> impl Iterator<Item = (&str, f64)> {
        self.members.iter().filter_map(|(member, heard)| {
            let speed = heard.answer.as_ref().ok()?.state.speed?;
            Some((member.as_str(), speed))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduling::{
        admission::QueueCounts,
        capacity::{Capacity, Cores},
        load::Gate,
        placed::Warmth,
    };

    fn quote(free: u32, copies: &[(&str, Warmth)]) -> Quote {
        Quote {
            state: MemberState {
                member: "devbox".into(),
                capacity: Capacity {
                    weighted_cores: Cores::from_millis(9000),
                    reserved: Cores::ZERO,
                    capacity: Cores::from_millis(9000),
                },
                in_use: Cores::ZERO,
                free: Cores::from_millis(free * 1000),
                queued: QueueCounts::default(),
                load: None,
                gate: Gate::Open,
                speed: None,
            },
            utilization: None,
            holding: Vec::new(),
            waiting: Vec::new(),
            admission: None,
            copies: copies
                .iter()
                .map(|(w, warmth)| CopyQuote {
                    workspace: (*w).into(),
                    warmth: *warmth,
                    busy: false,
                    unavailable: false,
                })
                .collect(),
        }
    }

    #[test]
    fn answers_merge_by_workspace_and_older_answers_lose() {
        let mut quotes = Quotes::default();
        let early = Instant::now() - Duration::from_millis(20);
        let late = early + Duration::from_millis(10);
        quotes.record("devbox", late, Ok(quote(9, &[("/a", Warmth::Warm)])));
        // An earlier request about another workspace finishes later.
        quotes.record(
            "devbox",
            early,
            Ok(quote(1, &[("/b", Warmth::Cold), ("/a", Warmth::Cold)])),
        );
        assert_eq!(quotes.copy("devbox", "/a").unwrap().warmth, Warmth::Warm);
        assert_eq!(quotes.copy("devbox", "/b").unwrap().warmth, Warmth::Cold);
        assert_eq!(
            quotes.answer("devbox").unwrap().unwrap().state.free,
            Cores::from_millis(9000),
            "the older state does not replace the newer"
        );
        quotes.lost("devbox", Failure::new("member_unavailable", "gone", ""));
        assert!(quotes.failed_recently("devbox", Duration::from_secs(10)));
        assert_eq!(
            quotes.copy("devbox", "/a").unwrap().warmth,
            Warmth::Warm,
            "copies outlive a failed answer"
        );
    }
}
