//! Member-side synthetic execution supervisor. The only executable admitted is
//! `run /usr/bin/perl worker.pl`; trace data never becomes shell source.
use super::{
    caller::Caller,
    invalid,
    journal::{self, Journal},
    protocol::{Action, Job, Launch, Reply, valid_session, valid_workspace},
    scratch,
};
use crate::{
    failure::{Failure, Result},
    housekeeping::pass::Housekeeper,
    paths::Paths,
    scheduling::{
        request::{Placement, Urgency},
        service::Scheduler,
    },
};
use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Mutex, time::Instant};

struct Session {
    id: String,
    root: PathBuf,
    until: crate::scheduling::timestamp::Timestamp,
    seed: u64,
    expires: Instant,
    stopped: bool,
    jobs: Vec<Caller>,
    cleaned: bool,
    internal_pause: Option<crate::scheduling::service::InternalPause>,
}
pub struct Member {
    paths: Paths,
    scheduler: Arc<Scheduler>,
    housekeeper: Arc<Housekeeper>,
    load: Arc<crate::load_log::LoadLog>,
    session: Mutex<Option<Session>>,
    recovering: AtomicBool,
}
impl Member {
    pub fn new(
        paths: Paths,
        scheduler: Arc<Scheduler>,
        housekeeper: Arc<Housekeeper>,
        load: Arc<crate::load_log::LoadLog>,
    ) -> Arc<Self> {
        let this = Arc::new(Self {
            paths,
            scheduler,
            housekeeper,
            load,
            session: Mutex::new(None),
            recovering: AtomicBool::new(true),
        });
        this.start_recovery();
        this
    }
    fn start_recovery(self: &Arc<Self>) {
        self.recovering.store(true, Ordering::Release);
        let recovery = self.clone();
        tokio::spawn(async move {
            loop {
                match recovery.recover().await {
                    Ok(()) => {
                        recovery.recovering.store(false, Ordering::Release);
                        break;
                    }
                    Err(error) => {
                        crate::service_log!(
                            "{}",
                            serde_json::json!({"event":"rehearsal_cleanup_pending","code":error.code})
                        );
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }
    pub async fn call(self: &Arc<Self>, action: Action) -> Result<Reply> {
        if let Action::Certify { ref certificate } = action {
            crate::tuning_trust::certify(&self.paths, certificate)?;
            return Ok(Reply::default());
        }
        if matches!(action, Action::Trust) {
            return Ok(Reply {
                member: self.scheduler.name(),
                trust: Some(crate::tuning_trust::status(&self.paths)?),
                ..Default::default()
            });
        }
        if let Action::Inspect { since } = action {
            let (production_active, production_arrivals) =
                self.scheduler.production_activity(since)?;
            return Ok(Reply {
                member: self.scheduler.name(),
                production_active,
                production_arrivals,
                cores: self.scheduler.member_state().load.map(|l| l.cores),
                observations_ready: Some(self.load.rehearsal_observations_ready()),
                settings: self
                    .scheduler
                    .settings_history(
                        since,
                        crate::scheduling::timestamp::Timestamp::now()
                            .after(Duration::from_millis(1)),
                    )?
                    .last()
                    .cloned(),
                ..Default::default()
            });
        }
        let id = match &action {
            Action::Prepare { session, .. }
            | Action::Launch { session, .. }
            | Action::RecordedLoad { session, .. }
            | Action::Poll { session }
            | Action::Stop { session }
            | Action::Clean { session } => session,
            _ => unreachable!(),
        };
        if !valid_session(id) {
            return Err(invalid("Invalid rehearsal session."));
        }
        if matches!(action, Action::Clean { .. }) {
            return self.clean(id).await;
        }
        let mut state = self.session.lock().await;
        if let Action::Prepare {
            session,
            limit_ms,
            workspaces,
            seed,
        } = action
        {
            if self.recovering.load(Ordering::Acquire) {
                return Err(invalid(
                    "Recovery of an earlier rehearsal is still pending.",
                ));
            }
            if state.as_ref().is_some_and(|s| !s.cleaned) {
                return Err(invalid(
                    "Another rehearsal is already active on this member.",
                ));
            }
            if !(1..=3_600_000).contains(&limit_ms)
                || workspaces.len() > 10_000
                || workspaces.iter().any(|w| !valid_workspace(w))
            {
                return Err(invalid("Invalid rehearsal lifetime or workspace identity."));
            }
            let expires = Instant::now() + Duration::from_millis(limit_ms);
            let until = crate::scheduling::timestamp::Timestamp::now()
                .after(Duration::from_millis(limit_ms));
            let internal_pause = self.scheduler.pause_internal()?;
            self.scheduler.drain_internal(expires).await?;
            let cores = self
                .scheduler
                .member_state()
                .load
                .map(|l| l.cores)
                .ok_or_else(|| invalid("Calibration requires core counts."))?;
            let calibration = super::worker::calibrate(cores).await?;
            if Instant::now() >= expires {
                return Err(invalid(
                    "Rehearsal preparation reached its execution limit.",
                ));
            }
            let root = scratch::create(&session, &self.scheduler.name())?;
            Journal {
                session: session.clone(),
                root: root.path().to_owned(),
                until,
                callers: Vec::new(),
            }
            .save(&self.paths)?;
            if let Err(error) = scratch::populate(root.path(), workspaces, expires).await {
                // Keep the journal responsible if partial-directory deletion
                // fails too; never overwrite a still-pending cleanup on retry.
                let _ = root.keep();
                self.start_recovery();
                return Err(error);
            }
            let scratch = root.path().to_string_lossy().into_owned();
            self.load.begin_rehearsal(session.clone());
            *state = Some(Session {
                id: session.clone(),
                root: root.keep(),
                until,
                seed,
                expires,
                stopped: false,
                cleaned: false,
                internal_pause: Some(internal_pause),
                jobs: Vec::new(),
            });
            // A member-owned watchdog bounds work even if the orchestrating CLI
            // disappears. It cleans only this session's copies and scratch.
            let this = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep_until(expires).await;
                // A caller on another member may still be closing its link.
                // Retry until the exclusive copy/store reservation becomes free.
                loop {
                    if this.clean(&session).await.is_ok() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            });
            return Ok(Reply {
                member: self.scheduler.name(),
                scratch: Some(scratch),
                calibration: Some(calibration),
                ..Default::default()
            });
        }
        let session = state
            .as_mut()
            .filter(|s| s.id == *id)
            .ok_or_else(|| invalid("This rehearsal is no longer active."))?;
        match action {
            Action::RecordedLoad { since, until, .. } => {
                return Ok(Reply {
                    member: self.scheduler.name(),
                    recorded_load: Some(self.load.rehearsal(&session.id, since, until)?),
                    ..Default::default()
                });
            }
            Action::Launch { request, .. } => {
                if session.stopped || Instant::now() >= session.expires {
                    return Err(invalid("The rehearsal has stopped or reached its limit."));
                }
                validate(&request)?;
                if !super::belongs(&session.id, Some(&request.label)) {
                    return Err(invalid("Synthetic label does not belong to this session."));
                }
                if session.jobs.len() >= 10_000
                    || session.jobs.iter().any(|j| j.index() == request.index)
                {
                    return Err(invalid("Duplicate or excessive rehearsal submission."));
                }
                session.jobs.push(Caller::start(
                    &self.paths,
                    &session.root,
                    session.seed,
                    session.until,
                    request,
                )?);
                Journal {
                    session: session.id.clone(),
                    root: session.root.clone(),
                    until: session.until,
                    callers: session.jobs.iter().filter_map(Caller::identity).collect(),
                }
                .save(&self.paths)?;
            }
            Action::Stop { .. } => {
                stop(session).await?;
                self.settle_groups(
                    &session.id,
                    &session
                        .jobs
                        .iter()
                        .filter_map(Caller::identity)
                        .collect::<Vec<_>>(),
                )
                .await?;
            }
            Action::Poll { .. } => (),
            _ => unreachable!(),
        }
        let jobs = poll(session)?;
        Ok(Reply {
            member: self.scheduler.name(),
            jobs,
            ..Default::default()
        })
    }
    async fn settle_groups(
        &self,
        id: &str,
        callers: &[crate::scheduling::processes::ProcessIdentity],
    ) -> Result<()> {
        let records = self.scheduler.active_records()?;
        let mut groups = callers.to_vec();
        groups.extend(
            records
                .iter()
                .filter(|r| {
                    super::belongs(id, r.label.as_deref())
                        && r.calling_member == self.scheduler.name()
                        && r.placed
                            .as_ref()
                            .is_none_or(|p| p.side != crate::scheduling::placed::Side::Member)
                })
                .map(|r| r.requested_by),
        );
        groups.sort();
        groups.dedup();
        for caller in &groups {
            // A killed caller can leave its local worker alive. Durable command
            // bindings supply a surviving witness for that same private group.
            let witnesses: Vec<_> = std::iter::once(*caller)
                .chain(
                    records
                        .iter()
                        .filter(|r| {
                            r.requested_by == *caller && super::belongs(id, r.label.as_deref())
                        })
                        .flat_map(|r| r.processes.iter().copied()),
                )
                .collect();
            let alive = || {
                crate::macos::process_group::members(caller.pid)
                    .iter()
                    .any(|m| witnesses.contains(&m.identity))
            };
            if alive() {
                crate::macos::process_group::signal(caller.pid, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while alive() {
                if Instant::now() >= deadline {
                    crate::macos::process_group::signal(caller.pid, libc::SIGKILL);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        Ok(())
    }
    async fn recover(&self) -> Result<()> {
        let Some(state) = Journal::read(&self.paths, &self.scheduler.name())? else {
            return Ok(());
        };
        self.settle_groups(&state.session, &state.callers).await?;
        self.housekeeper
            .clean_rehearsal(state.session.clone())
            .await?;
        match fs::symlink_metadata(&state.root) {
            Ok(meta) if meta.is_dir() => fs::remove_dir_all(&state.root)
                .map_err(|e| Failure::io("Remove recovered synthetic workspace", e))?,
            Ok(_) => return Err(invalid("Recovered synthetic workspace changed identity.")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(Failure::io("Inspect recovered synthetic workspace", e)),
        }
        self.housekeeper.forget_rehearsal_callers(&state.session)?;
        journal::clear(&self.paths)
    }
    async fn clean(&self, id: &str) -> Result<Reply> {
        let mut state = self.session.lock().await;
        if let Some(session) = state.as_mut().filter(|s| s.id == id) {
            stop(session).await?;
            self.settle_groups(
                &session.id,
                &session
                    .jobs
                    .iter()
                    .filter_map(Caller::identity)
                    .collect::<Vec<_>>(),
            )
            .await?;
        }
        // Hosting members may have no caller-side jobs. All prepared members
        // remove their copies using catalog identities and exclusive reservations.
        self.housekeeper.clean_rehearsal(id.to_owned()).await?;
        let jobs = if let Some(session) = state.as_mut().filter(|s| s.id == id) {
            poll(session)?
        } else {
            Vec::new()
        };
        if let Some(session) = state.as_mut().filter(|s| s.id == id && !s.cleaned) {
            match fs::remove_dir_all(session.root.as_path()) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(Failure::io("Remove rehearsal scratch", e)),
            }
            self.housekeeper.forget_rehearsal_callers(id)?;
            journal::clear(&self.paths)?;
            session.cleaned = true;
            session.internal_pause.take();
        }
        Ok(Reply {
            member: self.scheduler.name(),
            jobs,
            clean: true,
            ..Default::default()
        })
    }
}
fn poll(session: &mut Session) -> Result<Vec<Job>> {
    session.jobs.iter_mut().map(Caller::job).collect()
}
async fn stop(session: &mut Session) -> Result<()> {
    session.stopped = true;
    for caller in &mut session.jobs {
        caller.terminate()?;
    }
    for caller in &mut session.jobs {
        caller.settle().await?;
    }
    Ok(())
}
fn validate(r: &Launch) -> Result<()> {
    if !valid_workspace(&r.workspace)
        || !super::synthetic(Some(&r.label))
        || r.label.chars().count() > super::label_limit(&r.label)
        || r.label.chars().any(char::is_control)
        || r.size.millis() == 0
        || r.duration_ms == 0
        || r.duration_ms > 3_600_000
        || !(0..=255).contains(&r.exit_code)
        || r.demand.is_empty()
        || r.demand.len() > 64
        || r.demand.iter().any(|(m, c)| {
            m.parse::<crate::roster::MemberName>().is_err()
                || !c.is_finite()
                || !(0.0..=256.0).contains(c)
        })
    {
        return Err(invalid("Invalid synthetic request properties."));
    }
    let work = r
        .work
        .as_ref()
        .ok_or_else(|| invalid("Synthetic work lacks calibration."))?;
    if work
        .ideal_ms
        .is_some_and(|v| !v.is_finite() || v <= 0.0 || v > 3_600_000_000.0)
        || work.iterations > 1_000_000_000_000_000
        || work.rates.len() != r.demand.len()
        || r.demand.keys().any(|m| {
            work.rates
                .get(m)
                .is_none_or(|v| !v.is_finite() || *v <= 0.0)
        })
        || work.uncontended_ms.len() != r.demand.len()
        || work
            .uncontended_ms
            .values()
            .any(|v| !v.is_finite() || *v <= 0.0)
    {
        return Err(invalid("Invalid fixed-work calibration."));
    }
    match r.urgency {
        Urgency::Interactive { .. } if r.hold_ms.is_none_or(|h| h > 86_400_000) => {
            return Err(invalid(
                "A synthetic interactive request needs its resolved hold limit.",
            ));
        }
        Urgency::Deferrable { .. } if r.placement != Placement::Pinned => {
            return Err(invalid("Deferrable work must be pinned."));
        }
        _ => (),
    }
    Ok(())
}
