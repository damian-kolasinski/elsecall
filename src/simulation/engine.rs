//! Virtual event ordering and trial orchestration. Scheduling decisions remain
//! in the production services; execution and observation accounting have their
//! own boundaries.
use super::{
    adapters::{self, *},
    execution::{
        Active, Pending, begin_command, collect, finish, fixed_failure, milliseconds_until, rate,
        settle,
    },
    fidelity, initial,
    member::Member,
    metrics,
    model::{self, Envelope},
    settings::Settings as CandidateSettings,
    timeline::{integrate, record_queues},
    validation,
};
use crate::{
    failure::{Failure, Result},
    scheduling::{
        capacity::Capacity,
        clock::{Clock, VirtualClock},
        parameters::Parameters,
        record::SizeBasis,
        request::{AdmissionRequest, Urgency},
        run_id::RunId,
        settings::{AppliedRule, Settings, Source},
        timestamp::Timestamp,
    },
    workload::trace::{Request, Trace},
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};

pub(super) use super::execution::Outcome;
pub(super) use super::timeline::{Minute, Reserve};

#[derive(Default)]
pub struct Options {
    pub load_factor: crate::replay_load::LoadFactor,
    pub settings: Vec<String>,
    pub seed: u64,
    pub fidelity: bool,
    pub day: Option<Timestamp>,
}
#[derive(Debug, Serialize)]
pub struct Trial {
    pub envelope: Envelope,
    pub outcomes: Vec<Outcome>,
    pub timelines: Vec<Minute>,
    pub reserve: Reserve,
    pub missing_evidence: Vec<String>,
}
pub fn simulate(trace: Trace, options: Options) -> Result<Value> {
    validation::trace(&trace)?;
    let trace_digest = crate::build_identity::Digest::of(
        &serde_json::to_vec(&trace).expect("validated trace serializes"),
    );
    let code_build = crate::build_identity::this_build().ok();
    let overrides = CandidateSettings::parse(&trace, &options.settings)?;
    if options.fidelity
        && (!options.settings.is_empty() || options.load_factor != Default::default())
    {
        return Err(invalid(
            "Fidelity uses recorded settings and arrivals; --set and --load-factor other than 1 are not allowed.",
        ));
    }
    let (since, until) = if let Some(day) = options.day {
        (day, day.after(Duration::from_secs(86400)))
    } else if options.fidelity && trace.rehearsal.is_none() {
        fidelity::busiest_day(&trace).unwrap_or((trace.since, trace.until))
    } else {
        (trace.since, trace.until)
    };
    if since < trace.since || until > trace.until {
        return Err(invalid("The evaluation window must lie inside the trace."));
    }
    let trace = Arc::new(trace);
    let references: References = Arc::new(
        trace
            .requests
            .iter()
            .filter_map(|r| model::reference(&trace, r).map(|v| (r.fact.run_id.clone(), v)))
            .collect(),
    );
    let ideals: BTreeMap<_, _> = trace
        .requests
        .iter()
        .map(|r| {
            (
                r.fact.run_id.clone(),
                model::ideal_from_reference(&trace, r, references.get(&r.fact.run_id).copied()),
            )
        })
        .collect();
    let source = trace;
    let trace = Arc::new(options.load_factor.replay(&source, since, until));
    let until = if options.load_factor == Default::default() {
        until
    } else {
        trace.until
    };
    let run = |envelope, settings: &CandidateSettings| -> Result<Trial> {
        let mut result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .map_err(|e| Failure::io("Create virtual runtime", e))?
            .block_on(trial(
                trace.clone(),
                (options.load_factor != Default::default()).then(|| source.clone()),
                settings,
                envelope,
                since,
                until,
                references.clone(),
            ))?;
        for o in &mut result.outcomes {
            o.ideal_ms = ideals.get(&o.run_id).copied().flatten();
        }
        Ok(result)
    };
    let point = match run(Envelope::Point, &overrides) {
        Ok(trial) => trial,
        Err(error)
            if matches!(
                error.code.as_str(),
                "initial_state_missing" | "missing_trace_evidence"
            ) =>
        {
            let blockers = vec![error.message];
            let trial = Trial {
                envelope: Envelope::Point,
                outcomes: Vec::new(),
                timelines: Vec::new(),
                reserve: Reserve {
                    admission_unknown_member_seconds: until.since(since).as_secs_f64()
                        * trace.members.len() as f64,
                    ..Default::default()
                },
                missing_evidence: blockers.clone(),
            };
            let mut unscorable = metrics::summarize(&trace, &trial, since, until);
            unscorable["status"] = json!("unscorable");
            unscorable["score"] = Value::Null;
            unscorable["violation_vector"] = json!({"reserve_excess_core_seconds":null,"busy_rate_excess":null,"slowdown_excess":null,"extra_other_refusals":null});
            let fidelity = options
                .fidelity
                .then(|| fidelity::compare(&trace, since, until, &[], &[], &blockers));
            let mut result = json!({"simulation_version":2,"trace_version":trace.trace_version,"seed":options.seed,"load_factor":options.load_factor,"settings":options.settings,"since":since,"until":until,"result_return_ms":0,"trusted_for_tuning":false,"metrics":unscorable,"sensitivity":null,"fidelity":fidelity,"missing_evidence":blockers,"outcomes":trace.requests.iter().filter(|r|r.fact.requested_at>=since&&r.fact.requested_at<until).map(|r|json!({"run_id":r.fact.run_id,"state":"blocked_by_missing_evidence","terminal_at":null,"ideal_ms":ideals.get(&r.fact.run_id).copied().flatten()})).collect::<Vec<_>>(),"timelines":[]});
            result["trace_digest"] = json!(trace_digest);
            result["code_build"] = json!(code_build);
            return Ok(result);
        }
        Err(error) => return Err(error),
    };
    let lower = run(Envelope::Lower, &overrides)?;
    let upper = run(Envelope::Upper, &overrides)?;
    let report = if options.fidelity {
        let outcomes = point
            .outcomes
            .iter()
            .filter(|o| {
                trace.requests.iter().any(|r| {
                    r.fact.run_id == o.run_id
                        && !r.fact.nested
                        && r.fact.requested_at >= since
                        && r.fact.requested_at < until
                })
            })
            .map(|o| fidelity::ReplayOutcome {
                run_id: o.run_id.clone(),
                member: o.member.clone(),
                failure: o.failure.clone(),
                wait_ms: o.waited_ms,
                sync_ms: Some(o.sync_ms),
                run_ms: o.run_ms,
                charged_ms: o.charged_ms,
                reference_ms: o.reference_ms,
                terminal_at_ms: o.terminal_at.map(Timestamp::millis),
            })
            .collect::<Vec<_>>();
        let minutes = point
            .timelines
            .iter()
            .map(|m| fidelity::ReplayMinute {
                member: m.member.clone(),
                at_ms: m.at_ms,
                queued_max: m.queued_max,
                interactive_queued_max: m.interactive_queued_max,
                cpu_mean: (m.observed_seconds > 0.0)
                    .then(|| m.cpu_core_seconds / m.observed_seconds / m.weighted_cores),
            })
            .collect::<Vec<_>>();
        Some(fidelity::compare(
            &trace,
            since,
            until,
            &outcomes,
            &minutes,
            &point.missing_evidence,
        ))
    } else {
        None
    };
    let mut point_metrics = metrics::summarize(&trace, &point, since, until);
    let mut lower_metrics = metrics::summarize(&trace, &lower, since, until);
    let mut upper_metrics = metrics::summarize(&trace, &upper, since, until);
    let baseline_settings = CandidateSettings::parse(&trace, &[])?;
    for (envelope, metrics) in [
        (Envelope::Point, &mut point_metrics),
        (Envelope::Lower, &mut lower_metrics),
        (Envelope::Upper, &mut upper_metrics),
    ] {
        let baseline = if options.settings.is_empty() {
            metrics["scheduling_objective"]["failures"]["count"].as_u64()
        } else {
            let baseline = run(envelope, &baseline_settings)?;
            super::objective::summarize(&trace, &baseline, since, until)["failures"]["count"]
                .as_u64()
        };
        super::objective::constrain_failures(metrics, baseline, "unchanged-settings simulation");
    }
    Ok(
        json!({"simulation_version":2,"model_version":2,"trace_version":trace.trace_version,"trace_digest":trace_digest,"code_build":code_build,"seed":options.seed,"since":since,"until":until,
        "load_factor":options.load_factor,"settings":options.settings,"result_return_ms":0,"trusted_for_tuning":false,
        "cohort_terminal_span_seconds":point.outcomes.iter().filter_map(|o|o.terminal_at).max().map(|t|t.since(since).as_secs_f64()),"metrics":point_metrics,"sensitivity":{"lower":lower_metrics,"upper":upper_metrics},
        "fidelity":report,"missing_evidence":point.missing_evidence,"outcomes":point.outcomes,"timelines":point.timelines,
        "limits":["Outside CPU is an inferred residual or grant-derived proxy, not an independent measurement.",
        "H1 contention curves are observational; supported bins do not establish causal slowdown.",
        "The seed is reserved for future sampled work models; current envelopes contain no random draws."]}),
    )
}
pub(crate) fn invalid(message: impl Into<String>) -> Failure {
    Failure::new(
        "invalid_trace",
        message,
        "Use a version 1 fleet trace; see docs/tuning-harness.md.",
    )
}

fn baseline(
    trace: &Trace,
    name: &str,
    start: u64,
    missing: &mut BTreeSet<String>,
) -> Result<(Capacity, Parameters)> {
    let m = trace
        .members
        .iter()
        .find(|m| m.member == name)
        .expect("member exists");
    if let Some(s) = m
        .scheduler_settings
        .iter()
        .rev()
        .find(|s| s.at.millis() <= start)
    {
        return Ok((s.capacity, s.parameters.clone()));
    }
    missing.insert(format!("members[{name}].scheduler_settings before replay start missing; exploratory recorded-decision/default parameters used"));
    let candidate = trace
        .requests
        .iter()
        .filter_map(|r| r.decision.as_ref())
        .flat_map(|d| d.candidates.iter())
        .find(|c| c.member == name);
    let capacity = m
        .load
        .first()
        .map(|b| Capacity {
            weighted_cores: b.weighted_cores,
            reserved: b.weighted_cores.saturating_sub(b.capacity),
            capacity: b.capacity,
        })
        .or_else(|| candidate.map(|c| c.capacity))
        .ok_or_else(|| {
            Failure::new(
                "missing_trace_evidence",
                format!("members[{name}].capacity observation missing."),
                "Collect a complete trace with historical member capacity.",
            )
        })?;
    let parameters = trace
        .requests
        .iter()
        .filter(|r| r.fact.calling_member == name)
        .filter_map(|r| r.decision.as_ref()?.parameters.clone())
        .next()
        .unwrap_or_default();
    Ok((capacity, parameters))
}

async fn trial(
    trace: Arc<Trace>,
    source: Option<Arc<Trace>>,
    overrides: &CandidateSettings,
    envelope: Envelope,
    since: Timestamp,
    until: Timestamp,
    references: References,
) -> Result<Trial> {
    let temp =
        tempfile::tempdir().map_err(|e| Failure::io("Create private simulation state", e))?;
    let start = trace.since.millis();
    let clock: Arc<dyn Clock> = Arc::new(VirtualClock::new(Timestamp::from_millis(start)));
    let stretched_nested = source.is_some();
    let world = Arc::new(Mutex::new(World::with_work(
        source.unwrap_or_else(|| trace.clone()),
        clock.clone(),
        envelope,
        references,
    )));
    let registry: Registry = Arc::default();
    let mut members = BTreeMap::new();
    let mut missing = BTreeSet::new();
    let recency = trace
        .members
        .iter()
        .flat_map(|m| m.scheduler_settings.iter())
        .map(|s| s.parameters.estimates.recency.as_millis() as u64)
        .max()
        .unwrap_or(14 * 86_400_000);
    if trace.rehearsal.is_none() && since.millis().saturating_sub(trace.since.millis()) < recency {
        missing.insert("estimator_history_before_trace: less than one estimator-recency window of causal context and no initial learning/copy-state snapshot".into());
    }
    let fleet_adapters = super::member::FleetAdapters {
        world: world.clone(),
        registry,
        clock: clock.clone(),
        master: trace.master.clone(),
    };
    for (i, m) in trace.members.iter().enumerate() {
        let (capacity, base) = baseline(&trace, &m.member, start, &mut missing)?;
        members.insert(
            m.member.clone(),
            Member::start(
                &m.member,
                capacity,
                base,
                overrides,
                &fleet_adapters,
                temp.path(),
                i,
            )?,
        );
    }
    let mut setting_events: Vec<_> = trace
        .members
        .iter()
        .flat_map(|m| {
            m.scheduler_settings
                .iter()
                .filter(|s| s.at.millis() > start)
                .map(|s| (s.at, m.member.clone(), s.clone()))
        })
        .collect();
    setting_events.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut settings_cursor = 0;
    seed_copies(&trace, &world);
    let mut order: Vec<usize> = (0..trace.requests.len())
        .filter(|i| {
            trace.requests[*i].fact.requested_at < until
                || (stretched_nested
                    && trace.requests[*i].fact.nested
                    && trace.requests[*i].fact.requested_at >= since)
        })
        .collect();
    order.sort_by(|&a, &b| {
        let initial_priority = |i: usize| match trace.requests[i].urgency {
            Urgency::Deferrable {
                deadline,
                at_deadline: crate::scheduling::request::AtDeadline::Start,
            } if deadline.millis() <= start => (0, deadline),
            Urgency::Interactive { .. } => (1, trace.requests[i].fact.requested_at),
            _ => (2, trace.requests[i].fact.requested_at),
        };
        if trace.requests[a].fact.requested_at.millis() < start
            && trace.requests[b].fact.requested_at.millis() < start
        {
            let priority = initial_priority(a).cmp(&initial_priority(b));
            if !priority.is_eq() {
                return priority;
            }
        }
        trace.requests[a]
            .fact
            .requested_at
            .cmp(&trace.requests[b].fact.requested_at)
            .then(
                trace.requests[a]
                    .fact
                    .calling_member
                    .cmp(&trace.requests[b].fact.calling_member),
            )
            .then(
                trace.requests[a]
                    .fact
                    .run_id
                    .cmp(&trace.requests[b].fact.run_id),
            )
    });
    let pids: BTreeMap<String, u32> = order
        .iter()
        .enumerate()
        .map(|(n, &i)| (trace.requests[i].fact.run_id.clone(), 10000 + n as u32 * 2))
        .collect();
    let schedulers = members
        .iter()
        .map(|(name, m)| (name.clone(), m.scheduler.clone()))
        .collect();
    let processes = members
        .iter()
        .map(|(name, m)| (name.clone(), m.processes.clone()))
        .collect();
    let restored = initial::restore(
        &trace,
        Timestamp::from_millis(start),
        &schedulers,
        &processes,
        &pids,
    )?;
    let restored_ids: BTreeSet<_> = restored
        .iter()
        .map(|(i, _)| trace.requests[*i].fact.run_id.clone())
        .collect();
    order.retain(|i| {
        let r = &trace.requests[*i];
        !restored_ids.contains(&r.fact.run_id)
            && (r.fact.requested_at.millis() >= start
                || initial::ended_at(r).is_none_or(|at| at.millis() > start))
    });
    let mut cursor = 0;
    let mut pending: BTreeMap<String, Pending> = BTreeMap::new();
    let mut active: BTreeMap<String, Active> = BTreeMap::new();
    let mut outcomes = Vec::new();
    let mut timelines: BTreeMap<(String, u64), Minute> = BTreeMap::new();
    let mut reserve = Reserve::default();
    for (index, grant) in restored {
        let id = trace.requests[index].fact.run_id.clone();
        pending.insert(
            id.clone(),
            Pending {
                index,
                pid: pids[&id],
                task: tokio::spawn(async move { Ok(grant) }),
            },
        );
    }
    settle(&members, &pending, &trace).await;
    collect(
        &trace,
        &members,
        &world,
        &mut pending,
        &mut active,
        &mut outcomes,
        &mut missing,
        start,
    )
    .await?;
    for (id, a) in &mut active {
        let r = &trace.requests[a.index];
        let admitted = initial::admitted_at(r).unwrap().millis();
        a.leftovers_wait_ms = super::lingering::observed_wait(&trace, r);
        let command_start = admitted + a.sync_ms + a.leftovers_wait_ms;
        let elapsed = start.saturating_sub(command_start) as f64;
        if let Some(run) = r.fact.run_ms {
            let command_ms = run.saturating_sub(a.leftovers_wait_ms);
            a.remaining *=
                ((command_ms as f64 - elapsed) / command_ms.max(1) as f64).clamp(0.0, 1.0);
        } else {
            missing.insert(format!("requests[{id}].carry_in remaining work unknown"));
        }
        a.sync_until = command_start;
        a.run_ms = elapsed;
    }
    for l in trace.leftovers.iter().filter(|l| {
        l.since.millis() < start && l.since.millis() + l.duration_ms.unwrap_or(l.cap_ms) > start
    }) {
        let duration = l.duration_ms.unwrap_or(l.cap_ms).min(l.cap_ms);
        let cores = l.cpu_ms.map_or_else(
            || {
                missing.insert(format!("leftovers[{}].effective demand missing", l.run_id));
                trace
                    .requests
                    .iter()
                    .find(|r| r.fact.run_id == l.run_id)
                    .map_or_else(
                        || {
                            trace
                                .members
                                .iter()
                                .find(|m| m.member == l.member)
                                .and_then(|m| m.load.first())
                                .map_or(1.0, |b| b.weighted_cores.as_f64())
                        },
                        |r| r.fact.size.as_f64(),
                    )
            },
            |cpu| cpu as f64 / duration.max(1) as f64,
        );
        world.lock().unwrap().leftovers.insert(
            l.run_id.clone(),
            (l.member.clone(), cores, l.since.millis() + duration),
        );
        if let Some(workspace) = trace
            .requests
            .iter()
            .find(|r| r.fact.run_id == l.run_id)
            .and_then(adapters::workspace)
        {
            world
                .lock()
                .unwrap()
                .leftover_copies
                .insert(l.run_id.clone(), (l.member.clone(), workspace.root));
        } else {
            missing.insert(format!("leftovers[{}].workspace identity missing; next-run settlement cannot be reconstructed",l.run_id));
        }
    }
    let mut last_tick = None;
    let mut sample_until = start.saturating_add(3000);
    let mut previous_observation = None;
    let limit = until.millis().saturating_add(8 * 86_400_000);
    loop {
        let now = clock.now().millis();
        if now > limit {
            return Err(invalid(
                "Replay exceeded the maximum drain period; unresolved duration or deadline.",
            ));
        }
        let mut restarted = false;
        while settings_cursor < setting_events.len()
            && setting_events[settings_cursor].0.millis() <= now
        {
            let (_, name, settings) = &setting_events[settings_cursor];
            members
                .get_mut(name)
                .unwrap()
                .restart(settings, overrides)?;
            settings_cursor += 1;
            restarted = true;
        }
        if restarted {
            last_tick = None;
            for a in active.values_mut() {
                let r = &trace.requests[a.index];
                if a.grant.placed.is_some()
                    && [&r.fact.calling_member, &a.grant.member]
                        .iter()
                        .any(|name| {
                            members[*name]
                                .scheduler
                                .record(&a.grant.run_id)
                                .is_ok_and(|r| r.state == crate::scheduling::record::State::Ended)
                        })
                {
                    a.execution_failure = Some("connection_lost".into());
                    a.fixed_at = Some(now);
                }
            }
        }
        // Endings precede arrivals at equal timestamps. World state is updated
        // before real release pumps its local queue.
        let cancelled: Vec<String> = pending
            .iter()
            .filter(|(_, p)| {
                let r = &trace.requests[p.index];
                fixed_failure(r).is_some()
                    && model::terminal(r).is_some_and(|at| at <= now)
                    && !p.task.is_finished()
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in cancelled {
            let p = pending.remove(&id).unwrap();
            let r = &trace.requests[p.index];
            p.task.abort();
            let _ = p.task.await;
            outcomes.push(Outcome {
                run_id: id,
                member: None,
                failure: fixed_failure(r).map(str::to_owned),
                waited_ms: Some(now - r.fact.requested_at.millis()),
                sync_ms: 0,
                leftovers_wait_ms: 0,
                run_ms: None,
                terminal_at: Some(Timestamp::from_millis(now)),
                state: "ended",
                ideal_ms: None,
                reference_ms: world
                    .lock()
                    .unwrap()
                    .references
                    .get(&r.fact.run_id)
                    .map(|v| v.0),
                charged_ms: None,
                slowdown: None,
                uncertain: true,
                eligible_members: Vec::new(),
            });
        }
        for a in active
            .values_mut()
            .filter(|a| !a.command_started && a.execution_failure.is_none() && now >= a.sync_until)
        {
            begin_command(&trace, &members, a)?;
            a.command_started = true;
        }
        let ended: Vec<String> = active
            .iter()
            .filter(|(_, a)| {
                a.fixed_at.map_or(
                    a.has_reference && now >= a.sync_until && a.remaining <= 0.000001,
                    |at| now >= at,
                )
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in ended {
            let a = active.remove(&id).unwrap();
            let family_uncertain = finish(&trace, &members, &world, &a)?;
            if family_uncertain {
                missing.insert(format!("requests[{id}].effective demand retained for nested work after outer command ended; attribution uncertain"));
            }
            let r = &trace.requests[a.index];
            let terminal = clock.now();
            let elapsed = terminal
                .millis()
                .saturating_sub(r.fact.requested_at.millis());
            let fixed = a
                .execution_failure
                .clone()
                .or_else(|| fixed_failure(r).map(str::to_owned));
            let charged = if fixed.as_deref() == Some("fleet_busy") {
                a.has_reference.then(|| {
                    elapsed.max(r.hold_limit.map_or(0, |h| h.seconds.as_millis() as u64)) as f64
                        + a.reference
                })
            } else {
                a.has_reference.then_some(elapsed as f64)
            };
            let setup_elapsed = terminal
                .millis()
                .saturating_sub(r.fact.requested_at.millis() + a.grant.waited_ms);
            let actual_leftovers = a.leftovers_wait_ms.min(setup_elapsed);
            let actual_sync = a
                .sync_ms
                .min(setup_elapsed.saturating_sub(actual_leftovers));
            outcomes.push(Outcome {
                run_id: id,
                member: Some(a.grant.member),
                failure: fixed,
                waited_ms: Some(a.grant.waited_ms),
                sync_ms: actual_sync,
                run_ms: a
                    .command_started
                    .then_some(a.run_ms.round() as u64 + a.leftovers_wait_ms),
                leftovers_wait_ms: actual_leftovers,
                terminal_at: Some(terminal),
                state: "ended",
                reference_ms: a.has_reference.then_some(a.reference),
                ideal_ms: None,
                charged_ms: charged,
                slowdown: a.has_reference.then_some(if a.reference > 0.0 {
                    a.run_ms / a.reference
                } else {
                    1.0
                }),
                uncertain: a.uncertain || family_uncertain,
                eligible_members: a.eligible_members,
            });
        }
        {
            let mut w = world.lock().unwrap();
            let ended: Vec<_> = w
                .leftovers
                .iter()
                .filter(|(_, (_, _, end))| *end <= now)
                .map(|(id, _)| id.clone())
                .collect();
            for id in ended {
                w.leftovers.remove(&id);
                w.leftover_copies.remove(&id);
            }
        }
        // Tick observations at the service's one-second cadence.
        if last_tick != Some(now / 1000) {
            for (name, m) in &members {
                if let Some(speed) = model::speed(&trace, name, now) {
                    // A trace contains the already-aggregated best speed, not
                    // six underlying meter trials. Restore that observation.
                    for _ in 0..crate::scheduling::speed::KEPT {
                        m.scheduler.observe_speed(speed);
                    }
                }
                m.scheduler.tick()?;
            }
            for m in members.values() {
                m.scheduler.reconsider_waiting().await;
            }
            last_tick = Some(now / 1000);
        }
        while cursor < order.len()
            && trace.requests[order[cursor]].fact.requested_at.millis() <= now
        {
            let index = order[cursor];
            cursor += 1;
            let r = &trace.requests[index];
            let id = &r.fact.run_id;
            let pid = pids[id];
            let Some(m) = members.get(&r.fact.calling_member) else {
                return Err(invalid("Request caller is not in the member roster."));
            };
            for (field, ancestor) in [
                ("nested_in", r.nested_in.as_ref()),
                ("shares_capacity_of", r.shares_capacity_of.as_ref()),
            ] {
                if let Some(ancestor) = ancestor
                    && !pids.contains_key(ancestor)
                {
                    missing.insert(format!(
                        "requests[{id}].{field} references omitted capacity ancestor {ancestor}"
                    ));
                }
            }
            let outer_frame = r
                .nested_in
                .as_ref()
                .and_then(|id| pids.get(id))
                .map(|pid| Processes::frame(*pid));
            let holder_pid = m.processes.prepare_request(pid, r.fact.kind, outer_frame);
            m.ids
                .0
                .lock()
                .unwrap()
                .push_back(RunId::parse(id).expect("validated ID"));
            let initial_pending = r.fact.requested_at.millis() < start;
            let mut urgency = if initial_pending {
                r.urgency
            } else {
                overrides.urgency(r)
            };
            // Recorded default holds are reconstructed through the estimator;
            // explicit holds retain their original deadline.
            if let Urgency::Interactive {
                hold_limit_ms: None,
            } = urgency
                && r.hold_limit.is_none()
            {
                missing.insert(format!("requests[{id}].resolved hold limit missing"));
                urgency = Urgency::Interactive {
                    hold_limit_ms: Some(600_000),
                };
            }
            let request = AdmissionRequest {
                workload: None,
                kind: r.fact.kind,
                holder_pid,
                size: if initial_pending {
                    Some(r.fact.size)
                } else {
                    overrides.size(r).or_else(|| {
                        match r.fact.resources.as_ref().and_then(|s| s.size_basis) {
                            Some(SizeBasis::Learned { .. } | SizeBasis::Default) => None,
                            _ => Some(r.fact.size),
                        }
                    })
                },
                label: r.fact.label.clone(),
                placement: r.placement_request.clone(),
                urgency,
                workspace: adapters::workspace(r),
                settings: settings(r),
            };
            if r.workspace.is_none()
                && !r.workspace_recorded
                && !matches!(
                    r.placement_request,
                    crate::scheduling::request::Placement::Pinned
                )
            {
                missing.insert(format!(
                    "requests[{id}].workspace missing: routing cannot be reconstructed"
                ));
            }
            let scheduler = m.scheduler.clone();
            let snapshot = if initial_pending {
                if !r.fact.deferrable && r.hold_limit.is_none() {
                    return Err(Failure::new(
                        "initial_state_missing",
                        format!("requests[{id}].initial pending resolved hold limit missing"),
                        "Export the original resolved pending state.",
                    ));
                }
                let basis = r
                    .fact
                    .resources
                    .as_ref()
                    .and_then(|v| v.size_basis)
                    .unwrap_or_else(|| {
                        missing
                            .insert(format!("requests[{id}].initial pending size basis missing"));
                        SizeBasis::Declared
                    });
                let mut work = request.clone().validate(r.fact.requested_at)?;
                work.size = r.fact.size;
                work.size_declared = matches!(basis, SizeBasis::Declared);
                Some(crate::scheduling::service::AdmissionSnapshot {
                    request: work,
                    received_at: r.fact.requested_at,
                    size_basis: basis,
                    hold_limit: r.hold_limit,
                    expected_run: r
                        .decision
                        .as_ref()
                        .and_then(|d| {
                            d.candidates
                                .iter()
                                .find(|c| c.member == r.fact.calling_member)
                        })
                        .and_then(|c| c.expected_run_seconds),
                })
            } else {
                None
            };
            pending.insert(
                id.clone(),
                Pending {
                    index,
                    pid,
                    task: tokio::spawn(async move {
                        if let Some(snapshot) = snapshot {
                            scheduler.request_snapshot(pid, snapshot).await
                        } else {
                            scheduler.request(pid, request).await
                        }
                    }),
                },
            );
            settle(&members, &pending, &trace).await;
            collect(
                &trace,
                &members,
                &world,
                &mut pending,
                &mut active,
                &mut outcomes,
                &mut missing,
                now,
            )
            .await?;
        }
        settle(&members, &pending, &trace).await;
        collect(
            &trace,
            &members,
            &world,
            &mut pending,
            &mut active,
            &mut outcomes,
            &mut missing,
            now,
        )
        .await?;
        // Queue maxima include immediate transitions within the same timestamp.
        record_queues(&members, &mut timelines, now, since, until);
        if active.values().any(|a| {
            a.fixed_at.map_or(
                a.has_reference && now >= a.sync_until && a.remaining <= 0.000001,
                |at| at <= now,
            )
        }) {
            continue;
        }
        // `until` closes admission, not execution. Deferrable and nested work
        // also owe terminal outcomes and can affect the cohort's hard limits.
        if cursor == order.len()
            && now >= until.millis()
            && pending.is_empty()
            // Missing execution references cannot be drained. Keep their
            // unresolved outcomes and capacity exposure explicit, as before.
            && active.values().all(|a| !a.has_reference && a.fixed_at.is_none())
        {
            break;
        }
        let mut next = (now / 1000 + 1) * 1000;
        let observation = {
            let w = world.lock().unwrap();
            trace
                .members
                .iter()
                .map(|m| {
                    let bucket = model::bucket(&trace, &m.member, now);
                    (
                        w.cpu(&m.member),
                        bucket.and_then(|b| b.worst_memory_pressure),
                        bucket.and_then(|b| b.worst_thermal_state),
                    )
                })
                .collect::<Vec<_>>()
        };
        if previous_observation.as_ref() != Some(&observation) {
            sample_until = now.saturating_add(3000);
            previous_observation = Some(observation);
        }
        if pending.is_empty() && active.is_empty() && now >= sample_until {
            next = (now / 60_000 + 1) * 60_000;
        }
        if cursor < order.len() {
            next = next.min(trace.requests[order[cursor]].fact.requested_at.millis());
        }
        if now < until.millis() {
            next = next.min(until.millis());
        }
        if now < trace.until.millis() {
            next = next.min(trace.until.millis());
        }
        if settings_cursor < setting_events.len() {
            next = next.min(setting_events[settings_cursor].0.millis());
        }
        if let Some(end) = world
            .lock()
            .unwrap()
            .leftovers
            .values()
            .map(|(_, _, end)| *end)
            .min()
        {
            next = next.min(end);
        }
        for m in members.values() {
            if let Some(at) = m.scheduler.next_event_at() {
                next = next.min(at.millis());
            }
        }
        if let Some(at) = world.lock().unwrap().next_pre_run_at() {
            next = next.min(at);
        }
        for a in active.values() {
            if let Some(at) = a.fixed_at {
                next = next.min(at);
                continue;
            }
            if now < a.sync_until {
                next = next.min(a.sync_until);
            } else if a.has_reference {
                next = next.min(now.saturating_add(milliseconds_until(
                    a.remaining * rate(&trace, &world, a, now).0,
                )));
            }
        }
        for p in pending.values() {
            let r = &trace.requests[p.index];
            if fixed_failure(r).is_some()
                && let Some(at) = model::terminal(r)
            {
                next = next.min(at);
            }
        }
        if now < since.millis()
            && pending.is_empty()
            && active.is_empty()
            && world.lock().unwrap().leftovers.is_empty()
        {
            let arrival = order.get(cursor).map_or(since.millis(), |index| {
                trace.requests[*index].fact.requested_at.millis()
            });
            let settings = setting_events
                .get(settings_cursor)
                .map_or(since.millis(), |event| event.0.millis());
            let target = arrival
                .min(settings)
                .min(since.millis())
                .saturating_sub(3000);
            if target > now + 1000 {
                next = target;
            }
        }
        if let Some(at) = world.lock().unwrap().next_pre_run_at() {
            next = next.min(at);
        }
        next = next.max(now + 1);
        integrate(
            &trace,
            &members,
            &world,
            &mut active,
            &mut timelines,
            &mut reserve,
            now,
            next,
            since,
            until,
        );
        tokio::time::advance(Duration::from_millis(next - now)).await;
    }
    for (id, p) in &pending {
        missing.insert(format!(
            "requests[{id}].terminal time unavailable after trace/evaluation boundary"
        ));
        outcomes.push(Outcome {
            run_id: id.clone(),
            member: None,
            failure: None,
            waited_ms: None,
            sync_ms: 0,
            leftovers_wait_ms: 0,
            run_ms: None,
            terminal_at: None,
            state: "queued",
            ideal_ms: None,
            reference_ms: world
                .lock()
                .unwrap()
                .references
                .get(&trace.requests[p.index].fact.run_id)
                .map(|v| v.0),
            charged_ms: None,
            slowdown: None,
            uncertain: true,
            eligible_members: Vec::new(),
        });
    }
    for (id, a) in &active {
        missing.insert(format!(
            "requests[{id}].terminal time unavailable after trace/evaluation boundary"
        ));
        outcomes.push(Outcome {
            run_id: id.clone(),
            member: Some(a.grant.member.clone()),
            failure: None,
            waited_ms: Some(a.grant.waited_ms),
            sync_ms: a.sync_ms,
            leftovers_wait_ms: a.leftovers_wait_ms,
            run_ms: None,
            terminal_at: None,
            state: "running",
            ideal_ms: None,
            reference_ms: a.has_reference.then_some(a.reference),
            charged_ms: None,
            slowdown: None,
            uncertain: true,
            eligible_members: Vec::new(),
        });
    }
    for (member, minutes) in &reserve.unknown_minutes {
        for minute in minutes {
            missing.insert(format!(
                "members[{member}].load[{minute}]: outside CPU observation unavailable"
            ));
        }
    }
    missing.extend(world.lock().unwrap().missing_pre_run.iter().cloned());
    for r in &trace.requests {
        if r.fact.failure.as_deref() == Some("config_sync_failed") && !r.pre_run_attempts_complete {
            missing.insert(format!(
                "requests[{}].pre_run_attempts missing for recorded config_sync_failed",
                r.fact.run_id
            ));
        }
    }
    outcomes.retain(|o| {
        trace.requests.iter().any(|r| {
            r.fact.run_id == o.run_id
                && (r.fact.requested_at < until || (stretched_nested && r.fact.nested))
                && (r.fact.requested_at >= since || o.terminal_at.is_none_or(|at| at >= since))
        })
    });
    outcomes.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    Ok(Trial {
        envelope,
        outcomes,
        timelines: timelines.into_values().collect(),
        reserve,
        missing_evidence: missing.into_iter().collect(),
    })
}

pub(super) fn settings(r: &Request) -> Option<Settings> {
    let rule = r.fact.family.as_ref().map(|f| AppliedRule {
        number: 1,
        pattern: f.clone(),
    });
    Some(Settings {
        size: Source::Cli,
        urgency: Source::Cli,
        placement: Source::Cli,
        hold_limit: (!r.fact.deferrable).then_some(Source::Cli),
        deadline: r.fact.deferrable.then_some(Source::Cli),
        at_deadline: r.fact.deferrable.then_some(Source::Cli),
        rule,
    })
}
fn seed_copies(trace: &Trace, world: &Arc<Mutex<World>>) {
    let mut w = world.lock().unwrap();
    let mut requests: Vec<_> = trace.requests.iter().collect();
    requests.sort_by_key(|r| r.fact.requested_at);
    for r in requests {
        let Some(s) = adapters::workspace(r) else {
            continue;
        };
        if let Some(d) = &r.decision {
            for c in &d.candidates {
                if let Some(inputs) = &c.inputs
                    && let crate::scheduling::placement::Copy::Remote { warmth, .. } = inputs.copy
                {
                    w.copies
                        .entry((c.member.clone(), s.root.clone()))
                        .or_insert(CopyState {
                            warmth,
                            busy: false,
                        });
                }
            }
        }
        if let Some(member) = &r.fact.member
            && let Some(warmth) = r.fact.warmth
        {
            w.copies
                .entry((member.clone(), s.root))
                .or_insert(CopyState {
                    warmth,
                    busy: false,
                });
        }
    }
}
#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::workload::trace::TraceOutcome;
    pub fn trace() -> Trace {
        use crate::{
            load_log::buckets::Bucket,
            reporting::fixtures,
            scheduling::{
                capacity::Cores,
                record::{Outcome as End, State},
                records::SchedulerSettings,
            },
            workload::{
                contention::{Curve, Point},
                demand::Demand,
                trace::Member as TraceMember,
            },
        };
        let start = Timestamp::parse_rfc3339("2026-11-03T00:00:00Z").unwrap();
        let mut trace:Trace=serde_json::from_value(json!({"trace_version":1,"since":start,"until":start.after(Duration::from_secs(60)),"incomplete":false,"master":"a","members":[],"requests":[],"leftovers":[],"demand":[],"contention":[],"limits":[]})).unwrap();
        let capacity = Capacity {
            weighted_cores: Cores::from_millis(2000),
            reserved: Cores::ONE,
            capacity: Cores::ONE,
        };
        for name in ["a", "b"] {
            let load:Bucket=serde_json::from_value(json!({"at":start,"seconds":60,"cpu_mean":0.0,"cpu_max":0.0,"outside_mean":0.0,"weighted_cores":2,"capacity":1,"in_use_mean":0.0,"in_use_max":0,"queued_max":0,"interactive_queued_max":0,"saturated_seconds":0,"saturated_busy_seconds":0,"memory_pressure_seconds":0,"worst_memory_pressure":"normal","worst_thermal_state":"nominal","speed_min":if name=="a"{1.0}else{2.0},"speed_max":if name=="a"{1.0}else{2.0}})).unwrap();
            trace.members.push(TraceMember {
                member: name.into(),
                load: vec![load],
                errors: vec![],
                scheduler_settings: vec![SchedulerSettings {
                    at: start,
                    capacity,
                    parameters: Parameters::default(),
                }],
            });
            trace.contention.push(Curve {
                member: name.into(),
                family: "work".into(),
                matched_runs: 6,
                unmatched_runs: 0,
                status: "observational_curve".into(),
                points: vec![
                    Point {
                        other_declared_cores: [0.0, 0.5],
                        runs: 3,
                        duration_ratio_median: 1.0,
                        duration_ratio_range: [1.0, 1.0],
                    },
                    Point {
                        other_declared_cores: [0.5, 2.0],
                        runs: 3,
                        duration_ratio_median: 1.0,
                        duration_ratio_range: [1.0, 1.0],
                    },
                ],
            });
            trace.demand.push(Demand {
                member: name.into(),
                family: "work".into(),
                runs: 3,
                fitted_runs: 3,
                declared_cores: Some(1.0),
                process_tree_cores: Some(1.0),
                effective_cores: Some(1.0),
                interval_cores: Some([0.5, 1.5]),
                fitted_minutes: 10,
                idle_minutes: 3,
                outside_baseline_cores: Some(0.0),
                residual_rmse_cores: Some(0.0),
                status: "identified".into(),
            });
        }
        for (n, name, wait, run) in [(1, "a", 0, 1000), (2, "a", 1000, 1000), (3, "b", 0, 500)] {
            let mut r = fixtures::pinned(n, "work", name, 1.0);
            r.size = Cores::ONE;
            r.size_basis = Some(SizeBasis::Declared);
            r.requested_at = start;
            r.started_at = Some(start.after(Duration::from_millis(wait)));
            r.ended_at = Some(start.after(Duration::from_millis(wait + run)));
            r.waited_ms = Some(wait);
            r.run_ms = Some(run);
            r.cpu_ms = Some(run);
            r.state = State::Ended;
            r.outcome = Some(End::Exited { exit_code: 0 });
            r.urgency = Urgency::Interactive {
                hold_limit_ms: Some(5000),
            };
            r.hold_limit = Some(crate::scheduling::record::HoldLimitRecord {
                seconds: Duration::from_secs(5),
                basis: crate::scheduling::record::HoldBasis::Requested,
                expected_run_seconds: None,
            });
            r.decision = None;
            trace.requests.push(Request::of(&r).unwrap());
        }
        trace
    }
    #[test]
    fn loaded_replay_keeps_the_last_root_before_a_half_open_cutoff() {
        let mut t = trace();
        t.requests.truncate(1);
        let r = &mut t.requests[0];
        r.fact.requested_at = Timestamp::from_millis(t.until.millis() - 1);
        r.fact.resources.as_mut().unwrap().started_at = Some(r.fact.requested_at);
        r.fact.resources.as_mut().unwrap().ended_at =
            Some(r.fact.requested_at.after(Duration::from_secs(1)));
        let result = simulate(
            t,
            Options {
                load_factor: "32".parse().unwrap(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result["outcomes"].as_array().unwrap().len(), 1, "{result}");
        assert_eq!(result["outcomes"][0]["run_ms"], 1000);
    }
    #[test]
    fn loaded_nested_work_stays_with_its_root_through_drain() {
        let mut t = trace();
        t.requests.truncate(2);
        let root = &mut t.requests[0];
        root.fact.requested_at = t.since.after(Duration::from_secs(10));
        root.fact.run_ms = Some(50_000);
        root.fact.resources.as_mut().unwrap().started_at = Some(root.fact.requested_at);
        root.fact.resources.as_mut().unwrap().ended_at = Some(t.until);
        let id = root.fact.run_id.clone();
        let child = &mut t.requests[1];
        child.fact.nested = true;
        child.nested_in = Some(id.clone());
        child.shares_capacity_of = Some(id);
        child.fact.requested_at = t.since.after(Duration::from_secs(50));
        child.fact.waited_ms = Some(0);
        child.fact.resources.as_mut().unwrap().started_at = Some(child.fact.requested_at);
        child.fact.resources.as_mut().unwrap().ended_at =
            Some(t.since.after(Duration::from_secs(51)));
        let result = simulate(
            t,
            Options {
                load_factor: "4".parse().unwrap(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result["outcomes"].as_array().unwrap().len(), 2, "{result}");
        assert!(
            result["outcomes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|o| o["terminal_at"].is_string()),
            "{result}"
        );
    }
    #[test]
    fn frozen_work_and_root_intervals_survive_arrival_scaling() {
        let mut source = trace();
        let r = &mut source.requests[0];
        r.fact.requested_at = source.since.after(Duration::from_secs(12));
        r.fact.resources.as_mut().unwrap().started_at = Some(r.fact.requested_at);
        r.fact.resources.as_mut().unwrap().ended_at =
            Some(r.fact.requested_at.after(Duration::from_secs(1)));
        r.urgency = crate::scheduling::request::Urgency::Deferrable {
            deadline: r.fact.requested_at.after(Duration::from_secs(60)),
            at_deadline: crate::scheduling::request::AtDeadline::Skip,
        };
        let reference = crate::simulation::freeze_reference(&source, &source.requests[0]);
        let replay = crate::replay_load::LoadFactor::try_from(4.0)
            .unwrap()
            .replay(&source, source.since, source.until);
        let mut expected = serde_json::to_value(&source.requests[0]).unwrap();
        expected["execution_reference"] = serde_json::to_value(reference).unwrap();
        let shift = |t: Timestamp| Timestamp::from_millis(t.millis() - 9000);
        expected["requested_at"] = serde_json::json!(shift(source.requests[0].fact.requested_at));
        expected["resources"]["started_at"] = expected["requested_at"].clone();
        expected["resources"]["ended_at"] = serde_json::json!(shift(
            source.requests[0]
                .fact
                .resources
                .as_ref()
                .unwrap()
                .ended_at
                .unwrap()
        ));
        expected["urgency"]["deferrable"]["deadline"] =
            serde_json::json!(source.since.after(Duration::from_secs(63)));
        assert_eq!(serde_json::to_value(&replay.requests[0]).unwrap(), expected);
        assert_eq!(
            serde_json::to_value(&replay.members).unwrap(),
            serde_json::to_value(&source.members).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&replay.demand).unwrap(),
            serde_json::to_value(&source.demand).unwrap()
        );
    }
    #[test]
    fn real_shape_arrival_window_drains_the_last_failed_result() {
        let mut trace = trace();
        trace.requests.truncate(1);
        let arrival = Timestamp::parse_rfc3339("2026-11-04T21:27:24.875Z").unwrap();
        let end = Timestamp::parse_rfc3339("2026-11-04T21:30:41.494Z").unwrap();
        trace.since = arrival;
        trace.until = Timestamp::parse_rfc3339("2026-11-04T21:29:53.032Z").unwrap();
        for member in &mut trace.members {
            member.scheduler_settings[0].at = arrival;
            let bucket = member.load[0].clone();
            member.load = (0..3)
                .map(|minute| {
                    let mut b = bucket.clone();
                    b.at = Timestamp::from_millis(
                        arrival.millis() / 60_000 * 60_000 + minute * 60_000,
                    );
                    b
                })
                .collect();
        }
        let r = &mut trace.requests[0];
        r.fact.run_id = "r-402a0e060926".into();
        r.fact.requested_at = arrival;
        r.fact.run_ms = Some(196_619);
        r.fact.resources.as_mut().unwrap().started_at = Some(arrival);
        r.fact.resources.as_mut().unwrap().ended_at = Some(end);
        r.outcome = Some(TraceOutcome::Exited { exit_code: 1 });
        let result = simulate(trace, Options::default()).unwrap();
        assert_eq!(result["outcomes"].as_array().unwrap().len(), 1);
        assert_eq!(result["outcomes"][0]["state"], "ended");
        assert_eq!(result["outcomes"][0]["terminal_at"], json!(end));
        assert_eq!(result["outcomes"][0]["run_ms"], 196_619);
        let objective = &result["metrics"]["scheduling_objective"];
        assert_eq!(objective["overhead"]["population"], 1);
        assert_eq!(objective["overhead"]["mean"], 0.0);
        assert_eq!(objective["overhead"]["p90"], 0.0);
        assert_eq!(objective["failures"]["count"], 1);
        assert_eq!(objective["hard_limits"]["failures"], true);
    }
    #[test]
    fn drain_never_admits_arrivals_at_or_after_the_selected_day_end() {
        let mut trace = trace();
        let start = trace.since;
        let boundary = start.after(Duration::from_secs(86_400));
        trace.until = boundary.after(Duration::from_secs(60));
        trace.requests.truncate(2);
        for member in &mut trace.members {
            let mut bucket = member.load[0].clone();
            bucket.at = boundary.after(Duration::from_secs(0));
            member.load.push(bucket.clone());
            bucket.at = Timestamp::from_millis(boundary.millis() - 60_000);
            member.load.insert(1, bucket);
        }
        for (i, r) in trace.requests.iter_mut().enumerate() {
            r.fact.requested_at = Timestamp::from_millis(boundary.millis() - 500 + i as u64 * 500);
            r.fact.waited_ms = Some(0);
            r.fact.resources.as_mut().unwrap().started_at = Some(r.fact.requested_at);
            r.fact.resources.as_mut().unwrap().ended_at =
                Some(r.fact.requested_at.after(Duration::from_secs(1)));
        }
        let mut child = trace.requests[1].clone();
        child.fact.run_id = "r-000000000099".into();
        child.fact.nested = true;
        child.nested_in = Some(trace.requests[1].fact.run_id.clone());
        child.shares_capacity_of = child.nested_in.clone();
        trace.requests.push(child);
        let result = simulate(
            trace,
            Options {
                day: Some(start),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result["outcomes"].as_array().unwrap().len(), 1);
        assert_eq!(
            result["outcomes"][0]["terminal_at"],
            json!(boundary.after(Duration::from_millis(500)))
        );
        assert_eq!(
            result["metrics"]["scheduling_objective"]["overhead"]["population"],
            1
        );
    }
    #[test]
    fn drain_includes_queued_and_deferrable_arrivals() {
        use crate::scheduling::request::AtDeadline;
        let mut trace = trace();
        trace.requests.truncate(2);
        trace.until = trace.since.after(Duration::from_millis(500));
        trace.requests[1].fact.deferrable = true;
        trace.requests[1].urgency = Urgency::Deferrable {
            deadline: trace.since.after(Duration::from_secs(3)),
            at_deadline: AtDeadline::Start,
        };
        let result = simulate(trace, Options::default()).unwrap();
        assert_eq!(result["outcomes"].as_array().unwrap().len(), 2);
        for o in result["outcomes"].as_array().unwrap() {
            assert_eq!(o["state"], "ended");
            assert!(!o["terminal_at"].is_null());
        }
        assert_eq!(
            result["metrics"]["scheduling_objective"]["failures"]["unknown_run_ids"],
            json!([])
        );
    }
    #[test]
    fn two_members_exact_ground_truth_and_determinism() {
        let run = || {
            simulate(
                trace(),
                Options {
                    seed: 17,
                    ..Default::default()
                },
            )
            .unwrap()
        };
        let first = run();
        assert_eq!(first, run());
        assert_eq!(first["metrics"]["fleet_busy"]["numerator"], 0);
        assert_eq!(first["metrics"]["fleet_busy"]["denominator"], 3);
        let outcomes = first["outcomes"].as_array().unwrap();
        assert_eq!(outcomes[0]["run_ms"], 1000);
        assert_eq!(outcomes[0]["waited_ms"], 0);
        assert_eq!(outcomes[1]["run_ms"], 1000);
        assert_eq!(outcomes[1]["waited_ms"], 1000);
        assert_eq!(outcomes[2]["run_ms"], 500);
        assert_eq!(outcomes[2]["member"], "b");
        assert_eq!(
            first["metrics"]["objective"]["mean_charged_seconds"],
            json!(3.5 / 3.0)
        );
        assert_eq!(first["metrics"]["reserve"]["violating_member_seconds"], 0.0);
    }
    #[test]
    fn subsecond_candidate_hold_preserves_recorded_refusal_charge_floor() {
        let result = simulate(
            trace(),
            Options {
                settings: vec!["hold_limit=250ms".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let outcome = &result["outcomes"][1];
        assert_eq!(outcome["failure"], "fleet_busy");
        assert_eq!(outcome["waited_ms"], 250);
        assert_eq!(outcome["charged_ms"], 6000.0);
    }
    #[test]
    fn smaller_labels_cannot_hide_effective_demand_or_contention() {
        let mut trace = trace();
        trace.contention[0].points[1].duration_ratio_median = 1.5;
        trace.contention[0].points[1].duration_ratio_range = [1.5, 1.5];
        let result = simulate(
            trace,
            Options {
                settings: vec!["label.work.size=0.5".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result["outcomes"][0]["run_ms"], 1500);
        assert_eq!(result["outcomes"][1]["run_ms"], 1500);
        assert_eq!(result["outcomes"][1]["waited_ms"], 0);
        assert_eq!(result["metrics"]["status"], "infeasible");
        assert_eq!(result["metrics"]["reserve"]["excess_core_seconds"], 1.5);
    }
    #[test]
    fn external_cancellation_is_fixed_while_the_queue_uses_real_admission() {
        let mut trace = trace();
        let first = &mut trace.requests[0];
        first.outcome = Some(TraceOutcome::Cancelled);
        first.fact.failure = Some("cancelled".into());
        first.fact.completed = false;
        let result = simulate(trace, Options::default()).unwrap();
        assert_eq!(result["outcomes"][0]["failure"], "cancelled");
        assert_eq!(result["outcomes"][1]["waited_ms"], 1000);
        assert_eq!(
            result["metrics"]["objective"]["actual_admitted_seconds"]["count"],
            2
        );
    }
    #[test]
    fn next_remote_run_is_admitted_then_settles_leftovers_before_sync() {
        use crate::{
            reporting::facts::PlacementKind,
            scheduling::{
                leftovers::Ending, placed::Warmth, record::WorkspaceIdentity, request::Placement,
            },
            workload::trace::Leftover,
        };
        let mut trace = trace();
        trace.requests.truncate(2);
        let start = trace.since;
        for (i, r) in trace.requests.iter_mut().enumerate() {
            r.placement_request = Placement::Member("b".into());
            r.fact.placement = PlacementKind::Required;
            r.fact.calling_member = "a".into();
            r.fact.member = Some("b".into());
            r.fact.warmth = Some(Warmth::Warm);
            r.fact.sync_ms = Some(0);
            r.workspace = Some(WorkspaceIdentity {
                workspace: "a".repeat(64),
                repository: "b".repeat(64),
            });
            r.workspace_recorded = true;
            let arrival = if i == 0 { 0 } else { 2000 };
            let run = if i == 0 { 1000 } else { 11000 };
            r.fact.requested_at = start.after(Duration::from_millis(arrival));
            r.fact.waited_ms = Some(0);
            r.fact.run_ms = Some(run);
            let resources = r.fact.resources.as_mut().unwrap();
            resources.started_at = Some(r.fact.requested_at);
            resources.ended_at = Some(start.after(Duration::from_millis(arrival + run)));
            r.urgency = Urgency::Interactive {
                hold_limit_ms: Some(20000),
            };
            r.hold_limit.as_mut().unwrap().seconds = Duration::from_secs(20);
        }
        trace.leftovers.push(Leftover {
            run_id: trace.requests[0].fact.run_id.clone(),
            member: "b".into(),
            since: start.after(Duration::from_secs(1)),
            cap_ms: 60000,
            duration_ms: Some(11000),
            cpu_ms: Some(11000),
            ending: Some(Ending::NextRun {
                run_id: trace.requests[1].fact.run_id.clone(),
            }),
        });
        let result = simulate(trace, Options::default()).unwrap();
        assert_eq!(result["outcomes"][1]["member"], "b");
        assert_eq!(result["outcomes"][1]["waited_ms"], 0);
        assert_eq!(result["outcomes"][1]["leftovers_wait_ms"], 10000);
        assert_eq!(result["outcomes"][1]["run_ms"], 11000);
        assert_eq!(result["outcomes"][1]["slowdown"], 1.0);
    }
    #[test]
    fn idle_jumps_preserve_the_production_three_second_gate_window() {
        use crate::scheduling::record::Verdict;
        let mut trace = trace();
        let start = trace.since;
        trace.until = start.after(Duration::from_secs(120));
        trace.requests.truncate(1);
        for member in &mut trace.members {
            let mut b = member.load[0].clone();
            b.at = start.after(Duration::from_secs(60));
            if member.member == "a" {
                b.cpu_mean = Some(1.0);
                b.cpu_max = Some(1.0);
                b.outside_mean = Some(1.0);
            }
            member.load.push(b);
        }
        let r = &mut trace.requests[0];
        r.fact.requested_at = start.after(Duration::from_secs(63));
        r.fact.member = None;
        r.fact.failure = Some("fleet_busy".into());
        r.fact.completed = false;
        r.fact.verdict = Some(Verdict::FleetBusy);
        r.fact.waited_ms = Some(250);
        r.fact.run_ms = None;
        r.fact.resources.as_mut().unwrap().started_at = None;
        r.fact.resources.as_mut().unwrap().ended_at =
            Some(r.fact.requested_at.after(Duration::from_millis(250)));
        r.outcome = Some(crate::workload::trace::TraceOutcome::Failed {
            code: "fleet_busy".into(),
        });
        r.urgency = Urgency::Interactive {
            hold_limit_ms: Some(250),
        };
        r.hold_limit.as_mut().unwrap().seconds = Duration::from_millis(250);
        let result = simulate(trace, Options::default()).unwrap();
        assert_eq!(result["outcomes"][0]["failure"], "fleet_busy");
        assert_eq!(result["outcomes"][0]["waited_ms"], 250);
    }
    #[test]
    fn missing_physical_input_uses_the_same_uncertain_point_reference_without_inventing_cpu() {
        let mut trace = trace();
        trace.members[0].load.clear();
        let result = simulate(trace, Options::default()).unwrap();
        assert_eq!(result["outcomes"][0]["run_ms"], 1000);
        assert_eq!(result["outcomes"][1]["waited_ms"], 1000);
        assert_eq!(result["outcomes"][0]["uncertain"], true);
        assert!(result["metrics"]["members"]["a"]["cpu_mean"].is_null());
        assert_eq!(result["metrics"]["members"]["a"]["unknown_seconds"], 60.0);
        assert!(result["metrics"]["violation_vector"]["reserve_excess_core_seconds"].is_null());
        assert_ne!(result["metrics"]["status"], "feasible");
    }
}
