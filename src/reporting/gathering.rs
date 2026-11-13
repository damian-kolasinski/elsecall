//! Making a report: ask every member for its history of the period, at once,
//! and compute the report from what they answer. Members are reached through
//! the [`MemberHistory`] port: this member's own records and load, and other
//! members' over the member channel.
use crate::{
    failure::{Failure, Result},
    load_log::buckets::Bucket,
    reporting::{
        document::{self, Document, Gathered, MemberInput, Query, RecordsRead},
        facts::Fact,
        leftovers::LeftoverFact,
        window::{Window, invalid},
    },
};
use futures_util::future::BoxFuture;
use std::{collections::BTreeSet, sync::Arc, time::Duration};

/// The most requests one report reads from one member.
pub const MAXIMUM_FACTS: usize = 50_000;

/// A member's requests of a period.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub facts: Vec<Fact>,
    /// The leftovers of placed runs the member hosted, from the same records.
    pub leftovers: Vec<LeftoverFact>,
    /// The period held more than [`MAXIMUM_FACTS`]; these are the latest.
    pub truncated: bool,
}

/// One member's history, as a report reads it.
pub trait MemberHistory: Send + Sync {
    fn peer_io(
        &self,
        _window: Window,
    ) -> BoxFuture<'static, Result<crate::peer_diagnostics::History>> {
        Box::pin(async { Err(invalid("This member does not support peer I/O history.")) })
    }

    /// The member's outside-pressure history of `window`: what held its gate
    /// closed, and its early warnings. A source that keeps none answers that it
    /// cannot say.
    fn outside(
        &self,
        _window: Window,
    ) -> BoxFuture<'static, Result<crate::reporting::outside::OutsideHistory>> {
        Box::pin(async {
            Err(invalid(
                "This member does not support outside-pressure history.",
            ))
        })
    }

    /// Privacy-preserving trace page, including retained carry-in work.
    fn trace_page(
        &self,
        _window: Window,
        _before: Option<crate::report_protocol::Cursor>,
    ) -> BoxFuture<'static, Result<crate::workload::trace::Page>> {
        Box::pin(async {
            Err(invalid(
                "This history source does not support workload traces.",
            ))
        })
    }
    /// The requests made on the member in `window`, at most [`MAXIMUM_FACTS`].
    fn facts(&self, window: Window) -> BoxFuture<'static, Result<Facts>>;
    /// The member's load in `window`, in aligned buckets of `width`.
    fn load(&self, window: Window, width: Duration) -> BoxFuture<'static, Result<Vec<Bucket>>>;
}

/// A member and how to read its history.
#[derive(Clone)]
pub struct Source {
    pub member: String,
    pub history: Arc<dyn MemberHistory>,
}

/// The fleet a report covers.
#[derive(Clone)]
pub struct Fleet {
    pub this_member: String,
    pub master: Option<String>,
    /// Why the fleet's members could not be listed, when this member is
    /// enrolled; the report then covers this member alone.
    pub fleet_error: Option<Failure>,
    /// Every member, this one first.
    pub sources: Vec<Source>,
}

/// The report `query` asks for, from every member that answers. A member that
/// cannot is named in the report; the rest of it stands.
pub async fn report(fleet: &Fleet, query: &Query) -> Result<Document> {
    if let Some(member) = &query.member
        && !fleet.sources.iter().any(|s| &s.member == member)
    {
        let names: Vec<&str> = fleet.sources.iter().map(|s| s.member.as_str()).collect();
        return Err(invalid(format!(
            "--member {member:?} is not a member of this fleet; its members are {}.",
            names.join(", ")
        )));
    }
    if let Some(label) = &query.label
        && (label.is_empty()
            || label.chars().count() > crate::scheduling::request::MAXIMUM_LABEL
            || label.chars().any(char::is_control))
    {
        return Err(invalid(
            "--label must be a pattern of 1 to 200 characters without control characters.",
        ));
    }
    let gathered = gather(fleet, query.window).await;
    // Computing it, the backtest above all, is CPU work: off the async runtime
    // that also serves admission.
    let query = query.clone();
    Ok(
        tokio::task::spawn_blocking(move || document::document(&query, &gathered))
            .await
            .unwrap_or_else(|error| std::panic::resume_unwind(error.into_panic())),
    )
}

/// What every member answers for `window`, each request counted once: by the
/// member it was made on, which also reports where it ran. A request another
/// member sends must have run on a member of the fleet, if it ran; this
/// member's own are trusted. A run ID already counted, as from this member,
/// which is asked first, is not counted again.
pub async fn gather(fleet: &Fleet, window: Window) -> Gathered {
    let names: BTreeSet<&str> = fleet.sources.iter().map(|s| s.member.as_str()).collect();
    let width = window.fine_width();
    let answers = futures_util::future::join_all(fleet.sources.iter().map(|source| {
        let facts = source.history.facts(window);
        let load = source.history.load(window, width);
        let outside = source.history.outside(window);
        futures_util::future::join3(facts, load, outside)
    }))
    .await;
    let mut seen = BTreeSet::new();
    let mut facts = Vec::new();
    let mut seen_leftovers = BTreeSet::new();
    let mut leftovers = Vec::new();
    let mut members: Vec<MemberInput> = fleet
        .sources
        .iter()
        .zip(answers)
        .map(|(source, (answered, load, outside))| {
            let records = answered.map(|answered| {
                let mut read = RecordsRead {
                    requests: answered.facts.len(),
                    truncated: answered.truncated,
                    dropped: 0,
                };
                let own = source.member == fleet.this_member;
                // A member reports the leftovers it hosted, each run's once.
                for fact in answered.leftovers {
                    if fact.member == source.member && seen_leftovers.insert(fact.run_id.clone()) {
                        leftovers.push(fact);
                    }
                }
                for fact in answered.facts {
                    let fact = fact.attributed_to(&source.member);
                    if !own && fact.member.as_deref().is_some_and(|m| !names.contains(m)) {
                        read.dropped += 1;
                    } else if seen.insert(fact.run_id.clone()) {
                        facts.push(fact);
                    }
                }
                read
            });
            MemberInput {
                member: source.member.clone(),
                load: load.map(|buckets| within(buckets, window, width)),
                records,
                // Without it, the report only cannot name outside processes
                // or list early warnings.
                outside,
            }
        })
        .collect();
    members.sort_by(|a, b| a.member.cmp(&b.member));
    Gathered {
        this_member: fleet.this_member.clone(),
        master: fleet.master.clone(),
        fleet_error: fleet.fleet_error.clone(),
        members,
        facts,
        leftovers,
    }
}

/// Buckets that overlap `window`, in order and each once.
fn within(mut buckets: Vec<Bucket>, window: Window, width: Duration) -> Vec<Bucket> {
    buckets.retain(|b| window.overlaps(b.at, width));
    buckets.sort_by_key(|b| b.at);
    buckets.dedup_by_key(|b| b.at);
    buckets
}

/// Bounded peer-failure evidence from every selected member, independently of
/// whether its request/load history was readable.
pub async fn peer_io(fleet: &Fleet, window: Window, member: Option<&str>) -> serde_json::Value {
    let rows=futures_util::future::join_all(fleet.sources.iter().filter(|s|member.is_none_or(|m|m==s.member)).map(|s|async move {
        match s.history.peer_io(window).await {
            Ok(history)=>serde_json::json!({"member":s.member,"history":history}),
            Err(error)=>serde_json::json!({"member":s.member,"history":{"error":error.code,"attempts":[],"coverage_since":null,"truncated":false}}),
        }
    })).await;
    serde_json::json!({"scope":"outgoing attempts by source member, including recovered retries; not attributed to a consumer label", "members":rows})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        reporting::fixtures::{self, MINUTE, Run, T0, bucket},
        scheduling::timestamp::Timestamp,
    };

    struct Fake {
        facts: Result<Facts>,
        load: Result<Vec<Bucket>>,
    }
    impl MemberHistory for Fake {
        fn facts(&self, _: Window) -> BoxFuture<'static, Result<Facts>> {
            let facts = self.facts.clone();
            Box::pin(async move { facts })
        }
        fn load(&self, _: Window, _: Duration) -> BoxFuture<'static, Result<Vec<Bucket>>> {
            let load = self.load.clone();
            Box::pin(async move { load })
        }
    }

    fn source(member: &str, facts: Result<Facts>, load: Result<Vec<Bucket>>) -> Source {
        Source {
            member: member.into(),
            history: Arc::new(Fake { facts, load }),
        }
    }

    fn query(member: Option<&str>) -> Query {
        let until = Timestamp::from_millis(T0 + 60 * MINUTE);
        let window = Window::new(Timestamp::from_millis(T0), until, until).unwrap();
        Query {
            window,
            series_width: window.fine_width(),
            member: member.map(str::to_owned),
            label: None,
            parameters: Default::default(),
        }
    }

    fn leftover(run_id: &str, member: &str) -> LeftoverFact {
        LeftoverFact {
            run_id: run_id.into(),
            label: Some("test".into()),
            member: member.into(),
            calling_member: "mini".into(),
            requested_at: Timestamp::from_millis(T0 + MINUTE),
            duration_ms: Some(1_000),
            cpu_ms: Some(500),
            capped: false,
        }
    }

    fn fleet() -> Fleet {
        let placed = Fact::of(&fixtures::placed(Run {
            n: 1,
            member: "devbox",
            ..Run::default()
        }))
        .unwrap();
        let mut renamed = Fact::of(&fixtures::pinned(2, "build", "MiniHost", 1.0)).unwrap();
        renamed.member = Some("MiniHost".into());
        let failed = Failure::new("member_unavailable", "devbox is gone", "check");
        // A run the devbox claims ran on a machine outside the fleet.
        let mut stray = Fact::of(&fixtures::pinned(3, "build", "devbox", 1.0)).unwrap();
        stray.member = Some("stranger".into());
        Fleet {
            this_member: "mini".into(),
            master: Some("mini".into()),
            fleet_error: None,
            sources: vec![
                source(
                    "mini",
                    Ok(Facts {
                        facts: vec![placed.clone(), renamed],
                        leftovers: vec![leftover("r-0000000000a1", "mini")],
                        truncated: false,
                    }),
                    Ok(vec![
                        // Outside the period, twice the same minute, out of order.
                        bucket(70, 0.5, 0.0, 0, 0, 1.0),
                        bucket(1, 0.5, 0.0, 0, 0, 1.0),
                        bucket(0, 0.5, 0.0, 0, 0, 1.0),
                        bucket(1, 0.5, 0.0, 0, 0, 1.0),
                    ]),
                ),
                source(
                    "devbox",
                    // The same request again, as if the devbox also reported it.
                    Ok(Facts {
                        facts: vec![placed, stray],
                        // Its own, then one it claims the mini hosted, and the
                        // mini's again.
                        leftovers: vec![
                            leftover("r-0000000000a2", "devbox"),
                            leftover("r-0000000000a3", "mini"),
                            leftover("r-0000000000a1", "devbox"),
                        ],
                        truncated: false,
                    }),
                    Err(failed),
                ),
            ],
        }
    }

    #[tokio::test]
    async fn gathers_every_member_counting_each_request_once() {
        let report = report(&fleet(), &query(None)).await.unwrap();
        let fleet_metrics = &report.metrics.fleet;
        assert_eq!(fleet_metrics.requests.total, 2);
        assert_eq!(fleet_metrics.placement.placeable.by_member["devbox"], 1);
        let members = &report.metrics.members;
        assert_eq!(
            members
                .iter()
                .map(|m| m.member.as_str())
                .collect::<Vec<_>>(),
            ["devbox", "mini"]
        );
        assert_eq!(
            members[0].load_error.as_ref().unwrap().code,
            "member_unavailable"
        );
        let devbox = members[0].records.unwrap();
        assert_eq!((devbox.requests, devbox.dropped), (2, 1));
        assert!(
            report
                .metrics
                .fleet
                .placement
                .placeable
                .by_member
                .keys()
                .all(|m| m != "stranger")
        );
        assert_eq!(members[1].load.as_ref().unwrap().sampled_seconds, 120);
        assert_eq!(
            report.metrics.leftovers.runs, 2,
            "each member's own, each run once"
        );
        assert_eq!(members[1].work.runs, 1, "the run made under the old name");
        assert_eq!(report.findings[0].kind, "incomplete");

        // Filtered to the mini, the report still says the devbox is missing.
        let mini = report_for(Some("mini")).await;
        assert_eq!(mini.metrics.members.len(), 1);
        assert_eq!(mini.findings[0].kind, "incomplete");
        assert_eq!(mini.findings[0].member.as_deref(), Some("devbox"));
    }

    async fn report_for(member: Option<&str>) -> Document {
        report(&fleet(), &query(member)).await.unwrap()
    }

    #[tokio::test]
    async fn a_fleet_that_cannot_be_listed_is_incomplete() {
        let mut fleet = fleet();
        fleet.sources.truncate(1);
        fleet.master = None;
        fleet.fleet_error = Some(Failure::new("invalid_fleet_state", "broken", "repair"));
        let report = report(&fleet, &query(None)).await.unwrap();
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["fleet"]["fleet_error"]["code"], "invalid_fleet_state");
        assert_eq!(report.findings[0].kind, "incomplete");
        assert_eq!(report.findings[0].metric, "fleet");
    }

    #[tokio::test]
    async fn an_unknown_member_or_label_pattern_is_invalid() {
        let error = report(&fleet(), &query(Some("nobody"))).await.unwrap_err();
        assert_eq!(error.code, "invalid_request");
        assert!(error.message.contains("mini, devbox"), "{}", error.message);
        let mut bad = query(None);
        bad.label = Some("a\tb".into());
        assert_eq!(
            report(&fleet(), &bad).await.unwrap_err().code,
            "invalid_request"
        );
    }

    /// A member that answers with its blocked episodes, or cannot say.
    struct Pressured(Result<crate::reporting::outside::OutsideHistory>);
    impl MemberHistory for Pressured {
        fn outside(
            &self,
            _: Window,
        ) -> BoxFuture<'static, Result<crate::reporting::outside::OutsideHistory>> {
            let outside = self.0.clone();
            Box::pin(async move { outside })
        }
        fn facts(&self, _: Window) -> BoxFuture<'static, Result<Facts>> {
            Box::pin(async { Ok(Facts::default()) })
        }
        fn load(&self, _: Window, _: Duration) -> BoxFuture<'static, Result<Vec<Bucket>>> {
            Box::pin(async { Ok(vec![bucket(0, 0.5, 0.0, 0, 0, 1.0)]) })
        }
    }

    #[tokio::test]
    async fn outside_histories_name_processes_and_a_member_without_one_is_still_complete() {
        let episode = fixtures::episode(
            Timestamp::from_millis(T0),
            None,
            &[("fseventsd", 56 << 30, 1.0)],
        );
        let fleet = Fleet {
            this_member: "mini".into(),
            master: Some("mini".into()),
            fleet_error: None,
            sources: vec![
                Source {
                    member: "mini".into(),
                    history: Arc::new(Pressured(Ok(crate::reporting::outside::OutsideHistory {
                        episodes: vec![episode.clone()],
                        ..Default::default()
                    }))),
                },
                Source {
                    member: "devbox".into(),
                    history: Arc::new(Pressured(Err(invalid("unsupported")))),
                },
                // A member on an earlier version, which has no outside route.
                source("old", Ok(Facts::default()), Ok(Vec::new())),
            ],
        };
        let gathered = gather(&fleet, query(None).window).await;
        let outside: Vec<(&str, usize)> = gathered
            .members
            .iter()
            .map(|m| {
                (
                    m.member.as_str(),
                    m.outside.as_ref().map_or(0, |o| o.episodes.len()),
                )
            })
            .collect();
        assert_eq!(outside, [("devbox", 0), ("mini", 1), ("old", 0)]);
        assert_eq!(
            gathered.members[1].outside.as_ref().unwrap().episodes[0],
            episode
        );
        assert!(gathered.members[0].outside.is_err());
        let report = report(&fleet, &query(None)).await.unwrap();
        assert!(
            report.findings.iter().all(|f| f.kind != "incomplete"),
            "{:#?}",
            report.findings
        );
    }
}
