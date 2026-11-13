//! Time-compressed replay orchestration. Trace parsing and model validation
//! happen before the first fleet mutation; every exit path tears down sessions.
use crate::{
    client,
    failure::{Failure, Result},
    paths::Paths,
    rehearsal::{
        invalid,
        plan::{Arrival, Plan},
        protocol::{Action, Call, Job, Reply},
    },
    scheduling::{request::Urgency, timestamp::Timestamp},
    trace_api::Members,
};
use clap::Args;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};
use tokio::time::Instant;

#[derive(Args, Debug, Clone)]
pub struct RehearseArgs {
    #[command(flatten)]
    pub replay_placement: crate::replay_placement::ReplayPlacement,
    /// Replay arrivals F times closer, on top of compression (1–32)
    #[arg(long, default_value = "1")]
    pub load_factor: crate::replay_load::LoadFactor,
    #[arg(long)]
    pub trace: PathBuf,
    /// Explicit RFC 3339 arrival bounds inside the trace
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long)]
    pub until: Option<String>,
    /// Tune JSON artifact (or canonical settings map) to A/B against current settings
    #[arg(long, value_name = "PROPOSAL")]
    pub compare: Option<PathBuf>,
    /// Run identical settings twice and emit an A/A noise-floor artifact
    #[arg(long, conflicts_with = "compare")]
    pub aa: bool,
    /// Completed A/A result or noise-floor artifact for the acceptance rule
    #[arg(long, requires = "compare")]
    pub noise: Option<PathBuf>,
    /// New private file containing the A/A noise floor (also present in JSON)
    #[arg(long)]
    pub noise_output: Option<PathBuf>,
    /// Six compresses an hour into ten minutes, leaving second-scale service
    /// timers and real sync costs visible rather than scaling them away
    #[arg(long, default_value_t = 6.0)]
    pub compress: f64,
    /// Hard execution/submission bound; cleanup and offline fidelity follow
    #[arg(long, default_value = "15m", value_parser = humantime::parse_duration)]
    pub limit: Duration,
    #[arg(long, default_value_t = 0)]
    pub seed: u64,
    /// Permit production work already present at startup; new production arrivals
    /// still stop submissions
    #[arg(long)]
    pub force: bool,
    /// New private trace artifact (default: elsecall-rehearsal-SESSION.json)
    #[arg(long)]
    pub output: Option<PathBuf>,
}
pub(crate) async fn call(paths: &Paths, member: &str, operation: Action) -> Result<Reply> {
    client::post_large(
        &paths.socket(),
        "/v1/rehearsal",
        &Call {
            member: member.into(),
            operation,
        },
    )
    .await
}
pub(crate) async fn all(
    paths: &Paths,
    members: &[String],
    operation: Action,
) -> Vec<Result<Reply>> {
    futures_util::future::join_all(members.iter().map(|m| call(paths, m, operation.clone()))).await
}
fn jobs(replies: &[Reply], collected: &mut BTreeMap<usize, Job>) {
    for reply in replies {
        for job in &reply.jobs {
            collected.insert(job.index, job.clone());
        }
    }
}
fn checked(replies: Vec<Result<Reply>>) -> Result<Vec<Reply>> {
    replies.into_iter().collect()
}
pub async fn run(paths: &Paths, args: RehearseArgs) -> Result<Value> {
    if args.noise_output.is_some() && args.compare.is_none() && !args.aa {
        return Err(invalid(
            "--noise-output requires --aa or --compare with an unchanged proposal.",
        ));
    }
    if args.compare.is_some() || args.aa {
        return crate::rehearsal::compare::run(paths, args).await;
    }
    let mut signals = crate::rehearsal::cancellation::Signals::register()?;
    let source = source(&args)?;
    single(
        paths,
        args,
        &BTreeMap::new(),
        &mut signals,
        &source,
        None,
        None,
    )
    .await
}
pub(crate) async fn single(
    paths: &Paths,
    args: RehearseArgs,
    settings: &BTreeMap<String, String>,
    signals: &mut crate::rehearsal::cancellation::Signals,
    source: &crate::workload::trace::Trace,
    frozen_calibration: Option<&BTreeMap<String, crate::rehearsal::worker::Calibration>>,
    frozen_ideals: Option<&BTreeMap<String, Option<f64>>>,
) -> Result<Value> {
    if args.limit.is_zero() || args.limit > Duration::from_secs(3600) {
        return Err(invalid("--limit must be positive and at most 1h."));
    }
    let source_digest =
        crate::build_identity::Digest::of(&serde_json::to_vec(&source).expect("trace serializes"));
    let candidate = crate::simulation::CandidateSettings::parse(
        source,
        &settings
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>(),
    )?;
    let mut plan = Plan::with_load(source, args.compress, args.load_factor)?;
    for arrival in &mut plan.arrivals {
        let original = source
            .requests
            .iter()
            .find(|r| r.fact.run_id == arrival.original_run_id)
            .expect("planned source");
        if let Some(size) = candidate.size(original) {
            arrival.request.size = size;
        }
        if settings.contains_key("hold_limit")
            && let Urgency::Interactive { hold_limit_ms } = candidate.urgency(original)
        {
            arrival.request.hold_ms = hold_limit_ms;
            arrival.request.urgency = Urgency::Interactive { hold_limit_ms };
        }
    }
    let roster: Members =
        client::post_large(&paths.socket(), "/v1/trace/members", &json!({})).await?;
    if roster.fleet_error.is_some() || roster.master.is_none() {
        return Err(invalid("Rehearsal requires a complete enrolled fleet."));
    }
    let members = roster.members;
    if plan.workspaces.keys().any(|m| !members.contains(m))
        || plan
            .arrivals
            .iter()
            .any(|a| a.request.demand.keys().any(|m| !members.contains(m)))
    {
        return Err(invalid(
            "Trace callers and eligible members must be enrolled in the current fleet.",
        ));
    }
    let now = Timestamp::now();
    let inspections = checked(all(paths, &members, Action::Inspect { since: now }).await)?;
    if inspections.iter().any(|r| r.observations_ready.is_none()) {
        return Err(invalid(
            "Rehearsal certification requires compatible session-recording services on every member.",
        ));
    }
    if !args.force && inspections.iter().any(|r| r.production_active > 0) {
        return Err(Failure::new(
            "rehearsal_busy",
            "Production work is running or queued on the fleet.",
            "Wait until the fleet is idle, or explicitly use --force.",
        ));
    }
    for arrival in &plan.arrivals {
        for (member, demand) in &arrival.request.demand {
            let cores = inspections
                .iter()
                .find(|r| &r.member == member)
                .and_then(|r| r.cores)
                .ok_or_else(|| invalid("Execution-member core counts are unavailable."))?;
            let logical = crate::rehearsal::worker::logical_demand(*demand, cores);
            if !logical.is_finite() || logical > 256.0 {
                return Err(invalid(
                    "Synthetic logical CPU duty exceeds the 256-core worker bound.",
                ));
            }
        }
    }
    let mut random = [0u8; 12];
    getrandom::fill(&mut random)
        .map_err(|_| invalid("Could not generate a rehearsal identity."))?;
    let session = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    for arrival in &mut plan.arrivals {
        arrival.request.label = format!(
            "{}{}:{}",
            crate::rehearsal::PREFIX,
            session,
            arrival
                .request
                .label
                .strip_prefix(crate::rehearsal::PREFIX)
                .expect("planned namespace")
        );
    }
    let output = args
        .output
        .unwrap_or_else(|| PathBuf::from(format!("elsecall-rehearsal-{session}.json")));
    // Reserve the artifact before any work. Refuse existing paths and links.
    let mut reservation = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&output)
    }
    .map_err(|e| Failure::io("Reserve rehearsal trace", e))?;
    use std::os::unix::fs::PermissionsExt;
    reservation
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| Failure::io("Restrict rehearsal trace", e))?;
    let started = Instant::now();
    let deadline = started + args.limit;
    let mut collected = BTreeMap::new();
    let mut submitted = Vec::new();
    let mut epoch = Timestamp::now();
    let mut measured_calibration = BTreeMap::new();
    let mut calibration = BTreeMap::new();
    let rehearsal = async {
        for member in &members {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok("limit");
            }
            let prepared = call(
                paths,
                member,
                Action::Prepare {
                    session: session.clone(),
                    limit_ms: remaining.as_millis() as u64,
                    workspaces: plan.workspaces.get(member).cloned().unwrap_or_default(),
                    seed: args.seed,
                },
            )
            .await?;
            let rate = prepared
                .calibration
                .filter(|c| c.valid())
                .ok_or_else(|| invalid("Member did not supply valid fixed-work calibration."))?;
            measured_calibration.insert(member.clone(), rate);
        }
        calibration = frozen_calibration.unwrap_or(&measured_calibration).clone();
        let work_members: BTreeMap<_, _> = inspections
            .iter()
            .map(|r| {
                Ok((
                    r.member.clone(),
                    crate::rehearsal::work::Member {
                        calibration: calibration
                            .get(&r.member)
                            .cloned()
                            .ok_or_else(|| invalid("Missing member calibration."))?,
                        cores: r
                            .cores
                            .ok_or_else(|| invalid("Missing member core counts."))?,
                        capacity: r
                            .settings
                            .as_ref()
                            .ok_or_else(|| invalid("Missing member capacity."))?
                            .capacity
                            .capacity,
                    },
                ))
            })
            .collect::<Result<_>>()?;
        for arrival in &mut plan.arrivals {
            let original = source
                .requests
                .iter()
                .find(|r| r.fact.run_id == arrival.original_run_id)
                .expect("source request");
            let ideal = frozen_ideals
                .map(|ideals| {
                    ideals
                        .get(&arrival.original_run_id)
                        .copied()
                        .ok_or_else(|| invalid("Frozen ideal evidence lacks a planned request."))
                })
                .transpose()?;
            arrival.request.work = Some(crate::rehearsal::work::Work::freeze(
                arrival.request.duration_ms,
                &arrival.request.demand,
                &arrival.reference_member,
                &work_members,
                original.fact.size,
                ideal,
            )?);
        }
        // A settings relaunch can expose its active snapshot before the first
        // complete speed/load sample. Wait within the same preparation bound;
        // do not begin a certification window with known missing observations.
        loop {
            let replies = checked(all(paths, &members, Action::Inspect { since: now }).await)?;
            if replies.iter().any(|r| r.production_arrivals > 0) {
                return Ok("production_arrived");
            }
            if replies.iter().all(|r| r.observations_ready == Some(true)) {
                break;
            }
            if Instant::now() >= deadline {
                return Ok("limit");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        epoch = Timestamp::now();
        let clock = Instant::now();
        let mut next = 0;
        let mut stop_reason = "completed";
        loop {
            if Instant::now() >= deadline {
                return Ok("limit");
            }
            let replies = checked(all(paths, &members, Action::Inspect { since: now }).await)?;
            if replies.iter().any(|r| r.production_arrivals > 0) {
                next = plan.arrivals.len();
                stop_reason = "production_arrived";
            }
            while next < plan.arrivals.len()
                && Duration::from_millis(plan.arrivals[next].at_ms) <= clock.elapsed()
            {
                // Dispatch a due batch concurrently so equal trace timestamps
                // do not accumulate one fleet round trip per request. Recheck
                // safety between bounded batches, including within dense bursts.
                let current = checked(all(paths, &members, Action::Inspect { since: now }).await)?;
                if current.iter().any(|r| r.production_arrivals > 0) {
                    next = plan.arrivals.len();
                    stop_reason = "production_arrived";
                    break;
                }
                let mut ready = Vec::new();
                while next < plan.arrivals.len()
                    && ready.len() < 32
                    && Duration::from_millis(plan.arrivals[next].at_ms) <= clock.elapsed()
                {
                    let arrival = &plan.arrivals[next];
                    let mut request = arrival.request.clone();
                    if let Urgency::Deferrable { deadline, .. } = &mut request.urgency {
                        *deadline = epoch.after(Duration::from_millis(deadline.millis()));
                    }
                    for (member, demand) in &mut request.demand {
                        let cores = inspections
                            .iter()
                            .find(|r| &r.member == member)
                            .and_then(|r| r.cores)
                            .ok_or_else(|| {
                                invalid("Execution-member core counts are unavailable.")
                            })?;
                        *demand = crate::rehearsal::worker::logical_demand(*demand, cores);
                    }
                    ready.push((arrival.member.clone(), request));
                    // A lost response is an attempted submission too: its
                    // session-marked record remains in the partial export.
                    submitted.push(arrival.clone());
                    next += 1;
                }
                let replies =
                    futures_util::future::join_all(ready.into_iter().map(|(member, request)| {
                        let operation = Action::Launch {
                            session: session.clone(),
                            request,
                        };
                        async move { call(paths, &member, operation).await }
                    }))
                    .await;
                let mut failure = None;
                for reply in replies {
                    match reply {
                        Ok(reply) => jobs(&[reply], &mut collected),
                        Err(error) => {
                            failure.get_or_insert(error);
                        }
                    }
                }
                if let Some(error) = failure {
                    return Err(error);
                }
            }
            let replies = checked(
                all(
                    paths,
                    &members,
                    Action::Poll {
                        session: session.clone(),
                    },
                )
                .await,
            )?;
            jobs(&replies, &mut collected);
            if next == plan.arrivals.len() && collected.values().all(|j| j.exit_code.is_some()) {
                return Ok(stop_reason);
            }
            let delay = plan
                .arrivals
                .get(next)
                .map_or(Duration::from_millis(100), |a| {
                    Duration::from_millis(a.at_ms)
                        .saturating_sub(clock.elapsed())
                        .min(Duration::from_millis(100))
                });
            tokio::time::sleep(delay.max(Duration::from_millis(1))).await;
        }
    };
    let result: Result<&str> = tokio::select! {
        biased;
        reason=signals.cancelled()=>Ok(reason),
        _=tokio::time::sleep_until(deadline)=>Ok("limit"),
        result=rehearsal=>result,
    };
    // Stop every caller before removing any hosting copy. Other production work
    // is never signalled. Stopped remote links settle through normal execution.
    let cleanup_deadline = Instant::now() + Duration::from_secs(15);
    let stopped = tokio::time::timeout_at(
        cleanup_deadline,
        all(
            paths,
            &members,
            Action::Stop {
                session: session.clone(),
            },
        ),
    )
    .await;
    if let Ok(stopped) = stopped {
        for reply in stopped.into_iter().flatten() {
            jobs(&[reply], &mut collected);
        }
    }
    let mut cleanup_errors = Vec::new();
    loop {
        cleanup_errors.clear();
        match tokio::time::timeout_at(
            cleanup_deadline,
            all(
                paths,
                &members,
                Action::Clean {
                    session: session.clone(),
                },
            ),
        )
        .await
        {
            Ok(replies) => {
                for (member, result) in members.iter().zip(replies) {
                    match result {
                        Ok(reply) => jobs(&[reply], &mut collected),
                        Err(error) => cleanup_errors.push(format!("{member}:{}", error.code)),
                    }
                }
            }
            Err(_) => {
                cleanup_errors.push(
                    "cleanup_deadline: member watchdogs/recovery retain responsibility".into(),
                );
                break;
            }
        }
        if cleanup_errors.is_empty() {
            break;
        }
        if Instant::now() >= cleanup_deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Keep the reserved inode; write via its already-open handle below, so a
    // replacement symlink cannot redirect the artifact.
    let (mut stop_reason, rehearsal_error) = match result {
        Ok(reason) => (reason, None),
        Err(error) => ("failed", Some(error.code)),
    };
    let final_activity = all(paths, &members, Action::Inspect { since: now }).await;
    let production_clear = final_activity
        .iter()
        .all(|r| r.as_ref().is_ok_and(|r| r.production_arrivals == 0));
    if !production_clear && stop_reason == "completed" {
        stop_reason = "production_arrived";
    }
    let until = Timestamp::now();
    let recorded_load = all(
        paths,
        &members,
        Action::RecordedLoad {
            session: session.clone(),
            since: epoch,
            until,
        },
    )
    .await;
    let mut trace = crate::trace_cli::collect(paths, epoch, until).await?;
    let mut load_complete = BTreeMap::new();
    for (member, result) in members.iter().zip(recorded_load) {
        let recording = result.ok().and_then(|r| r.recorded_load);
        load_complete.insert(
            member.clone(),
            recording.as_ref().is_some_and(|r| r.complete),
        );
        if let Some(recording) = recording
            && let Some(entry) = trace.members.iter_mut().find(|m| m.member == *member)
        {
            entry.load = recording.load;
        }
    }
    trace
        .requests
        .retain(|r| crate::rehearsal::belongs(&session, r.fact.label.as_deref()));
    let ids: std::collections::BTreeSet<_> = trace
        .requests
        .iter()
        .map(|r| r.fact.run_id.as_str())
        .collect();
    trace.leftovers.retain(|r| ids.contains(r.run_id.as_str()));
    trace
        .demand
        .retain(|d| crate::rehearsal::synthetic(Some(&d.family)));
    trace
        .contention
        .retain(|d| crate::rehearsal::synthetic(Some(&d.family)));
    if inspections.iter().any(|r| r.production_active > 0) {
        trace.incomplete = true;
        trace.limits.push("Forced overlap: pre-existing production grants are outside this rehearsal's request population".into());
    }
    trace.limits.push(format!("Rehearsal compressed by {}; all fidelity thresholds use actual milliseconds; holds and service timers remain real", args.compress));
    if trace.requests.len() != submitted.len()
        || rehearsal_error.is_some()
        || !cleanup_errors.is_empty()
    {
        trace.incomplete = true;
        trace
            .limits
            .push("Some submitted synthetic arrivals have no collected caller record".into());
    }
    let request_mapping = mapping(&submitted, &collected, &trace, epoch);
    let recorded_ids = trace
        .requests
        .iter()
        .map(|r| r.fact.run_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let demand = submitted
        .iter()
        .filter_map(|a| {
            let id = collected.get(&a.request.index)?.run_id.clone()?;
            recorded_ids
                .contains(&id)
                .then(|| (id, a.request.demand.clone()))
        })
        .collect();
    let charge_floor_ms = submitted
        .iter()
        .filter_map(|a| {
            Some((
                collected.get(&a.request.index)?.run_id.clone()?,
                a.original_hold_ms?,
            ))
            .filter(|(id, _)| recorded_ids.contains(id))
        })
        .collect();
    let duration_ms = submitted
        .iter()
        .filter_map(|a| {
            Some((
                collected.get(&a.request.index)?.run_id.clone()?,
                a.request.duration_ms,
            ))
            .filter(|(id, _)| recorded_ids.contains(id))
        })
        .collect();
    trace.rehearsal = Some(crate::rehearsal::recording::Recording {
        calibration: calibration.clone(),
        ideal_ms: submitted
            .iter()
            .filter_map(|a| {
                Some((
                    collected.get(&a.request.index)?.run_id.clone()?,
                    a.request.work.as_ref()?.ideal_ms,
                ))
            })
            .collect(),
        uncontended_ms: submitted
            .iter()
            .filter_map(|a| {
                Some((
                    collected.get(&a.request.index)?.run_id.clone()?,
                    a.request.work.as_ref()?.uncontended_ms.clone(),
                ))
            })
            .collect(),
        work_iterations: submitted
            .iter()
            .filter_map(|a| {
                Some((
                    collected.get(&a.request.index)?.run_id.clone()?,
                    a.request.work.as_ref()?.iterations,
                ))
            })
            .collect(),
        session: session.clone(),
        source_digest,
        build: crate::build_identity::this_build()?,
        compress: args.compress,
        load_factor: args.load_factor,
        seed: args.seed,
        planned: plan.arrivals.len(),
        complete: stop_reason == "completed"
            && submitted.len() == plan.arrivals.len()
            && rehearsal_error.is_none()
            && cleanup_errors.is_empty()
            && inspections.iter().all(|r| r.production_active == 0),
        demand,
        duration_ms,
        charge_floor_ms,
        load_complete,
        requests: json!(request_mapping),
        aggregated_nested: plan.aggregated_nested.clone(),
    });
    let mut artifact = serde_json::to_value(&trace).expect("trace serializes");
    use std::io::Write;
    reservation
        .write_all(&serde_json::to_vec(&artifact).expect("trace serializes"))
        .and_then(|_| reservation.sync_all())
        .map_err(|e| Failure::io("Write rehearsal trace", e))?;
    let replay_trace = trace.clone();
    let seed = args.seed;
    let simulation = tokio::task::spawn_blocking(move || {
        crate::simulation::simulate(
            replay_trace,
            crate::simulation::Options {
                seed,
                fidelity: true,
                ..Default::default()
            },
        )
    })
    .await
    .map_err(|_| invalid("Rehearsal fidelity task failed."))??;
    let certificate = crate::tuning_trust::Certificate::from_rehearsal(&trace, &simulation);
    artifact["fidelity"] = simulation["fidelity"].clone();
    artifact["certificate"] = json!(certificate);
    use std::io::Seek;
    reservation
        .rewind()
        .and_then(|_| reservation.set_len(0))
        .and_then(|_| {
            reservation.write_all(&serde_json::to_vec(&artifact).expect("artifact serializes"))
        })
        .and_then(|_| reservation.sync_all())
        .map_err(|e| Failure::io("Retain rehearsal fidelity", e))?;
    if let Some(certificate) = &certificate {
        checked(
            all(
                paths,
                &members,
                Action::Certify {
                    certificate: Box::new(certificate.clone()),
                },
            )
            .await,
        )?;
    }
    Ok(
        json!({"placement_replay":{"mode":args.replay_placement.placement,"split":crate::replay_placement::split(source)},"calibration":calibration,"measured_calibration":measured_calibration,"recording_complete":trace.rehearsal.as_ref().is_some_and(|r|r.complete_for(&trace)),"certificate":certificate,"session":session,"compress":args.compress,"load_factor":args.load_factor,"seed":seed,"limit_ms":args.limit.as_millis(),"stop_reason":stop_reason,"rehearsal_error":rehearsal_error,"submitted":submitted.len(),"planned":plan.arrivals.len(),"aggregated_nested":plan.aggregated_nested,"elapsed_ms":started.elapsed().as_millis(),"trace":output,"cleanup_complete":cleanup_errors.is_empty(),"cleanup_errors":cleanup_errors,"requests":request_mapping,"metrics":crate::simulation::observed_metrics(&trace),"simulation":simulation}),
    )
}
fn mapping(
    submitted: &[Arrival],
    collected: &BTreeMap<usize, Job>,
    trace: &crate::workload::trace::Trace,
    epoch: Timestamp,
) -> Vec<Value> {
    submitted.iter().map(|a|{
        let job=collected.get(&a.request.index);
        let recorded=job.and_then(|j|j.run_id.as_deref()).and_then(|id|trace.requests.iter().find(|r|r.fact.run_id==id));
        let actual=recorded.map(|r|r.fact.requested_at.millis().saturating_sub(epoch.millis()));
        json!({"original_run_id":a.original_run_id,"original_kind":a.original_kind,"original_label":a.original_label,"original_family":a.original_family,"synthetic_label":a.request.label,"target_effective_cores":a.request.demand,"fixed_work":a.request.work,"ideal_ms":a.request.work.as_ref().and_then(|w|w.ideal_ms),"failure":recorded.and_then(|r|r.fact.failure.as_deref()),"outcome":recorded.and_then(|r|r.outcome.as_ref()),"recorded":recorded.is_some(),"arrival_offset_ms":a.at_ms,"actual_arrival_offset_ms":actual,"arrival_error_ms":actual.map(|ms|ms as i64-a.at_ms as i64),"job":job})
    }).collect()
}

pub(crate) fn source(args: &RehearseArgs) -> Result<crate::workload::trace::Trace> {
    let mut trace = crate::workload::input::read(&args.trace)?;
    args.replay_placement.apply(&mut trace)?;
    let (since, until) =
        crate::workload::bounds::within(&trace, args.since.as_deref(), args.until.as_deref())?;
    crate::rehearsal::window::bound(&trace, since, until)
}
