//! The master's provisioning loop: every minute it reads each member's drift
//! signal ([`crate::drift`]), and plans and applies the members whose plan's
//! inputs changed, that are new, whose apply finished, or that someone asked
//! for. A member awaiting provisioning whose plan comes out complete is marked
//! ready. See `docs/fleet-provisioning.md`, "Delivery 6".
//!
//! The loop installs nothing `tools apply` would not: it plans as `tools plan`
//! does and sends each member its plan's steps, holding back failed steps only
//! a person can unblock, and those whose retry is not due ([`decision`]).
//! Installs are admitted work on each member. Nothing is ever uninstalled, and
//! the master itself, which every member is kept in step with, is never
//! applied to.
pub(crate) mod decision;
pub(crate) mod report;
pub(crate) mod tracking;

use crate::{
    apply_dispatch,
    drift::{self, ApplySeen, Drift, Signal},
    install::{Applier, request::InstallStep},
    plan::{document::Document, member::MemberPlan},
    plan_api::{self, Planning, Reach},
};
use decision::{Act, Holds};
use elsecall_contract::{
    failure::{Failure, Result},
    scheduling_model::timestamp::Timestamp,
};
use elsecall_fleet::fleet::Fleet;
use elsecall_peers::{
    peer_client,
    roster::{MemberName, Readiness},
};
use hyper::Method;
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};
use tracking::{Members, Planned, Ready, Timing, Trigger};

/// How long a member may take to answer its signal.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the loop looks while an apply runs, so a member is checked, and
/// marked ready, seconds after its last step rather than a minute.
const FOLLOWING_AN_APPLY: Duration = Duration::from_secs(5);

pub struct ProvisioningLoop {
    planning: Planning,
    applier: Arc<Applier>,
    drift: Arc<Drift>,
    timing: Timing,
    shared: Mutex<Shared>,
    wake: Notify,
}

#[derive(Default)]
struct Shared {
    /// The master, when this member is it.
    master: Option<MemberName>,
    members: Members,
}

impl ProvisioningLoop {
    pub fn new(planning: Planning, applier: Arc<Applier>, drift: Arc<Drift>) -> Arc<Self> {
        Arc::new(Self {
            planning,
            applier,
            drift,
            timing: timing(),
            shared: Mutex::new(Shared::default()),
            wake: Notify::new(),
        })
    }

    /// Follows the fleet until the returned task is aborted.
    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let following = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                following.tick().await;
                let applying = following
                    .lock()
                    .members
                    .0
                    .values()
                    .any(tracking::Tracked::applying);
                let wait = if applying {
                    following.timing.poll.min(FOLLOWING_AN_APPLY)
                } else {
                    following.timing.poll
                };
                tokio::select! {
                    () = tokio::time::sleep(wait) => {}
                    () = following.wake.notified() => {}
                }
            }
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Marks `member` due now, as `elsecall tools provision` asks.
    pub(crate) fn ask(&self, member: &str) -> Result<report::Report> {
        let fleet = self.planning.fleet()?;
        let (master, members) = master_and_members(&fleet)?;
        let name = find(member, &members)?;
        if name == master {
            return Err(Failure::new(
                "invalid_request",
                format!(
                    "{master} is the master: every member is kept in step with it, so it is never provisioned."
                ),
                format!("Run elsecall tools apply --member {master} to install its own plan."),
            ));
        }
        {
            let mut shared = self.lock();
            shared.master = Some(master);
            shared
                .members
                .follow(&members.iter().map(|m| m.name.clone()).collect::<Vec<_>>());
            if let Some(tracked) = shared.members.0.get_mut(&name) {
                tracked.ask(Instant::now());
            }
        }
        self.wake.notify_one();
        self.report(Some(member))
    }

    /// What the loop knows, from memory.
    pub(crate) fn report(&self, only: Option<&str>) -> Result<report::Report> {
        let fleet = self.planning.fleet()?;
        let (master, members) = master_and_members(&fleet)?;
        if let Some(wanted) = only {
            find(wanted, &members)?;
        }
        let roster = fleet.roster().ok();
        let shared = self.lock();
        let entries = members
            .iter()
            .filter(|reach| only.is_none_or(|only| reach.name.as_str() == only))
            .map(|reach| {
                let readiness = roster
                    .as_ref()
                    .map_or(Readiness::Ready, |roster| roster.readiness(&reach.name));
                report::MemberReport::of(
                    &reach.name,
                    reach.name == master,
                    readiness,
                    shared.members.0.get(&reach.name),
                )
            })
            .collect();
        Ok(report::Report {
            master,
            following: true,
            members: entries,
        })
    }

    async fn tick(&self) {
        let Ok(fleet) = self.planning.fleet() else {
            return self.stop_following();
        };
        let Ok(((_, this_member), reaches)) = plan_api::roster_members(&fleet) else {
            return self.stop_following();
        };
        if !this_member {
            return self.stop_following();
        }
        let signals: Vec<Result<Signal>> = futures_util::future::join_all(
            reaches.iter().map(|reach| self.signal_of(&fleet, reach)),
        )
        .await;
        let Some(Ok(master_signal)) = reaches
            .iter()
            .zip(&signals)
            .find(|(reach, _)| reach.peer.is_none())
            .map(|(_, signal)| signal.clone())
        else {
            elsecall_host::service_log!(
                "{}",
                json!({"event": "provisioning_unavailable", "detail": "own drift signal unreadable"})
            );
            return;
        };
        let now = Instant::now();
        let at = Timestamp::now();
        let (due, redeliveries) = {
            let mut shared = self.lock();
            shared.master = reaches
                .iter()
                .find(|reach| reach.peer.is_none())
                .map(|reach| reach.name.clone());
            shared.members.follow(
                &reaches
                    .iter()
                    .map(|reach| reach.name.clone())
                    .collect::<Vec<_>>(),
            );
            // Pins first, so every member's want sees this minute's.
            for (reach, signal) in reaches.iter().zip(&signals) {
                let tracked = shared.members.0.get_mut(&reach.name).expect("followed");
                if let Ok(signal) = signal {
                    match &mut tracked.signal {
                        Some(known) => known.pins.clone_from(&signal.pins),
                        None => tracked.signal = Some(signal.clone()),
                    }
                }
            }
            let fleet_want = shared.members.fleet(&master_signal.tools);
            let mut due = Vec::new();
            let mut redeliveries = Vec::new();
            for (reach, signal) in reaches.iter().zip(signals) {
                let tracked = shared.members.0.get_mut(&reach.name).expect("followed");
                match signal {
                    Ok(signal) => {
                        tracked.observe(signal, &fleet_want, now);
                        tracked.signal_at = Some(at);
                    }
                    Err(failure) => tracked.lost(failure),
                }
                // The master is what every member is kept in step with.
                if reach.peer.is_none() {
                    continue;
                }
                if let Some(trigger) = tracked.due(now, &self.timing) {
                    due.push((reach.name.clone(), trigger));
                } else if tracked.redeliver(now) {
                    redeliveries.push(reach.name.clone());
                }
            }
            (due, redeliveries)
        };
        for name in redeliveries {
            self.make_ready(&fleet, &name).await;
        }
        if !due.is_empty() {
            self.plan(&fleet, &reaches, &due, now).await;
        }
    }

    fn stop_following(&self) {
        let mut shared = self.lock();
        shared.master = None;
        shared.members = Members::default();
    }

    async fn signal_of(&self, fleet: &Arc<Fleet>, reach: &Reach) -> Result<Signal> {
        match &reach.peer {
            None => self.drift.signal(true).await,
            Some(peer) => peer_client::call::<Signal>(
                fleet.key(),
                peer,
                Method::GET,
                drift::PEER_ROUTE,
                None::<&()>,
                SIGNAL_TIMEOUT,
            )
            .await
            .map_err(|error| {
                let code =
                    crate::fleet_tool_inventory::unavailable_member(&reach.name, &error).code;
                Failure::new(
                    &code,
                    format!("{} could not say whether its tools changed.", reach.name),
                    format!(
                        "Check {} with elsecall fleet; the loop asks again every minute.",
                        reach.name
                    ),
                )
            }),
        }
    }

    /// Plans the due members at once, then acts on each.
    async fn plan(
        &self,
        fleet: &Arc<Fleet>,
        reaches: &[Reach],
        due: &[(MemberName, Trigger)],
        started: Instant,
    ) {
        let names: Vec<MemberName> = due.iter().map(|(name, _)| name.clone()).collect();
        let document = match plan_api::gather_for(&self.planning, Some(names.clone())).await {
            Ok((document, _)) => document,
            Err(failure) => {
                for name in &names {
                    self.with(name, |tracked, timing| {
                        tracked.failed(failure.clone(), started, Instant::now(), timing);
                    });
                }
                return;
            }
        };
        elsecall_host::service_log!(
            "{}",
            json!({"event": "provisioning_planned",
                   "members": names.iter().map(MemberName::as_str).collect::<Vec<_>>()})
        );
        let partial = partial(&document);
        let acts = due.iter().filter_map(|(name, trigger)| {
            let reach = reaches.iter().find(|reach| &reach.name == name)?;
            let plan = document.members.iter().find(|(member, _)| member == name)?;
            Some(self.act(
                fleet,
                reach,
                &plan.1,
                Context {
                    trigger: *trigger,
                    master_xcodes: document.master_xcodes.as_deref(),
                    partial,
                    started,
                },
            ))
        });
        futures_util::future::join_all(acts).await;
    }

    async fn act(
        &self,
        fleet: &Arc<Fleet>,
        reach: &Reach,
        plan: &std::result::Result<MemberPlan, crate::fleet_tool_inventory::MemberUnavailable>,
        context: Context<'_>,
    ) {
        let name = &reach.name;
        let plan = match plan {
            Ok(plan) => plan,
            Err(unavailable) => {
                let failure = Failure::new(
                    &unavailable.code,
                    unavailable.message.clone(),
                    unavailable.hint.clone(),
                );
                return self.with(name, |tracked, timing| {
                    tracked.failed(failure, context.started, Instant::now(), timing);
                });
            }
        };
        let latest = match apply_dispatch::latest(fleet, &self.applier, reach).await {
            Ok(latest) => latest,
            Err(failure) => {
                return self.with(name, |tracked, timing| {
                    tracked.failed(failure, context.started, Instant::now(), timing);
                });
            }
        };
        if let Some(record) = latest.as_ref().filter(|record| record.running()) {
            // Someone started one since the signal was read; checked when it ends.
            let seen = ApplySeen::of(record);
            return self.with(name, |tracked, _| tracked.sent(seen));
        }
        let Some((privileges_ready, retry)) = self.read(name, |tracked| {
            (
                tracked
                    .signal
                    .as_ref()
                    .is_some_and(|signal| signal.privileges_ready),
                tracked.retry(context.trigger),
            )
        }) else {
            return;
        };
        let steps: Vec<InstallStep> = plan.steps.iter().map(InstallStep::of).collect();
        let decision = decision::decide(
            &steps,
            latest.as_ref(),
            &Holds {
                privileges_ready,
                master_xcodes: context.master_xcodes,
                retry,
                now: Timestamp::now(),
            },
        );
        let sent = match decision.act {
            Act::Nothing => Ok(None),
            Act::Apply { steps, held } => {
                let counts = (steps.len(), held.len());
                let sent = apply_dispatch::send(fleet, &self.applier, reach, steps, held).await;
                if sent.is_ok() {
                    elsecall_host::service_log!(
                        "{}",
                        json!({"event": "provisioning_applied", "member": name.as_str(),
                               "steps": counts.0, "held": counts.1})
                    );
                }
                sent
            }
        };
        let planned = Planned {
            at: Timestamp::now(),
            steps: plan.steps.len(),
            unresolved: plan.unresolved.len(),
            unread: plan.unread.len(),
            partial: context.partial,
            held: decision.held,
        };
        let complete = planned.complete();
        let readying = complete && awaiting(fleet, name);
        self.with(name, |tracked, timing| {
            tracked.settle(planned, context.started);
            match sent {
                Ok(apply) => {
                    if let Some(record) = apply.and_then(|apply| apply.accept()) {
                        tracked.sent(ApplySeen::of(&record));
                    }
                    // Marking it ready says whether this attempt succeeded.
                    if !readying {
                        tracked.succeeded();
                    }
                }
                Err(failure) => {
                    tracked.failed(failure, context.started, Instant::now(), timing);
                }
            }
        });
        if readying {
            self.make_ready(fleet, name).await;
        }
    }

    /// Marks `name` ready on every member. A refusal, as while a member is
    /// unreachable, plans it again later; a mark some member did not receive
    /// is delivered again.
    async fn make_ready(&self, fleet: &Arc<Fleet>, name: &MemberName) {
        let started = Instant::now();
        let was_awaiting = awaiting(fleet, name);
        let ready = match fleet.mark_ready(name).await {
            Ok(true) => Ok(Ready::Settled),
            Ok(false) => Err((Ready::Undelivered, undelivered(name))),
            // Already ready here: only its delivery is outstanding.
            Err(_) if !was_awaiting => Err((Ready::Undelivered, undelivered(name))),
            Err(failure) => Err((Ready::Refused, failure)),
        };
        if was_awaiting && !matches!(ready, Err((Ready::Refused, _))) {
            elsecall_host::service_log!(
                "{}",
                json!({"event": "provisioning_ready", "member": name.as_str()})
            );
        }
        self.with(name, |tracked, timing| match ready {
            Ok(settled) => tracked.readied(settled),
            Err((state, failure)) => {
                tracked.readied(state);
                tracked.failed(failure, started, Instant::now(), timing);
            }
        });
    }

    fn with(&self, name: &MemberName, change: impl FnOnce(&mut tracking::Tracked, &Timing)) {
        if let Some(tracked) = self.lock().members.0.get_mut(name) {
            change(tracked, &self.timing);
        }
    }

    fn read<T>(&self, name: &MemberName, read: impl FnOnce(&tracking::Tracked) -> T) -> Option<T> {
        self.lock().members.0.get(name).map(read)
    }
}

fn undelivered(name: &MemberName) -> Failure {
    Failure::new(
        "member_unavailable",
        format!("{name} is ready here, but not every member has heard yet."),
        "The loop delivers it again; elsecall fleet sync on the master does it now.",
    )
}

/// What acting on one member's plan needs from the plan as a whole.
struct Context<'a> {
    trigger: Trigger,
    master_xcodes: Option<&'a [crate::tool_inventory::ToolVersion]>,
    partial: bool,
    started: Instant,
}

/// Whether some pins could not be read for this plan, so a member's plan may
/// lack what they pin.
fn partial(document: &Document) -> bool {
    !document.unread_members.is_empty()
        || document.global.is_err()
        || document
            .workspaces
            .iter()
            .any(|workspace| workspace.files.is_err())
}

fn awaiting(fleet: &Fleet, name: &MemberName) -> bool {
    fleet
        .roster()
        .is_ok_and(|roster| roster.readiness(name) == Readiness::AwaitingProvisioning)
}

/// The master and every member, when this member is the master.
fn master_and_members(fleet: &Arc<Fleet>) -> Result<(MemberName, Vec<Reach>)> {
    let ((master, this_member), members) = plan_api::roster_members(fleet)?;
    if !this_member {
        return Err(Failure::new(
            "not_master",
            format!("Only the master, {master}, provisions the fleet."),
            format!(
                "Run elsecall tools provisioning on {master}, or elsecall run --member {master} -- elsecall tools provisioning."
            ),
        ));
    }
    Ok((master, members))
}

fn find(member: &str, members: &[Reach]) -> Result<MemberName> {
    members
        .iter()
        .find(|reach| reach.name.as_str() == member)
        .map(|reach| reach.name.clone())
        .ok_or_else(|| {
            let listed = members
                .iter()
                .map(|member| member.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Failure::new(
                "invalid_request",
                format!("{member:?} is not a member of this fleet; its members are {listed}."),
                "Run elsecall fleet to list members, then retry.",
            )
        })
}

/// Production timing, unless `ELSECALL_PROVISIONING_POLL_MS` shortens the
/// minute for tests: the quiet period is then one poll, the longest deferral
/// ten, and the periodic plan a hundred.
fn timing() -> Timing {
    let Some(poll) = std::env::var("ELSECALL_PROVISIONING_POLL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
    else {
        return Timing::PRODUCTION;
    };
    elsecall_host::service_log!(
        "{}",
        json!({"event": "test_seam_in_use", "detail": "ELSECALL_PROVISIONING_POLL_MS sets how often the provisioning loop looks"})
    );
    Timing {
        poll,
        quiet: poll,
        longest: poll * 10,
        periodic: poll * 100,
    }
}
