//! A member hosting runs its master places on it: it claims the run's copy, asks
//! its scheduler to admit the run at once, holds the admission until the caller
//! attaches, and reports the runner's progress back to its records.
use crate::{
    failure::{Failure, Result},
    remote_run::leftovers::Cap,
    remote_run::protocol::Exited,
    remote_run::runner::{Assignment, Ending, Host, Reached as RunnerReached},
    scheduling::leftovers::Leftovers,
    scheduling::{
        placed::{OfferReply, PlacementOffer, Reached, SyncSummary, Warmth},
        quote::{CopyQuote, MAXIMUM_WORKSPACES, Quote, QuoteRequest},
        run_id::RunId,
        run_phases::RemoteFailurePhase,
        service::{HostedEnd, HostedExit, Scheduler},
    },
    workspace::{
        copies::Copies,
        location::{CopyLocation, Locations},
        object_sources::ObjectSources,
        sync::{self, MemberStats},
    },
};
use std::{
    collections::HashMap,
    ffi::OsString,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

/// How long an admitted run waits for its caller to attach before it lapses.
pub const ATTACH_WINDOW: Duration = Duration::from_secs(15);

pub struct PlacementHost {
    this: Weak<Self>,
    scheduler: Arc<Scheduler>,
    copies: Copies,
    locations: Locations,
    sources: ObjectSources,
    path: Option<OsString>,
    /// How long what a placed command leaves running may run after it exits.
    leftovers_cap: Cap,
    /// Admitted runs waiting for their caller to attach, with the master that
    /// placed each.
    tickets: Mutex<HashMap<RunId, (String, Assignment)>>,
}

fn not_admitted(id: &RunId) -> Failure {
    Failure::new(
        "not_found",
        format!("No admitted run {id} waits to be attached here."),
        "Request the run again; an admission lapses when nobody attaches within 15 seconds.",
    )
}

impl PlacementHost {
    pub fn new(
        scheduler: Arc<Scheduler>,
        copies: Copies,
        locations: Locations,
        sources: ObjectSources,
        path: Option<String>,
        leftovers_cap: Cap,
    ) -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            this: this.clone(),
            scheduler,
            copies,
            locations,
            sources,
            path: path.map(OsString::from),
            leftovers_cap,
            tickets: Mutex::new(HashMap::new()),
        })
    }

    /// Considers `offer` from `master`, which placement authority established.
    pub fn offer(&self, master: &str, offer: &PlacementOffer) -> Result<OfferReply> {
        let location =
            self.locations
                .locate(master, &offer.workspace.root, &offer.workspace.common_dir)?;
        let claim = self.copies.claim(&location);
        let warmth = warmth(&location);
        let setting = match location.setting {
            crate::workspace::location::Setting::Mapped => "mapped",
            crate::workspace::location::Setting::SamePath => "same-path",
        };
        let reply = self.scheduler.accept_placed(
            master,
            offer,
            claim.is_some(),
            &location.root.to_string_lossy(),
            setting,
            warmth,
        )?;
        if let (OfferReply::Accepted { .. }, Some(mut claim)) = (&reply, claim) {
            claim.begin_use();
            let id = offer.run_id.clone();
            let assignment = Assignment {
                run_id: id.clone(),
                member: self.scheduler.name(),
                calling_member: master.into(),
                location,
                claim,
                path: self.path.clone(),
                sources: self.sources.clone(),
                start_by: Instant::now() + Duration::from_millis(offer.start_within_ms),
                leftovers_cap: self.leftovers_cap,
            };
            self.tickets
                .lock()
                .expect("tickets lock")
                .insert(id.clone(), (master.into(), assignment));
            let host = self.this.clone();
            tokio::spawn(async move {
                tokio::time::sleep(ATTACH_WINDOW).await;
                if let Some(host) = host.upgrade()
                    && host
                        .tickets
                        .lock()
                        .expect("tickets lock")
                        .remove(&id)
                        .is_some()
                {
                    host.scheduler.hosted_ended(
                        &id,
                        HostedEnd::Failed {
                            code: "cancelled".into(),
                            reached: Reached::Placed,
                            exit: None,
                        },
                        None,
                    );
                }
            });
        }
        Ok(reply)
    }

    /// This member's quote for `master`'s placement decision.
    pub fn quote(&self, master: &str, request: &QuoteRequest) -> Result<Quote> {
        if request.workspaces.len() > MAXIMUM_WORKSPACES {
            return Err(crate::scheduling::request::invalid(
                "A quote may ask about at most 16 workspaces.",
            ));
        }
        // A member waiting to upgrade is left out of placement until it is back.
        if self.scheduler.hosting_held() {
            return Err(crate::failure::Failure::new(
                "member_upgrading",
                "This member is waiting for its work to end before it installs a new build, and takes no placed runs meanwhile.",
                "Placement uses the other members; retry on this member once its upgrade ends.",
            ));
        }
        let mut quote = self.scheduler.quote_state();
        for workspace in &request.workspaces {
            workspace.validate()?;
            // A workspace this member cannot host is reported as such; it does
            // not make the member look unreachable for every other request.
            quote.copies.push(
                match self
                    .locations
                    .locate(master, &workspace.root, &workspace.common_dir)
                {
                    Ok(location) => CopyQuote {
                        workspace: workspace.root.clone(),
                        warmth: warmth(&location),
                        busy: self.copies.is_held(&location.root),
                        unavailable: false,
                    },
                    Err(_) => CopyQuote {
                        workspace: workspace.root.clone(),
                        warmth: Warmth::Cold,
                        busy: false,
                        unavailable: true,
                    },
                },
            );
        }
        Ok(quote)
    }

    /// Takes the admitted run `id` for its caller, which must come from the master
    /// that placed it.
    pub fn take(&self, master: &str, id: &RunId) -> Result<Assignment> {
        let mut tickets = self.tickets.lock().expect("tickets lock");
        match tickets.get(id) {
            Some((placed_by, _)) if placed_by == master => {
                Ok(tickets.remove(id).expect("present").1)
            }
            _ => Err(not_admitted(id)),
        }
    }

    pub fn as_host(self: &Arc<Self>) -> Arc<dyn Host> {
        self.clone()
    }
}

/// A copy is warm once a sync completed into it; without one, the repository's
/// shared history may already be here from another of its worktrees.
fn warmth(location: &CopyLocation) -> Warmth {
    if location.state_file.exists() {
        Warmth::Warm
    } else if location.store.exists() {
        Warmth::ColdWithHistory
    } else {
        Warmth::Cold
    }
}

fn summary(stats: &MemberStats) -> SyncSummary {
    SyncSummary {
        duration_ms: None,
        sent_bytes: None,
        received_bytes: stats.received_bytes,
        pack_bytes: stats.pack_bytes,
        history_from_source: stats.history_from_source,
        refs: stats.refs.clone(),
        refs_only_commits: None,
        files_written: stats.written,
        files_removed: stats.removed + stats.extras_removed,
        run_changes_undone: stats.dirty,
        content_from_copy: stats.reused_local,
        content_from_history: stats.from_objects,
        full_listing: stats.full_manifest,
        copy_files: stats.files,
        copy_content_bytes: stats.content_bytes,
        phases_ms: sync::phases(None, &stats.phases),
    }
}

impl Host for PlacementHost {
    fn observation(&self, run: &RunId) -> Option<crate::workload_metadata::Observation> {
        self.scheduler
            .record(run)
            .ok()
            .and_then(|record| record.plan_observation)
    }
    fn synced(&self, run: &RunId, stats: &MemberStats) {
        self.scheduler.hosted_synced(run, summary(stats));
    }
    fn warned(&self, run: &RunId, warnings: &[String]) {
        self.scheduler.hosted_warnings(run, warnings);
    }
    fn started(&self, run: &RunId, pid: u32) {
        self.scheduler.hosted_started(run, pid);
    }
    fn leftovers(&self, run: &RunId, leftovers: Leftovers) {
        self.scheduler.hosted_leftovers(run, leftovers);
    }
    fn waiting_for_leftovers(&self, run: &RunId) {
        crate::service_log!(
            "{}",
            serde_json::json!({"event": "waiting_for_leftovers", "run_id": run})
        );
    }
    fn waited_for_leftovers(&self, run: &RunId, waited: Duration) {
        self.scheduler.hosted_waited_for_leftovers(run, waited);
    }
    fn ended(&self, run: &RunId, ending: Ending) {
        let exit = |exited: Exited| match exited {
            Exited::Code { code } => HostedExit::Code(code),
            Exited::Signal { signal } => HostedExit::Signal(signal),
        };
        let failure_phase = match &ending {
            Ending::Finished { return_failed, .. } => {
                return_failed.then_some(RemoteFailurePhase::ResultReturn)
            }
            Ending::Failed { reached, .. } => Some(match reached {
                RunnerReached::Syncing => RemoteFailurePhase::WorkspaceSync,
                // Even when the process exited, stdout/stderr may still be
                // draining and the caller may not have received Exited.
                RunnerReached::Synced | RunnerReached::Started | RunnerReached::Exited(_) => {
                    RemoteFailurePhase::Execution
                }
            }),
        };
        let end = match ending {
            Ending::Finished {
                exit: Exited::Code { code },
                cpu_ms,
                ..
            } => HostedEnd::Exited { code, cpu_ms },
            Ending::Finished {
                exit: Exited::Signal { signal },
                cpu_ms,
                ..
            } => HostedEnd::Signaled { signal, cpu_ms },
            Ending::Failed { failure, reached } => HostedEnd::Failed {
                code: failure.code,
                reached: match reached {
                    RunnerReached::Syncing => Reached::Placed,
                    RunnerReached::Synced => Reached::Synced,
                    RunnerReached::Started => Reached::Started,
                    RunnerReached::Exited(_) => Reached::Exited,
                },
                exit: match reached {
                    RunnerReached::Exited(exited) => Some(exit(exited)),
                    _ => None,
                },
            },
        };
        self.scheduler.hosted_ended(run, end, failure_phase);
    }
}
