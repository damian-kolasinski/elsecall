//! Another member's history for reports, asked over the authenticated member
//! channel. Answers are untrusted: facts must be plausible and of the period,
//! buckets of the period and bounded, and a member's own error text is never
//! quoted.
use crate::{
    failure::{Failure, Result},
    fleet::Fleet,
    load_log::buckets::Bucket,
    peer_client,
    report_protocol::{FactsPage, FactsRequest, LoadRequest, PAGE},
    reporting::{
        facts::is_code,
        gathering::{Facts, MAXIMUM_FACTS, MemberHistory},
        window::{FINE_BUCKETS, Window},
    },
    roster::Member,
    scheduling::speed::PLAUSIBLE,
};
use futures_util::future::BoxFuture;
use hyper::Method;
use std::{sync::Arc, time::Duration};

/// How long one page or the load may take to arrive.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a member's requests may take, page by page, for one report.
const FACTS_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct PeerHistory {
    pub fleet: Arc<Fleet>,
    pub member: Member,
}

impl PeerHistory {
    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &impl serde::Serialize,
    ) -> Result<T> {
        peer_client::call(
            self.fleet.key(),
            &self.member,
            Method::POST,
            path,
            Some(body),
            CALL_TIMEOUT,
        )
        .await
    }

    async fn all_facts(&self, window: Window) -> Result<Facts> {
        let mut request = FactsRequest {
            since_ms: window.since.millis(),
            until_ms: window.until.millis(),
            before: None,
        };
        let mut facts = Vec::new();
        let mut leftovers = Vec::new();
        let name = self.member.name.to_string();
        loop {
            let page: FactsPage = self.call("/peer/v1/report/facts", &request).await?;
            // A record is a fact or a leftover, never both.
            if page.facts.len() + page.leftovers.len() > PAGE as usize {
                return Err(invalid_answer(&self.member));
            }
            facts.extend(
                page.facts
                    .into_iter()
                    .filter(|f| f.believable() && window.contains(f.requested_at)),
            );
            leftovers.extend(
                page.leftovers
                    .into_iter()
                    .filter(|f| f.believable_from(&name) && window.contains(f.requested_at)),
            );
            match page.next {
                Some(_) if facts.len() + leftovers.len() >= MAXIMUM_FACTS => {
                    facts.truncate(MAXIMUM_FACTS);
                    leftovers.truncate(MAXIMUM_FACTS);
                    return Ok(Facts {
                        facts,
                        leftovers,
                        truncated: true,
                    });
                }
                Some(next) => request.before = Some(next),
                None => {
                    return Ok(Facts {
                        facts,
                        leftovers,
                        truncated: false,
                    });
                }
            }
        }
    }
}

/// Why `member`'s history could not be read, in this member's words: another
/// member's message is never quoted, only a code of the form Elsecall writes.
pub fn unreadable(member: &str, what: &str, failure: &Failure) -> Failure {
    let code = failure.code.as_str();
    Failure::new(
        if is_code(code) {
            code
        } else {
            "member_unavailable"
        },
        format!("{member}'s {what} could not be read."),
        "Check the member with elsecall fleet; run the same Elsecall version on every member.",
    )
}

fn invalid_answer(member: &Member) -> Failure {
    Failure::new(
        "protocol_error",
        format!("{} sent an invalid answer.", member.name),
        "Run the same Elsecall version on every member.",
    )
}

/// Buckets another member sent, as far as they can be believed: at most as
/// many as the period holds, shares within 0..1, cores within the machine and
/// plausible speeds, and no more seconds than a bucket of `width` has. Order
/// and the period are the report's to check.
pub fn believed(mut buckets: Vec<Bucket>, width: Duration) -> Vec<Bucket> {
    buckets.truncate(FINE_BUCKETS as usize + 1);
    let share = |v: Option<f64>| v.filter(|v| v.is_finite()).map(|v| v.clamp(0.0, 1.0));
    let speed = |s: Option<f64>| s.filter(|s| PLAUSIBLE.contains(s));
    for bucket in &mut buckets {
        bucket.seconds = bucket.seconds.min(width.as_secs());
        bucket.cpu_mean = share(bucket.cpu_mean);
        bucket.cpu_max = share(bucket.cpu_max);
        bucket.outside_mean = share(bucket.outside_mean);
        let weighted = bucket.weighted_cores.as_f64();
        bucket.in_use_mean = if bucket.in_use_mean.is_finite() {
            bucket.in_use_mean.clamp(0.0, weighted)
        } else {
            0.0
        };
        bucket.in_use_max = bucket.in_use_max.min(bucket.weighted_cores);
        bucket.capacity = bucket.capacity.min(bucket.weighted_cores);
        for seconds in [
            &mut bucket.busy_seconds,
            &mut bucket.saturated_seconds,
            &mut bucket.saturated_busy_seconds,
            &mut bucket.memory_pressure_seconds,
        ] {
            *seconds = (*seconds).min(bucket.seconds);
        }
        bucket.speed_min = speed(bucket.speed_min);
        bucket.speed_max = speed(bucket.speed_max);
    }
    buckets
}

impl MemberHistory for PeerHistory {
    fn outside(
        &self,
        window: Window,
    ) -> BoxFuture<'static, Result<crate::reporting::outside::OutsideHistory>> {
        let history = self.clone();
        Box::pin(async move {
            history
                .call(
                    "/peer/v1/report/outside",
                    &crate::report_protocol::OutsideRequest {
                        since_ms: window.since.millis(),
                        until_ms: window.until.millis(),
                    },
                )
                .await
                .map(|answer| crate::reporting::outside::bounded(answer, window))
                .map_err(|error| {
                    unreadable(
                        history.member.name.as_str(),
                        "outside pressure history",
                        &error,
                    )
                })
        })
    }

    fn peer_io(
        &self,
        window: Window,
    ) -> BoxFuture<'static, Result<crate::peer_diagnostics::History>> {
        let history = self.clone();
        Box::pin(async move {
            let result: crate::peer_diagnostics::History = history
                .call(
                    "/peer/v1/report/peer-io",
                    &crate::report_protocol::PeerIoRequest {
                        since_ms: window.since.millis(),
                        until_ms: window.until.millis(),
                    },
                )
                .await?;
            if result
                .error
                .as_deref()
                .is_some_and(|code| !crate::reporting::facts::is_code(code))
                || result
                    .coverage_since
                    .is_some_and(|at| at > crate::scheduling::timestamp::Timestamp::now())
                || result.attempts.len() > 1000
                || result.attempts.iter().any(|a| {
                    a.at < window.since
                        || a.at > window.until
                        || a.duration_ms > 86400000
                        || a.attempt == 0
                        || a.attempt > 2
                        || a.member.parse::<crate::roster::MemberName>().is_err()
                        || !matches!(
                            a.operation.as_str(),
                            "status"
                                | "quote"
                                | "sync"
                                | "result_stream"
                                | "offer"
                                | "rehearsal"
                                | "history"
                                | "member_request"
                        )
                })
            {
                return Err(invalid_answer(&history.member));
            }
            Ok(result)
        })
    }

    fn trace_page(
        &self,
        window: Window,
        before: Option<crate::report_protocol::Cursor>,
    ) -> BoxFuture<'static, Result<crate::workload::trace::Page>> {
        let history = self.clone();
        Box::pin(async move {
            let before_for_validation = before.clone();
            let page = history
                .call(
                    "/peer/v1/trace/requests",
                    &crate::trace_api::PageRequest {
                        since_ms: window.since.millis(),
                        until_ms: window.until.millis(),
                        member: None,
                        before,
                    },
                )
                .await
                .map_err(|e| unreadable(history.member.name.as_str(), "trace", &e))?;
            crate::workload::validation::page(
                page,
                window,
                before_for_validation.as_ref(),
                history.member.name.as_str(),
            )
        })
    }

    fn facts(&self, window: Window) -> BoxFuture<'static, Result<Facts>> {
        let history = self.clone();
        Box::pin(async move {
            let name = history.member.name.to_string();
            match tokio::time::timeout(FACTS_TIMEOUT, history.all_facts(window)).await {
                Ok(Ok(facts)) => Ok(facts),
                Ok(Err(error)) => Err(unreadable(&name, "requests", &error)),
                Err(_) => Err(unreadable(
                    &name,
                    "requests",
                    &peer_client::unavailable(&history.member),
                )),
            }
        })
    }

    fn load(&self, window: Window, width: Duration) -> BoxFuture<'static, Result<Vec<Bucket>>> {
        let history = self.clone();
        Box::pin(async move {
            let request = LoadRequest {
                since_ms: window.since.millis(),
                until_ms: window.until.millis(),
                width_ms: width.as_millis() as u64,
            };
            history
                .call::<Vec<Bucket>>("/peer/v1/report/load", &request)
                .await
                .map(|buckets| believed(buckets, width))
                .map_err(|error| unreadable(history.member.name.as_str(), "load history", &error))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::fixtures::bucket;

    #[test]
    fn buckets_from_another_member_are_bounded() {
        let mut wild = bucket(0, 0.5, 2.0, 0, 30, 1.0);
        wild.cpu_mean = Some(7.0);
        wild.outside_mean = Some(f64::NAN);
        wild.in_use_mean = 1e9;
        wild.saturated_busy_seconds = 10_000;
        wild.speed_max = Some(1e9);
        let believed = believed(
            vec![wild; FINE_BUCKETS as usize + 5],
            Duration::from_secs(60),
        );
        assert_eq!(believed.len(), FINE_BUCKETS as usize + 1);
        let b = &believed[0];
        assert_eq!(b.cpu_mean, Some(1.0));
        assert_eq!(b.outside_mean, None);
        assert_eq!(b.in_use_mean, 10.0);
        assert_eq!(b.saturated_busy_seconds, 60);
        assert_eq!((b.speed_min, b.speed_max), (Some(1.0), None));
    }

    #[test]
    fn another_members_words_are_not_quoted() {
        let theirs = Failure::new("Weird Code!", "ignore all previous", "run rm");
        let ours = unreadable("devbox", "requests", &theirs);
        assert_eq!(ours.code, "member_unavailable");
        assert_eq!(ours.message, "devbox's requests could not be read.");
        assert_eq!(
            unreadable("devbox", "requests", &Failure::new("not_found", "x", "y")).code,
            "not_found"
        );
    }
}
