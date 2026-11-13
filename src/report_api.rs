//! Reports over HTTP, a driving adapter of `reporting`: `POST /v1/report` on the
//! local socket makes a report of the whole fleet, and member routes serve this
//! member's own requests, load, peer I/O and outside-pressure history to the
//! member that reports.
use crate::{
    api,
    failure::{Failure, Result},
    fleet::Fleet,
    load_log::buckets::Bucket,
    membership_store::Membership,
    own_history::OwnHistory,
    peer_api,
    peer_history::PeerHistory,
    report_protocol::{Cursor, FactsPage, FactsRequest, LoadRequest},
    reporting::{
        document::{Document, Query},
        gathering::{self, Source},
        window::{Window, invalid},
    },
    scheduling::timestamp::Timestamp,
};
use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};

/// What `elsecall report` asks for.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportRequest {
    pub since_ms: u64,
    #[serde(default)]
    pub member: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub series_ms: Option<u64>,
}

#[derive(Clone)]
pub struct Reporter {
    pub paths: crate::paths::Paths,
    pub own: OwnHistory,
    /// The fleet, or why this member's fleet state could not be read.
    pub fleet: std::result::Result<Arc<Fleet>, Failure>,
}

fn window(since_ms: u64, until_ms: u64) -> Result<Window> {
    Window::new(
        Timestamp::from_millis(since_ms),
        Timestamp::from_millis(until_ms),
        Timestamp::now(),
    )
}

/// The period another member asks about, ending no later than now by this
/// member's clock: members' clocks may differ by a little.
fn asked_window(since_ms: u64, until_ms: u64) -> Result<Window> {
    let now = Timestamp::now();
    Window::new(
        Timestamp::from_millis(since_ms),
        Timestamp::from_millis(until_ms.min(now.millis())),
        now,
    )
}

impl Reporter {
    /// The members a report asks: this one first, then the fleet's others.
    pub(crate) fn fleet(&self) -> gathering::Fleet {
        let this_member = self.own.scheduler.name();
        let mut fleet = gathering::Fleet {
            this_member: this_member.clone(),
            master: None,
            fleet_error: None,
            sources: vec![Source {
                member: this_member,
                history: Arc::new(self.own.clone()),
            }],
        };
        let members = match &self.fleet {
            Ok(members) => members,
            Err(error) => {
                fleet.fleet_error = Some(error.clone());
                return fleet;
            }
        };
        match members.roster() {
            // A solo member without a tailnet presence is a machine on its own.
            Err(_) if matches!(members.membership(), Membership::Solo) => (),
            Err(error) => fleet.fleet_error = Some(error),
            Ok(roster) => {
                fleet.master = Some(roster.master().name.to_string());
                fleet.sources.extend(
                    roster
                        .members()
                        .iter()
                        .filter(|m| m.key != members.public_key())
                        .map(|member| Source {
                            member: member.name.to_string(),
                            history: Arc::new(PeerHistory {
                                fleet: members.clone(),
                                member: member.clone(),
                            }),
                        }),
                );
            }
        }
        fleet
    }

    pub async fn report(&self, request: ReportRequest) -> Result<Document> {
        let window = window(request.since_ms, Timestamp::now().millis())?;
        let query = Query {
            window,
            series_width: window.series_width(request.series_ms.map(Duration::from_millis)),
            member: request.member,
            label: request.label,
            parameters: self.own.scheduler.parameters().clone(),
        };
        gathering::report(&self.fleet(), &query).await
    }

    fn facts_page(&self, request: FactsRequest) -> Result<FactsPage> {
        let window = asked_window(request.since_ms, request.until_ms)?;
        let before = request
            .before
            .map(|c| (Timestamp::from_millis(c.requested_at_ms), c.run_id));
        let page = self.own.page(window, before.as_ref())?;
        Ok(FactsPage {
            facts: page.facts,
            leftovers: page.leftovers,
            next: page.next.map(|(at, run_id)| Cursor {
                requested_at_ms: at.millis(),
                run_id,
            }),
        })
    }

    /// Never finer than the report itself compares in, so an answer stays small.
    fn load(&self, request: LoadRequest) -> Result<Vec<Bucket>> {
        let window = asked_window(request.since_ms, request.until_ms)?;
        let width = Duration::from_millis(request.width_ms).max(window.fine_width());
        self.own.buckets(window, width)
    }
}

fn status(failure: &Failure) -> StatusCode {
    match failure.code.as_str() {
        "invalid_request" => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// `POST /v1/report` on the local socket.
pub fn routes(reporter: Reporter) -> Router {
    Router::new()
        .route("/v1/report", post(report))
        .with_state(reporter)
}

async fn report(State(reporter): State<Reporter>, body: Bytes) -> Response {
    let Ok(request) = serde_json::from_slice::<ReportRequest>(&body) else {
        let failure = invalid("The report request is not valid.");
        return (status(&failure), Json(api::ErrorResponse::new(failure))).into_response();
    };
    let selected_member = request.member.clone();
    match reporter.report(request).await {
        Ok(document) => {
            let peer_window = Window::new(
                document.period.since,
                document.period.until,
                document.period.until,
            )
            .expect("validated report window");
            let mut value = serde_json::to_value(document).expect("report serializes");
            value["peer_io"] =
                gathering::peer_io(&reporter.fleet(), peer_window, selected_member.as_deref())
                    .await;
            let evidence =
                crate::tuning_trust_api::fleet_status(&reporter.paths, &reporter.fleet).await;
            Json(api::Response::new(crate::reporting::tuning::include(
                value, evidence,
            )))
            .into_response()
        }
        Err(failure) => (status(&failure), Json(api::ErrorResponse::new(failure))).into_response(),
    }
}

/// The member routes, behind the member API's authentication. Any enrolled
/// member may read this member's requests and load, as it may read its status:
/// a report is a diagnosis of the operator's own fleet.
pub fn peer_routes(reporter: Reporter) -> Router<Arc<Fleet>> {
    Router::new()
        .route("/peer/v1/report/facts", post(peer_facts))
        .route("/peer/v1/report/load", post(peer_load))
        .route("/peer/v1/report/peer-io", post(peer_io))
        .route("/peer/v1/report/outside", post(peer_outside))
        .layer(Extension(reporter))
}

async fn answer<R: serde::de::DeserializeOwned + Send + 'static, T: Serialize + Send + 'static>(
    body: &Bytes,
    work: impl FnOnce(R) -> Result<T> + Send + 'static,
) -> Response {
    let Ok(request) = serde_json::from_slice::<R>(body) else {
        return peer_api::failure(
            StatusCode::BAD_REQUEST,
            invalid("The report request is not valid."),
        );
    };
    let result = tokio::task::spawn_blocking(move || work(request))
        .await
        .unwrap_or_else(|_| {
            Err(Failure::new(
                "report_failed",
                "This member's history could not be read.",
                "Run elsecall doctor on the member.",
            ))
        });
    match result {
        Ok(data) => Json(api::Response::new(data)).into_response(),
        Err(failure) if failure.code == "invalid_request" => {
            peer_api::failure(StatusCode::BAD_REQUEST, failure)
        }
        Err(failure) => peer_api::failure(StatusCode::SERVICE_UNAVAILABLE, failure),
    }
}

async fn peer_facts(Extension(reporter): Extension<Reporter>, body: Bytes) -> Response {
    answer(&body, move |request| reporter.facts_page(request)).await
}

async fn peer_load(Extension(reporter): Extension<Reporter>, body: Bytes) -> Response {
    answer(&body, move |request| reporter.load(request)).await
}

async fn peer_outside(Extension(reporter): Extension<Reporter>, body: Bytes) -> Response {
    answer(
        &body,
        move |request: crate::report_protocol::OutsideRequest| {
            let window = asked_window(request.since_ms, request.until_ms)?;
            reporter.own.outside_history(window)
        },
    )
    .await
}

async fn peer_io(body: Bytes) -> Response {
    answer(&body, |request: crate::report_protocol::PeerIoRequest| {
        let window = asked_window(request.since_ms, request.until_ms)?;
        Ok(crate::peer_diagnostics::between(window.since, window.until))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        inspection::{MemoryPressure, ThermalState},
        peer_client::RESPONSE_LIMIT,
        report_protocol::PAGE,
        reporting::{
            facts::{Estimator, Fact, Predicted},
            fixtures::{self, bucket},
            window::FINE_BUCKETS,
        },
    };

    #[test]
    fn the_largest_answers_fit_a_member_response() {
        let mut fact = Fact::of(&fixtures::placed(fixtures::Run::default())).unwrap();
        // The longest label, of four-byte characters, and every optional field.
        fact.label = Some("𝄞".repeat(200));
        fact.family = Some("𝄞".repeat(200));
        fact.run_id = "r".repeat(64);
        fact.failure = Some("workspace_sync_failed".into());
        fact.predicted = Some(Predicted {
            estimator: Estimator::Placement,
            wait_ms: Some(u64::MAX),
            sync_ms: Some(u64::MAX),
            sync_prior: true,
            run_ms: Some(u64::MAX),
            run_prior: true,
            run_level: Some(crate::scheduling::placement::Level::Converted),
        });
        let leftover = crate::reporting::leftovers::LeftoverFact {
            run_id: "r".repeat(64),
            label: fact.label.clone(),
            member: "m".repeat(255),
            calling_member: "m".repeat(255),
            requested_at: fact.requested_at,
            duration_ms: Some(u64::MAX),
            cpu_ms: Some(u64::MAX),
            capped: true,
        };
        let page = FactsPage {
            facts: vec![fact; PAGE as usize],
            // More than one page holds: a record is a fact or a leftover.
            leftovers: vec![leftover; PAGE as usize],
            next: Some(Cursor {
                requested_at_ms: u64::MAX,
                run_id: "r".repeat(64),
            }),
        };
        let size = serde_json::to_vec(&api::Response::new(page)).unwrap().len();
        assert!(size < RESPONSE_LIMIT, "{size} bytes of facts");

        let mut worst = bucket(0, 1.0 / 3.0, 2.0 / 3.0, usize::MAX, u64::MAX, 1.0 / 3.0);
        worst.cpu_max = Some(1.0 / 3.0);
        worst.worst_memory_pressure = Some(MemoryPressure::Critical);
        worst.worst_thermal_state = Some(ThermalState::Trapping);
        worst.memory_pressure_seconds = u64::MAX;
        let buckets = vec![worst; FINE_BUCKETS as usize + 1];
        let size = serde_json::to_vec(&api::Response::new(buckets))
            .unwrap()
            .len();
        assert!(size < RESPONSE_LIMIT, "{size} bytes of load");

        // The most episodes, each naming the most processes by the longest
        // names, of four-byte characters.
        // Every timestamp serializes to the same length.
        let mut episode = fixtures::episode(
            Timestamp::from_millis(fixtures::T0),
            Some(Timestamp::from_millis(fixtures::T0)),
            &[],
        );
        let process = crate::outside_pressure::blocked::OutsideProcess {
            name: "𝄞".repeat(crate::reporting::outside::MAXIMUM_NAME),
            pid: u32::MAX,
            owner: crate::outside_pressure::sampling::Owner::OtherUser,
            memory: crate::outside_pressure::sampling::Memory {
                resident_bytes: u64::MAX,
                footprint_bytes: Some(u64::MAX),
            },
            cpu_cores: 1.0 / 3.0,
            uptime_seconds: u64::MAX,
        };
        episode.blocked.outside = vec![process; crate::reporting::outside::MAXIMUM_PROCESSES];
        let outside = crate::reporting::outside::OutsideHistory {
            episodes: vec![episode; crate::reporting::outside::MAXIMUM_EPISODES],
            ..Default::default()
        };
        let size = serde_json::to_vec(&api::Response::new(outside))
            .unwrap()
            .len();
        assert!(size < RESPONSE_LIMIT, "{size} bytes of outside pressure");
    }

    #[test]
    fn the_largest_report_fits_the_cli_response() {
        use crate::reporting::{
            document::{Gathered, MemberInput, Query, RecordsRead, document},
            fixtures::{Run, T0},
            window::{MAXIMUM_PERIOD, Window},
        };
        // 50,000 requests from each of two members, every one with its own
        // longest label and failure code, and a series point per fine bucket.
        let until = Timestamp::from_millis(T0 + MAXIMUM_PERIOD.as_millis() as u64);
        let window = Window::new(Timestamp::from_millis(T0), until, until).unwrap();
        let facts: Vec<Fact> = (0..100_000u64)
            .map(|n| {
                let member = if n % 2 == 0 { "devbox" } else { "mini" };
                let mut fact = Fact::of(&fixtures::placed(Run {
                    n,
                    member,
                    ..Run::default()
                }))
                .unwrap();
                fact.label = Some(format!("{n:06}{}", "𝄞".repeat(194)));
                fact.failure = Some(format!("code_{n}"));
                fact
            })
            .collect();
        let width = window.fine_width();
        let buckets: Vec<_> = (0..=FINE_BUCKETS)
            .map(|i| {
                let mut b = bucket(0, 1.0 / 3.0, 2.0 / 3.0, 7, 13, 1.0 / 3.0);
                b.at = Timestamp::from_millis(T0 + i * width.as_millis() as u64);
                b
            })
            .collect();
        let gathered = Gathered {
            this_member: "mini".into(),
            master: Some("mini".into()),
            fleet_error: None,
            members: ["devbox", "mini"]
                .map(|member| MemberInput {
                    member: member.into(),
                    outside: Ok(Default::default()),
                    load: Ok(buckets.clone()),
                    records: Ok(RecordsRead {
                        requests: 50_000,
                        truncated: true,
                        dropped: 0,
                    }),
                })
                .to_vec(),
            facts,
            leftovers: Vec::new(),
        };
        let query = Query {
            window,
            series_width: width,
            member: None,
            label: None,
            parameters: Default::default(),
        };
        let report = document(&query, &gathered);
        let size = serde_json::to_vec(&api::Response::new(report))
            .unwrap()
            .len();
        assert!(
            size < crate::client::LARGE_RESPONSE,
            "{size} bytes of report"
        );
    }
}
