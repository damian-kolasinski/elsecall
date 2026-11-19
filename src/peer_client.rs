//! Signed requests to another member over the tailnet, with verified responses.
use crate::{
    peer_diagnostics::{self, Attempt, Kind, Phase},
    peer_transport::{self, Progress},
};
use elsecall_failure::{Failure, Result};
use elsecall_membership::{
    member_key::MemberKey,
    peer_auth::{self, SIGNATURE_HEADER},
    roster::Member,
};
use elsecall_wire::api::{ErrorResponse, Response, SCHEMA_VERSION};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Method, Request, body::Bytes, client::conn::http1};
use hyper_util::rt::TokioIo;
use serde::{Serialize, de::DeserializeOwned};
use std::time::Duration;
use std::time::Instant;

pub const RESPONSE_LIMIT: usize = 1024 * 1024;
enum Exchange<T> {
    Success(T),
    Refused(Failure),
}

/// Sends one request to `member` and returns its verified response. Every outcome
/// other than a verified success is an error: an unverifiable answer is treated as
/// no answer, whatever it says.
pub async fn call<T: DeserializeOwned>(
    key: &MemberKey,
    member: &Member,
    method: Method,
    path: &str,
    body: Option<&impl Serialize>,
    timeout: Duration,
) -> Result<T> {
    let retry = if retryable(&method, path) {
        Retry::Idempotent
    } else {
        Retry::Never
    };
    call_with_retry(key, member, method, path, body, timeout, retry).await
}

#[derive(Clone, Copy)]
pub enum Retry {
    Never,
    Idempotent,
}
/// The adapter must establish that replay is safe; mutations use `Never`.
pub async fn call_with_retry<T: DeserializeOwned>(
    key: &MemberKey,
    member: &Member,
    method: Method,
    path: &str,
    body: Option<&impl Serialize>,
    timeout: Duration,
    retry: Retry,
) -> Result<T> {
    let body = body
        .map(|b| serde_json::to_vec(b).expect("serializable request"))
        .unwrap_or_default();
    let started = Instant::now();
    let attempts = if matches!(retry, Retry::Idempotent) {
        2
    } else {
        1
    };
    for attempt in 1..=attempts {
        let remaining = timeout.saturating_sub(started.elapsed());
        let budget = if attempts > 1 {
            remaining.min(timeout / 2)
        } else {
            remaining
        };
        let progress = Progress::default();
        let at = Instant::now();
        let result = tokio::time::timeout(
            budget,
            exchange(key, member, method.clone(), path, body.clone(), &progress),
        )
        .await;
        let (result, kind, refusal) = match result {
            Err(_) => (Err(unavailable(member)), Some(Kind::Timeout), None),
            Ok(Err(error)) => {
                let kind = failure_kind(&error, &progress);
                (Err(error), Some(kind), None)
            }
            Ok(Ok(Exchange::Success(value))) => (Ok(value), None, None),
            Ok(Ok(Exchange::Refused(error))) => {
                let code = error.code.clone();
                (Err(error), Some(Kind::Remote), Some(code))
            }
        };
        let diagnostic = match refusal {
            Some(code) => Attempt::refused(member.name.as_str(), path, attempt, &code, at),
            None => Attempt::new(
                member.name.as_str(),
                path,
                attempt,
                progress.phase(),
                kind,
                at,
            ),
        };
        peer_diagnostics::record(&diagnostic);
        match result {
            Ok(value) => return Ok(value),
            Err(error) => {
                let transient = matches!(
                    kind,
                    Some(Kind::Connect | Kind::Send | Kind::Receive | Kind::Timeout)
                );
                let backoff = Duration::from_millis(100 * attempt as u64);
                if !transient
                    || attempt == attempts
                    || timeout.saturating_sub(started.elapsed()) <= backoff
                {
                    let mut error = error;
                    let mut details =
                        diagnosed(&mut error, &diagnostic, kind == Some(Kind::Remote));
                    details["request_duration_ms"] =
                        serde_json::json!(started.elapsed().as_millis());
                    details["attempts"] = serde_json::json!(attempt);
                    return Err(error.with_details(details));
                }
                tokio::time::sleep(backoff).await;
            }
        }
    }
    unreachable!("at least one attempt")
}

/// `error`'s details with this attempt's diagnostics added. A member's own
/// diagnostics move aside, and a `remote` refusal cannot claim that it closed
/// a connection without answering: only this member observes that.
fn diagnosed(error: &mut Failure, diagnostic: &Attempt, remote: bool) -> serde_json::Value {
    let mut details = error.details.take().map_or(serde_json::json!({}), |d| *d);
    if !details.is_object() {
        details = serde_json::json!({"remote_details":details});
    }
    let fields = details.as_object_mut().expect("details are an object");
    if let Some(prior) = fields.remove("peer_io") {
        fields.insert("remote_peer_io".into(), prior);
    }
    if remote {
        fields.remove(CLOSED_WITHOUT_ANSWER);
    }
    details["peer_io"] = serde_json::json!(diagnostic);
    details
}
fn retryable(method: &Method, path: &str) -> bool {
    (*method == Method::GET && matches!(path, "/peer/v1/status" | "/peer/v1/roster"))
        || (*method == Method::POST && path == "/peer/v1/placements/quote")
}
fn failure_kind(error: &Failure, progress: &Progress) -> Kind {
    match error.code.as_str() {
        "member_unverified" | "member_rejected" => Kind::Authentication,
        "protocol_error" => Kind::Protocol,
        "member_unavailable"
            if error
                .details
                .as_ref()
                .is_some_and(|d| d["peer_timeout"] == true) =>
        {
            Kind::Timeout
        }
        "member_unavailable" => progress.kind(),
        _ => Kind::Remote,
    }
}

async fn exchange<T: DeserializeOwned>(
    key: &MemberKey,
    member: &Member,
    method: Method,
    path: &str,
    body: Vec<u8>,
    progress: &Progress,
) -> Result<Exchange<T>> {
    let (stream, endpoint) = peer_transport::connect(member, progress).await?;
    let stream = crate::socket_io::Socket::new(stream, crate::receive_inactivity::GRACE);
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|_| unavailable(member))?;
    let mut connections = tokio::task::JoinSet::new();
    connections.spawn(connection);

    let nonce = peer_auth::nonce().ok_or_else(|| {
        Failure::new(
            "random_unavailable",
            "The operating system's random number generator failed.",
            "Retry; restart the machine if it keeps failing.",
        )
    })?;
    let proof = peer_auth::Request {
        recipient: &member.key,
        method: method.as_str(),
        path,
        body: &body,
    }
    .sign(key, peer_auth::now(), nonce);
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", endpoint.to_string())
        .header("content-type", "application/json");
    for (name, value) in proof.headers() {
        request = request.header(name, value);
    }
    if let Some(build) = elsecall_build_identity::build_identity::announced() {
        request = request.header(
            elsecall_build_identity::version_skew::BUILD_HEADER,
            build.as_str(),
        );
    }
    let request = request
        .body(Full::new(Bytes::from(body)))
        .map_err(|_| protocol_error(member))?;
    progress.set(Phase::Send);
    let sent = Instant::now();
    let response = sender
        .send_request(request)
        .await
        .map_err(|error| unanswered(member, &error, sent.elapsed()))?;
    let status = response.status();
    let signature = response
        .headers()
        .get(SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = Limited::new(response.into_body(), RESPONSE_LIMIT)
        .collect()
        .await
        .map_err(|e| {
            if e.is::<http_body_util::LengthLimitError>() {
                protocol_error(member)
            } else {
                unavailable(member)
            }
        })?
        .to_bytes();
    progress.set(Phase::Authentication);
    let verified = signature.is_some_and(|signature| {
        peer_auth::Response {
            responder: &member.key,
            requester: &key.public(),
            nonce: &nonce,
            status: status.as_u16(),
            body: &bytes,
        }
        .verify(&signature)
    });
    if !verified {
        return Err(if status == hyper::StatusCode::UNAUTHORIZED {
            Failure::new(
                "member_rejected",
                format!("{} did not accept this member's identity.", member.name),
                format!(
                    "This member is not enrolled on {}. Enrol it there with elsecall fleet enroll, or check that it was not removed.",
                    member.name
                ),
            )
        } else {
            Failure::new(
                "member_unverified",
                format!(
                    "A response claiming to be from {} was not signed by its enrolled key.",
                    member.name
                ),
                "Something else answers at that member's address, or its key changed. Check the member, then remove and enrol it again.",
            )
        });
    }
    // Authenticated: a failure from here on is in what the member answered.
    progress.set(Phase::Receive);
    if status.is_success() {
        let response: Response<T> =
            serde_json::from_slice(&bytes).map_err(|_| protocol_error(member))?;
        if response.schema_version != SCHEMA_VERSION {
            return Err(protocol_error(member));
        }
        Ok(Exchange::Success(response.data))
    } else {
        let response: ErrorResponse =
            serde_json::from_slice(&bytes).map_err(|_| protocol_error(member))?;
        if response.schema_version != SCHEMA_VERSION {
            return Err(protocol_error(member));
        }
        Ok(Exchange::Refused(response.error))
    }
}

pub fn unavailable(member: &Member) -> Failure {
    Failure::new(
        "member_unavailable",
        format!("{} could not be reached over the tailnet.", member.name),
        format!(
            "Check that {} is on, connected to Tailscale and running Elsecall (elsecall doctor there).",
            member.name
        ),
    )
}

/// An outer whole-operation bound, rather than a timeout in a known socket phase.
pub fn operation_timeout(member: &Member, path: &str, started: std::time::Instant) -> Failure {
    let attempt = crate::peer_diagnostics::Attempt::new(
        member.name.as_str(),
        path,
        1,
        crate::peer_diagnostics::Phase::Request,
        Some(crate::peer_diagnostics::Kind::Timeout),
        started,
    );
    crate::peer_diagnostics::record(&attempt);
    unavailable(member).with_details(serde_json::json!({"peer_io":attempt}))
}

/// Marks a `member_unavailable` whose member took the connection and dropped it
/// before any answer: its service listens, but something stops the request.
const CLOSED_WITHOUT_ANSWER: &str = "closed_without_answer";

/// How soon after the request a close still means the member never took it up.
/// A firewall closes at once; the member's own deadlines close a connection
/// after seconds (`peer_listener`'s 30), and that is a slow answer instead.
const PROMPT_CLOSE: Duration = Duration::from_secs(2);

/// Why a request sent `waited` ago got no answer. A member that accepted the
/// connection and promptly closed it is not lost on the tailnet: a firewall that
/// filters the service, such as the macOS Application Firewall with no rule for
/// a new build, does exactly this, and so does a service that stopped
/// mid-request.
fn unanswered(member: &Member, error: &hyper::Error, waited: Duration) -> Failure {
    use std::io::ErrorKind;
    let dropped = std::error::Error::source(error)
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .is_some_and(|io| {
            matches!(
                io.kind(),
                ErrorKind::ConnectionReset | ErrorKind::BrokenPipe | ErrorKind::ConnectionAborted
            )
        });
    if (error.is_incomplete_message() || dropped) && waited < PROMPT_CLOSE {
        closed_without_answer(&member.name)
    } else {
        unavailable(member)
    }
}

/// `member_unavailable` for a member that closed the connection without
/// answering, with its own repair. Callers that branch on `member_unavailable`
/// keep treating it as an unreachable member.
pub fn closed_without_answer(member: &impl std::fmt::Display) -> Failure {
    Failure::new(
        "member_unavailable",
        format!("{member} accepted the connection but closed it without answering."),
        format!(
            "Its firewall may be blocking Elsecall, or its service stopped mid-request. Run elsecall doctor on {member}: it reports the firewall and prints the commands that let members in."
        ),
    )
    .with_details(serde_json::json!({ CLOSED_WITHOUT_ANSWER: true }))
}

/// Whether `failure` is a member that closed the connection without answering.
pub fn is_closed_without_answer(failure: &Failure) -> bool {
    failure
        .details
        .as_ref()
        .and_then(|details| details.get(CLOSED_WITHOUT_ANSWER))
        == Some(&serde_json::Value::Bool(true))
}

fn protocol_error(member: &Member) -> Failure {
    Failure::new(
        "protocol_error",
        format!(
            "{} returned an invalid or incompatible response.",
            member.name
        ),
        "Install the same Elsecall version on every member.",
    )
}

/// How long one member request may take, including connection setup.
pub const PEER_TIMEOUT: Duration = Duration::from_secs(6);

/// Sends the same request to each member concurrently; results follow member order.
pub async fn call_each<T: DeserializeOwned>(
    key: &MemberKey,
    members: &[Member],
    method: Method,
    path: &str,
    body: Option<&impl Serialize>,
) -> Vec<Result<T>> {
    futures_util::future::join_all(
        members
            .iter()
            .map(|member| call(key, member, method.clone(), path, body, PEER_TIMEOUT)),
    )
    .await
}

/// A run's channel to the member that hosts it.
pub type RunChannel = crate::peer_transport::Observed<
    elsecall_secure_channel::SecureStream<TokioIo<hyper::upgrade::Upgraded>>,
>;

/// How long attaching to a member may take, including the member's own checks.
pub const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);

/// Asks `member` to attach the run `run` it admitted, and returns the run's
/// channel: an upgraded connection sealed with keys from an ephemeral exchange that
/// both members' enrolled keys sign.
pub async fn attach(key: &MemberKey, member: &Member, run: &str) -> Result<RunChannel> {
    let path = format!("/peer/v1/placements/{run}/attach");
    channel(key, member, &path, run, "elsecall-run/1").await
}

/// Opens a sealed channel to `member` at `path`, as `attach` does for a run.
/// `purpose` is what both members sign the channel for, such as a run ID, and
/// `protocol` the upgrade the member answers.
pub async fn channel(
    key: &MemberKey,
    member: &Member,
    path: &str,
    purpose: &str,
    protocol: &'static str,
) -> Result<RunChannel> {
    let ephemeral =
        elsecall_secure_channel::Ephemeral::generate().ok_or_else(random_unavailable)?;
    let accepted = open_channel(key, member, path, purpose, protocol, ephemeral.public).await?;
    let keys = accepted
        .acceptance
        .keys(purpose, ephemeral)
        .ok_or_else(|| protocol_error(member))?;
    Ok(crate::peer_transport::Observed::new(
        elsecall_secure_channel::SecureStream::new(accepted.stream, keys),
        member.name.as_str(),
        path,
        crate::peer_transport::stream_timeout(protocol),
    ))
}

fn random_unavailable() -> Failure {
    Failure::new(
        "random_unavailable",
        "The operating system's random number generator failed.",
        "Retry; restart the machine if it keeps failing.",
    )
}

pub(crate) struct AcceptedChannel {
    pub(crate) stream: TokioIo<hyper::upgrade::Upgraded>,
    pub(crate) acceptance: elsecall_membership::peer_auth::ChannelProof,
}

/// Run-only local relay: the service authenticates its roster-selected peer but
/// never owns the caller's ephemeral secret or derives the run's session keys.
pub(crate) async fn relay_attach(
    key: &MemberKey,
    member: &Member,
    run: &str,
    public: [u8; 32],
) -> Result<AcceptedChannel> {
    let path = format!("/peer/v1/placements/{run}/attach");
    open_channel(key, member, &path, run, "elsecall-run/1", public).await
}

async fn open_channel(
    key: &MemberKey,
    member: &Member,
    path: &str,
    purpose: &str,
    protocol: &'static str,
    public: [u8; 32],
) -> Result<AcceptedChannel> {
    let started = Instant::now();
    let progress = Progress::default();
    let result = tokio::time::timeout(
        ATTACH_TIMEOUT,
        channel_once(key, member, path, purpose, protocol, public, &progress),
    )
    .await;
    let (result, kind, refusal) = match result {
        Err(_) => (Err(unavailable(member)), Some(Kind::Timeout), None),
        Ok(Err(error)) => {
            let kind = failure_kind(&error, &progress);
            (Err(error), Some(kind), None)
        }
        Ok(Ok(Exchange::Success(channel))) => (Ok(channel), None, None),
        Ok(Ok(Exchange::Refused(error))) => {
            let code = error.code.clone();
            (Err(error), Some(Kind::Remote), Some(code))
        }
    };
    let diagnostic = match refusal {
        Some(code) => Attempt::refused(member.name.as_str(), path, 1, &code, started),
        None => Attempt::new(
            member.name.as_str(),
            path,
            1,
            progress.phase(),
            kind,
            started,
        ),
    };
    peer_diagnostics::record(&diagnostic);
    result.map_err(|mut error| {
        let details = diagnosed(&mut error, &diagnostic, kind == Some(Kind::Remote));
        error.with_details(details)
    })
}

async fn channel_once(
    key: &MemberKey,
    member: &Member,
    path: &str,
    run: &str,
    protocol: &'static str,
    public: [u8; 32],
    progress: &Progress,
) -> Result<Exchange<AcceptedChannel>> {
    let body = serde_json::to_vec(&serde_json::json!({
        "ephemeral": base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, public),
    }))
    .expect("serializable request");
    let (stream, endpoint) = peer_transport::connect(member, progress).await?;
    let stream = crate::socket_io::Socket::new(stream, crate::receive_inactivity::GRACE);
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|_| unavailable(member))?;
    tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    });
    let nonce = peer_auth::nonce().ok_or_else(|| protocol_error(member))?;
    let proof = peer_auth::Request {
        recipient: &member.key,
        method: "POST",
        path,
        body: &body,
    }
    .sign(key, peer_auth::now(), nonce);
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("host", endpoint.to_string())
        .header("content-type", "application/json")
        .header("connection", "upgrade")
        .header("upgrade", protocol);
    for (name, value) in proof.headers() {
        request = request.header(name, value);
    }
    if let Some(build) = elsecall_build_identity::build_identity::announced() {
        request = request.header(
            elsecall_build_identity::version_skew::BUILD_HEADER,
            build.as_str(),
        );
    }
    let request = request
        .body(Full::new(Bytes::from(body)))
        .map_err(|_| protocol_error(member))?;
    progress.set(Phase::Send);
    let sent = Instant::now();
    let mut response = sender
        .send_request(request)
        .await
        .map_err(|error| unanswered(member, &error, sent.elapsed()))?;
    let status = response.status();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let signature = header(SIGNATURE_HEADER);
    let channel = header(peer_auth::CHANNEL_HEADER);
    if status != hyper::StatusCode::SWITCHING_PROTOCOLS {
        // An answer that is not the upgrade is a verified error, or no answer.
        let bytes = Limited::new(response.into_body(), RESPONSE_LIMIT)
            .collect()
            .await
            .map_err(|e| {
                if e.is::<http_body_util::LengthLimitError>() {
                    protocol_error(member)
                } else {
                    unavailable(member)
                }
            })?
            .to_bytes();
        progress.set(Phase::Authentication);
        let verified = signature.is_some_and(|signature| {
            peer_auth::Response {
                responder: &member.key,
                requester: &key.public(),
                nonce: &nonce,
                status: status.as_u16(),
                body: &bytes,
            }
            .verify(&signature)
        });
        if !verified {
            return Err(Failure::new(
                "member_unverified",
                "The member did not authenticate its answer.",
                "Check fleet membership.",
            ));
        }
        // Authenticated: a failure from here on is in what the member answered.
        progress.set(Phase::Receive);
        let response: ErrorResponse =
            serde_json::from_slice(&bytes).map_err(|_| protocol_error(member))?;
        return Ok(Exchange::Refused(response.error));
    }
    progress.set(Phase::Authentication);
    let unverified = || {
        Failure::new(
            "member_unverified",
            format!(
                "{} did not sign the run's channel with its enrolled key.",
                member.name
            ),
            "Something else answers at that member's address, or its key changed. Check the member, then remove and enrol it again.",
        )
    };
    let acceptance = elsecall_membership::peer_auth::ChannelProof::new(
        run,
        key.public(),
        member.key,
        nonce,
        public,
        channel.ok_or_else(unverified)?,
        signature.ok_or_else(unverified)?,
    );
    if acceptance.verify(run, &public).is_none() {
        return Err(unverified());
    }
    // Authenticated: losing the connection now is a receive failure, not identity.
    progress.set(Phase::Receive);
    let upgraded = hyper::upgrade::on(&mut response)
        .await
        .map_err(|_| unavailable(member))?;
    Ok(Exchange::Success(AcceptedChannel {
        stream: TokioIo::new(upgraded),
        acceptance,
    }))
}
