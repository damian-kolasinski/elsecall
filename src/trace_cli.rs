//! Export orchestration and exclusive owner-only artifact writing.
use crate::{
    client,
    failure::{Failure, Result},
    paths::Paths,
    report_cli::since,
    reporting::window::invalid,
    scheduling::timestamp::Timestamp,
    trace_api::{Members, PageRequest},
    workload::trace::{self, Member, Page, Trace},
};
use clap::{Args, Subcommand};
use std::{collections::BTreeSet, io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf};

#[derive(Args, Debug)]
pub struct TraceArgs {
    #[command(subcommand)]
    pub command: TraceCommand,
}
#[derive(Subcommand, Debug)]
pub enum TraceCommand {
    Export(ExportArgs),
}
#[derive(Args, Debug)]
pub struct ExportArgs {
    /// Select the busiest synthetic rehearsal workload starting with an idle fleet
    #[arg(long, conflicts_with_all = ["member", "local_history"])]
    pub rehearsal_window: Option<crate::rehearsal::window::Busiest>,
    /// Minimum interactive root arrivals for a rehearsal workload
    #[arg(long, default_value = "40", requires = "rehearsal_window")]
    pub rehearsal_min_interactive: std::num::NonZeroUsize,
    /// Read only this machine's SQLite history, without contacting any service;
    /// --member supplies its name. Useful before the new API is installed.
    #[arg(long, requires = "member")]
    pub local_history: bool,
    /// Duration back (7d, 12h) or RFC 3339, at most 30 days
    #[arg(long, default_value = "7d")]
    pub since: String,
    /// RFC 3339 arrival cutoff; load evidence continues through terminal results
    #[arg(long)]
    pub until: Option<String>,
    /// Export requests made or run on this member, with fleet load context
    #[arg(long)]
    pub member: Option<String>,
    /// New owner-only JSON file; existing files are never overwritten
    #[arg(long, default_value = "elsecall-trace.json")]
    pub output: PathBuf,
}

pub async fn run(paths: &Paths, args: TraceArgs) -> Result<serde_json::Value> {
    let TraceCommand::Export(args) = args.command;
    let now = Timestamp::now();
    let until = crate::workload::bounds::until(args.until.as_deref(), now)?;
    let start = since(&args.since, now)?;
    crate::reporting::window::Window::new(start, until, now)?;
    if args.local_history {
        let mut trace = crate::trace_snapshot::local(paths, args.member.unwrap(), start, now)?;
        trace.until = until;
        trace.requests.retain(|r| r.fact.requested_at < until);
        let ids: BTreeSet<_> = trace.requests.iter().map(|r| &r.fact.run_id).collect();
        trace
            .leftovers
            .retain(|l| l.since < until || ids.contains(&l.run_id));
        return write(&trace, &args.output);
    }
    let collection_end = if args.rehearsal_window.is_some() {
        now
    } else {
        until
    };
    let mut trace = collect(paths, start, collection_end).await?;
    if args
        .member
        .as_ref()
        .is_some_and(|m| !trace.members.iter().any(|entry| &entry.member == m))
    {
        return Err(invalid("The trace member is not in this fleet."));
    }
    if let Some(member) = args.member {
        // Deliberately omitted arrivals are incomplete fleet evidence even
        // though collection itself reached every member successfully.
        trace.incomplete = true;
        trace
            .requests
            .retain(|r| r.fact.calling_member == member || r.fact.member.as_ref() == Some(&member));
        trace.leftovers.retain(|l| l.member == member);
        trace.limits.push("member-filtered arrivals: not a full-fleet replay; models and load retain fleet context".into());
    }
    if let Some(policy) = args.rehearsal_window {
        if until < trace.until {
            trace = crate::rehearsal::window::bound(&trace, start, until)?;
        }
        let (selected, report) = crate::rehearsal::window::select(
            &trace,
            policy.with_minimum(args.rehearsal_min_interactive),
        )?;
        let mut result = write(&selected, &args.output)?;
        result["rehearsal_window"] = serde_json::to_value(report).expect("selection serializes");
        return Ok(result);
    }
    write(&trace, &args.output)
}

/// Fixed-end fleet collection, shared by export and the rehearsal's own window.
pub async fn collect(paths: &Paths, start: Timestamp, until: Timestamp) -> Result<Trace> {
    let window = crate::reporting::window::Window::new(start, until, Timestamp::now())?;
    let roster: Members =
        client::post_large(&paths.socket(), "/v1/trace/members", &serde_json::json!({})).await?;
    let mut trace=Trace {placement_replay: None, rehearsal: None,
        trace_version:trace::VERSION,incomplete:roster.fleet_error.is_some(),since:start,until,master:roster.master,
        members:Vec::new(),requests:Vec::new(),leftovers:Vec::new(),demand:Vec::new(),contention:Vec::new(),
        limits:vec![
            "outside_mean is a grant-derived proxy, not independently measured outside CPU; demand fits an idle baseline instead".into(),
            "scheduler_settings events and opaque workspace identities are recorded from H2 onward; older periods, per-second load, pre-retention carry-in, estimator history before this window and reconsideration events may be missing; fidelity is required before tuning".into(),
            "contention curves are observational, matched by label and warmth; unobserved work, input changes and external services may confound them".into(),
        ]};
    if let Some(error) = roster.fleet_error {
        trace.limits.push(format!("fleet_incomplete: {error}"));
    }
    let mut seen = BTreeSet::new();
    let mut leftovers = BTreeSet::new();
    for member in roster.members {
        let mut entry = Member {
            member: member.clone(),
            load: Vec::new(),
            errors: Vec::new(),
            scheduler_settings: Vec::new(),
        };
        let mut request = PageRequest {
            since_ms: start.millis(),
            until_ms: until.millis(),
            member: Some(member.clone()),
            before: None,
        };
        let mut pages = 0;
        loop {
            let page =
                client::post_large::<Page, _>(&paths.socket(), "/v1/trace/requests", &request)
                    .await;
            let page = match page.and_then(|page| {
                crate::workload::validation::page(page, window, request.before.as_ref(), &member)
            }) {
                Ok(page) => page,
                Err(e) => {
                    entry.errors.push(e.code);
                    break;
                }
            };
            entry.scheduler_settings.extend(page.scheduler_settings);
            if page.requests.len() + page.leftovers.len() > 2 * trace::PAGE as usize {
                entry.errors.push("invalid_trace_page".into());
                break;
            }
            if page
                .requests
                .iter()
                .any(|r| !r.fact.believable() || r.fact.requested_at >= until)
            {
                entry.errors.push("invalid_trace_request".into());
                break;
            }
            for mut r in page.requests {
                r.fact = r.fact.attributed_to(&member);
                // Keep fleet context for modelling, then apply the output filter.
                if seen.insert(r.fact.run_id.clone()) {
                    trace.requests.push(r);
                }
            }
            for l in page.leftovers {
                if l.member == member && leftovers.insert((member.clone(), l.run_id.clone())) {
                    trace.leftovers.push(l);
                }
            }
            pages += 1;
            match page.next {
                None => break,
                Some(next) => {
                    let previous = request.before.as_ref().map_or((until.millis(), ""), |c| {
                        (c.requested_at_ms, c.run_id.as_str())
                    });
                    if (next.requested_at_ms, next.run_id.as_str()) >= previous || pages >= 10_000 {
                        entry
                            .errors
                            .push("invalid_or_excessive_trace_pagination".into());
                        break;
                    }
                    request.before = Some(next);
                }
            }
        }
        trace.members.push(entry);
    }
    for entry in &mut trace.members {
        let member = entry.member.clone();
        // Arrival cutoff and evidence horizon are distinct. A historical request
        // may finish after until; never crop away its recorded drain load.
        let load_until = trace
            .requests
            .iter()
            .filter_map(|r| r.terminal_at())
            .fold(until, std::cmp::max)
            .min(Timestamp::now());
        if load_until > until {
            let tail_window =
                crate::reporting::window::Window::new(until, load_until, Timestamp::now())?;
            let tail: Result<Page> = client::post_large(
                &paths.socket(),
                "/v1/trace/requests",
                &PageRequest {
                    since_ms: until.millis(),
                    until_ms: load_until.millis(),
                    member: Some(member.clone()),
                    before: None,
                },
            )
            .await;
            match tail.and_then(|page| {
                crate::workload::validation::page(page, tail_window, None, &member)
            }) {
                Ok(page) => entry.scheduler_settings.extend(page.scheduler_settings),
                Err(error) => entry.errors.push(error.code),
            }
            entry.scheduler_settings.sort_by_key(|s| s.at);
            entry.scheduler_settings.dedup_by_key(|s| s.at);
        }
        // Aligned 12-hour pages preserve one-minute resolution even for 30d.
        let mut at = start.millis() / 60_000 * 60_000;
        while at < load_until.millis() {
            let end = (at + 12 * 3_600_000).min(load_until.millis());
            let load: Result<Vec<crate::load_log::buckets::Bucket>> = client::post_large(
                &paths.socket(),
                "/v1/trace/load",
                &PageRequest {
                    since_ms: at,
                    until_ms: end,
                    member: Some(member.clone()),
                    before: None,
                },
            )
            .await;
            match load {
                Ok(mut buckets) => {
                    if buckets.iter().any(|b: &crate::load_log::buckets::Bucket| {
                        b.at.millis() < at
                            || b.at.millis() >= end
                            || !b.at.millis().is_multiple_of(60_000)
                            // `seconds` counts observations, not distinct wall
                            // seconds. Timer jitter can put 61 in a minute.
                            || b.seconds > u64::from(u32::MAX)
                    }) {
                        entry.errors.push("invalid_trace_load_page".into());
                        break;
                    }
                    entry.load.append(&mut buckets);
                }
                Err(e) => {
                    entry.errors.push(e.code);
                    break;
                }
            }
            at = end;
        }
        entry.load.sort_by_key(|b| b.at);
        entry.load.dedup_by_key(|b| b.at);
    }
    trace.model();
    Ok(trace)
}

pub fn write(trace: &Trace, path: &std::path::Path) -> Result<serde_json::Value> {
    let bytes = serde_json::to_vec(trace).expect("trace serializes");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| Failure::io("Create new workload trace", e))?;
    if let Err(e) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        let _ = std::fs::remove_file(path);
        return Err(Failure::io("Write workload trace", e));
    }
    Ok(
        serde_json::json!({"trace_version":trace.trace_version,"file":path,"bytes":bytes.len(),
        "requests":trace.requests.len(),"members":trace.members.len(),"incomplete":trace.incomplete,
        "demand":trace.demand,"contention":trace.contention,"limits":trace.limits}),
    )
}
