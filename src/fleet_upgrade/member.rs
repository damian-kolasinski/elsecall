//! A member receiving an upgrade from its master: it verifies the signed manifest
//! and the content, stages the build, waits for its runs and leases to end, then
//! hands over to a helper process that installs the build and restarts the service.
//!
//! **In-flight work.** From the moment the build is staged the member refuses runs
//! other members would place on it (placement sends them elsewhere), and waits until
//! nothing runs or queues on it, up to the manifest's bound. If work remains then,
//! the upgrade of this member is `deferred`: nothing is installed, the work keeps
//! running, and the master reports what it waited for. Work is never stopped. A
//! local request made in the second between the member falling idle and the
//! restart behaves as at any service restart: a granted lease or run keeps its
//! capacity, a queued request ends with `service_unavailable`.
//!
//! **Elsecall's own work.** The service asks its own admission for housekeeping
//! and similar passes on a timer. While the member drains, that work is held: a
//! queued request does not start, so no pass begins only to be cut short by the
//! restart, and it is not waited for. The restart ends the request, and the new
//! service asks again on its own timer, so nothing is lost. Waiting for it could
//! last forever, since under sustained load or memory pressure the member admits
//! nothing and the pass is asked for again every interval. A deferred or failed
//! upgrade releases the hold. A pass already running is waited for like any other
//! work: it was admitted, so it ends by itself, and it may be midway through
//! removing copies or compacting stores, which a later pass would have to finish.
use crate::{
    build_identity::{Build, Digest},
    executable_format::{self, Architecture},
    failure::{Failure, Result},
    fleet::Fleet,
    fleet_upgrade::{
        journal::{Journal, Phase, UpgradeState},
        protocol::{
            self, MAXIMUM_EXECUTABLE, MAXIMUM_WAIT_SECONDS, Message, Os, SignedManifest, UpgradeId,
        },
    },
    member_key::PublicKey,
    roster::MemberName,
    scheduling::timestamp::Timestamp,
};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncWrite};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind {
    Run,
    Lease,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkState {
    Running,
    Queued,
}

/// Work that was running or queued on the member while it waited.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkInFlight {
    pub run_id: String,
    pub kind: WorkKind,
    pub label: Option<String>,
    pub state: WorkState,
    /// Whether another member placed it here.
    pub placed: bool,
    /// Whether Elsecall itself asked for it, such as a housekeeping pass.
    #[serde(default)]
    pub internal: bool,
}

impl WorkInFlight {
    /// Whether the upgrade waits for it: everything except Elsecall's own work that
    /// has not started, which the restart ends harmlessly.
    fn awaited(&self) -> bool {
        !(self.internal && self.state == WorkState::Queued)
    }
}

/// The member's own work, as the upgrade needs to see it.
pub trait MemberWork: Send + Sync {
    /// Runs and leases running or queued on this member, placed and internal ones
    /// included.
    fn in_flight(&self) -> Vec<WorkInFlight>;
    /// Refuses runs other members would place here until the hold is dropped.
    fn hold_placed(&self) -> Box<dyn Send>;
    /// Keeps Elsecall's own queued work from starting until the hold is dropped.
    fn hold_internal(&self) -> Box<dyn Send>;
}

/// Starts the process that installs the staged build and restarts the service. The
/// receiver completes when that process ends, which normally happens only after it
/// stopped this service.
pub trait Installer: Send + Sync {
    fn start(&self, id: &UpgradeId) -> Result<tokio::sync::oneshot::Receiver<()>>;
}

/// What a member says about its upgrades on the frozen status route: the build it
/// runs, and how its latest upgrade went.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpgradeStatus {
    pub running: Build,
    pub latest: Option<UpgradeState>,
    /// The attempt this service is receiving or running, which may not be recorded
    /// yet while its content is being checked.
    #[serde(default)]
    pub active: Option<UpgradeId>,
}

/// How long the master has to send its offer and its content.
const OFFER_TIMEOUT: Duration = Duration::from_secs(30);
const CONTENT_TIMEOUT: Duration = Duration::from_secs(180);
const DRAIN_POLL: Duration = Duration::from_millis(250);

pub struct Upgrades {
    journal: Journal,
    /// The installed executable, which a rollback restores.
    installed: PathBuf,
    fleet: Arc<Fleet>,
    work: Arc<dyn MemberWork>,
    installer: Arc<dyn Installer>,
    running: Build,
    /// Set from accepting an offer until the attempt ends on this service.
    active: Mutex<Option<UpgradeId>>,
}

fn log(event: &str, id: &UpgradeId, detail: serde_json::Value) {
    crate::service_log!(
        "{}",
        serde_json::json!({"event": event, "upgrade": id, "detail": detail})
    );
}

fn in_progress() -> Failure {
    Failure::new(
        "upgrade_in_progress",
        "This member is already being upgraded.",
        "Wait for that upgrade to finish (elsecall fleet shows its state), then retry.",
    )
}

fn unavailable(error: impl std::fmt::Display) -> Failure {
    Failure::new(
        "upgrade_failed",
        format!("The upgrade could not be staged: {error}"),
        "Run elsecall fleet upgrade again; check this member's service log if it repeats.",
    )
}

/// Closes an attempt that can no longer finish: one waiting for work when no service
/// waits for it (`waiting` is false), or one installing whose helper is gone. With
/// `only`, it closes only that attempt.
fn close_stale(
    journal: &Journal,
    running: &Build,
    waiting: bool,
    only: Option<&UpgradeId>,
) -> Result<()> {
    let Some(mut state) = journal.read()? else {
        return Ok(());
    };
    if only.is_some_and(|id| id != &state.id) {
        return Ok(());
    }
    // Held until the record is closed, so no helper can start on it meanwhile.
    let mut _helper_lock = None;
    let ended = match state.phase {
        Phase::Draining { .. } if !waiting => Some(Failure::new(
            "service_restarted",
            "The service restarted while the upgrade waited for work to end; nothing was installed.",
            "Run elsecall fleet upgrade again.",
        )),
        // The helper holds its lock for as long as it works.
        Phase::Installing => {
            _helper_lock = journal.helper_lock()?;
            _helper_lock.as_ref().map(|_| {
                Failure::new(
                    "helper_stopped",
                    format!(
                        "The helper installing the build stopped before it finished; this member runs build {}.",
                        running.build
                    ),
                    "Run elsecall doctor on this member, then elsecall fleet upgrade again.",
                )
            })
        }
        _ => None,
    };
    if let Some(error) = ended {
        log(
            "upgrade_interrupted",
            &state.id,
            serde_json::json!({"code": error.code}),
        );
        journal.advance(&mut state, Phase::Interrupted { error })?;
        journal.discard_builds();
    }
    Ok(())
}

impl Upgrades {
    /// Opens the member's upgrade record and closes an attempt that cannot finish.
    pub fn open(
        journal: Journal,
        installed: PathBuf,
        fleet: Arc<Fleet>,
        work: Arc<dyn MemberWork>,
        installer: Arc<dyn Installer>,
        running: Build,
    ) -> Result<Self> {
        journal.set_aside_unreadable();
        close_stale(&journal, &running, false, None)?;
        Ok(Self {
            journal,
            installed,
            fleet,
            work,
            installer,
            running,
            active: Mutex::new(None),
        })
    }

    /// Ends attempt `id` on this service, unless a newer one took over.
    fn end(&self, id: &UpgradeId) {
        let mut active = self.active.lock().expect("active lock");
        if active.as_ref() == Some(id) {
            *active = None;
        }
    }

    pub fn status(&self) -> Result<UpgradeStatus> {
        Ok(UpgradeStatus {
            running: self.running.clone(),
            latest: self.journal.read()?,
            active: self.active.lock().expect("active lock").clone(),
        })
    }

    /// Serves one upgrade channel the recorded master `master` (key `sender`) opened.
    pub async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
        self: Arc<Self>,
        mut channel: S,
        master: MemberName,
        sender: PublicKey,
    ) {
        match self.exchange(&mut channel, &master, &sender).await {
            Ok(Some((state, wait))) => {
                drop(channel);
                tokio::spawn(self.drain(state, wait, sender));
            }
            Ok(None) => (),
            Err(failure) => {
                let _ = protocol::send(&mut channel, &Message::Refused { failure }).await;
            }
        }
    }

    /// Receives, verifies and stages a build. `None` when nothing needs installing.
    async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
        self: &Arc<Self>,
        channel: &mut S,
        master: &MemberName,
        sender: &PublicKey,
    ) -> Result<Option<(UpgradeState, Duration)>> {
        let offer = tokio::time::timeout(OFFER_TIMEOUT, protocol::receive(channel))
            .await
            .map_err(|_| protocol::lost())??;
        let Message::Offer { manifest: signed } = offer else {
            return Err(protocol::protocol_error());
        };
        self.check(&signed, sender)?;
        let manifest = signed.manifest;
        if manifest.digest.build_id() == self.running.build {
            let running = self.running.clone();
            protocol::send(channel, &Message::AlreadyCurrent { running }).await?;
            return Ok(None);
        }
        {
            // An attempt this service runs blocks another until its record ends; it
            // may still be tidying up after that, and then the new one takes over.
            let mut active = self.active.lock().expect("active lock");
            if let Some(id) = active.as_ref()
                && self
                    .journal
                    .read()?
                    .is_none_or(|state| &state.id != id || !state.phase.terminal())
            {
                return Err(in_progress());
            }
            *active = Some(manifest.id.clone());
        }
        let staged = async {
            // No attempt runs on this service, so an open record is a dead one's.
            close_stale(&self.journal, &self.running, false, None)?;
            if self
                .journal
                .read()?
                .is_some_and(|state| !state.phase.terminal())
            {
                return Err(in_progress());
            }
            protocol::send(channel, &Message::Accepted).await?;
            let bytes = tokio::time::timeout(
                CONTENT_TIMEOUT,
                protocol::receive_blob(channel, manifest.size),
            )
            .await
            .map_err(|_| protocol::lost())??;
            let state = UpgradeState {
                id: manifest.id.clone(),
                master: master.clone(),
                from: self.running.clone(),
                to: manifest.digest.build_id(),
                digest: manifest.digest,
                phase: Phase::Draining {
                    wait_seconds: manifest.wait_seconds,
                    in_flight: Vec::new(),
                },
                updated_at: Timestamp::now(),
            };
            let this = self.clone();
            let size = manifest.size;
            let recorded = state.clone();
            tokio::task::spawn_blocking(move || this.stage(bytes, size, &recorded))
                .await
                .map_err(unavailable)??;
            log(
                "upgrade_staged",
                &state.id,
                serde_json::json!({"from": state.from.build, "to": state.to, "bytes": size}),
            );
            protocol::send(
                channel,
                &Message::Staged {
                    running: self.running.clone(),
                },
            )
            .await?;
            Ok(state)
        }
        .await;
        match staged {
            Ok(state) => Ok(Some((state, Duration::from_secs(manifest.wait_seconds)))),
            Err(error) => {
                // A failed exchange leaves no staged build and no open attempt. Only
                // a refusal for another live attempt leaves that attempt's files.
                if let Ok(Some(mut state)) = self.journal.read()
                    && state.id == manifest.id
                    && !state.phase.terminal()
                {
                    let _ = self.journal.advance(
                        &mut state,
                        Phase::Interrupted {
                            error: error.clone(),
                        },
                    );
                }
                if error.code != "upgrade_in_progress" {
                    self.journal.discard_builds();
                }
                self.end(&manifest.id);
                Err(error)
            }
        }
    }

    /// Verifies the content, then stages it and the executable it replaces.
    fn stage(&self, bytes: Vec<u8>, size: u64, state: &UpgradeState) -> Result<()> {
        if bytes.len() as u64 != size || Digest::of(&bytes) != state.digest {
            return Err(Failure::new(
                "digest_mismatch",
                "The build's content does not match the digest its master signed; nothing was installed.",
                "Run elsecall fleet upgrade again; if it repeats, the content is altered on the master or in transit.",
            ));
        }
        executable_format::require(&bytes, own_architecture()?)?;
        // A rollback restores the installed executable and expects this service's
        // build back, so both must be the same, and this service must run from it.
        let not_installed = || {
            Failure::new(
                "not_installed",
                "This member's service does not run from its Elsecall installation, so there is nothing to upgrade or roll back to.",
                "Run elsecall install on this member, so the installed service runs.",
            )
        };
        let canonical = |path: &std::path::Path| std::fs::canonicalize(path).ok();
        let running_from = std::env::current_exe().ok().and_then(|p| canonical(&p));
        if running_from.is_none() || running_from != canonical(&self.installed) {
            return Err(not_installed());
        }
        let previous = std::fs::read(&self.installed).map_err(|_| not_installed())?;
        if Digest::of(&previous).build_id() != self.running.build {
            return Err(not_installed());
        }
        self.journal.stage(&self.journal.previous(), &previous)?;
        self.journal.stage(&self.journal.incoming(), &bytes)?;
        self.journal.record(state)
    }

    /// Checks the manifest before any content is accepted.
    fn check(&self, signed: &SignedManifest, master: &PublicKey) -> Result<()> {
        let manifest = &signed.manifest;
        if !signed.verify(master) || manifest.recipient != self.fleet.public_key() {
            return Err(Failure::new(
                "manifest_unverified",
                "The upgrade's manifest is not signed by this member's master for this member.",
                "Run elsecall fleet upgrade on the master.",
            ));
        }
        let now = crate::peer_auth::now();
        if manifest.issued_at.abs_diff(now) > protocol::MANIFEST_WINDOW_SECONDS {
            return Err(Failure::new(
                "manifest_expired",
                "The upgrade's manifest was issued too long ago, or the members' clocks disagree.",
                "Check both machines' clocks, then run elsecall fleet upgrade again.",
            ));
        }
        let architecture = own_architecture()?;
        if Some(manifest.os) != Os::this() || !manifest.architectures.contains(&architecture) {
            return Err(Failure::new(
                "wrong_platform",
                format!(
                    "The build is for {} on {}; this member runs {} on {}.",
                    manifest.os.name(),
                    manifest
                        .architectures
                        .iter()
                        .map(|a| a.name())
                        .collect::<Vec<_>>()
                        .join(" and "),
                    std::env::consts::OS,
                    architecture.name()
                ),
                format!(
                    "Distribute a build for {} on {} to this member.",
                    std::env::consts::OS,
                    architecture.name()
                ),
            ));
        }
        if manifest.size == 0 || manifest.size > MAXIMUM_EXECUTABLE {
            return Err(Failure::new(
                "build_too_large",
                format!(
                    "The build is {} bytes; members accept 1 byte to {MAXIMUM_EXECUTABLE} bytes.",
                    manifest.size
                ),
                "Distribute a release build.",
            ));
        }
        if manifest.wait_seconds > MAXIMUM_WAIT_SECONDS {
            return Err(Failure::new(
                "invalid_request",
                "The upgrade waits longer than an hour for work to end.",
                "Pass --wait 1h or less.",
            ));
        }
        Ok(())
    }

    /// Waits for the member's work to end, then hands over to the installer.
    async fn drain(self: Arc<Self>, mut state: UpgradeState, wait: Duration, master: PublicKey) {
        let hold = (self.work.hold_placed(), self.work.hold_internal());
        let deadline = Instant::now() + wait;
        let mut seen: Option<Vec<WorkInFlight>> = None;
        let ended = loop {
            let mut in_flight = self.work.in_flight();
            in_flight.retain(WorkInFlight::awaited);
            if in_flight.is_empty() {
                break None;
            }
            if Instant::now() >= deadline {
                log(
                    "upgrade_deferred",
                    &state.id,
                    serde_json::json!({"in_flight": in_flight.len()}),
                );
                break Some(Phase::Deferred { in_flight });
            }
            if seen.as_ref() != Some(&in_flight) {
                let _ = self.journal.advance(
                    &mut state,
                    Phase::Draining {
                        wait_seconds: wait.as_secs(),
                        in_flight: in_flight.clone(),
                    },
                );
                seen = Some(in_flight);
            }
            tokio::time::sleep(DRAIN_POLL).await;
        };
        // The role may have moved while the member waited.
        let ended = ended.or_else(|| {
            self.fleet
                .authorize_upgrade(&master)
                .err()
                .map(|error| Phase::Interrupted { error })
        });
        let started = match ended {
            Some(phase) => Err(phase),
            None => match self.journal.advance(&mut state, Phase::Installing) {
                Err(error) => Err(Phase::Interrupted { error }),
                Ok(()) => {
                    log(
                        "upgrade_installing",
                        &state.id,
                        serde_json::json!({"to": state.to}),
                    );
                    self.installer
                        .start(&state.id)
                        .map_err(|error| Phase::Interrupted { error })
                }
            },
        };
        match started {
            Err(phase) => {
                let _ = self.journal.advance(&mut state, phase);
                self.journal.discard_builds();
            }
            Ok(helper_ended) => {
                // Placed runs stay refused until the helper stops this service. A
                // helper that ends while this service still runs installed nothing
                // (it records why); if it vanished without a word, close the attempt.
                let _ = helper_ended.await;
                let _ = close_stale(&self.journal, &self.running, true, Some(&state.id));
            }
        }
        drop(hold);
        self.end(&state.id);
    }
}

fn own_architecture() -> Result<Architecture> {
    Architecture::this().ok_or_else(|| {
        Failure::new(
            "unsupported_platform",
            "This member's architecture cannot be upgraded.",
            "Install Elsecall on it by hand.",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(state: WorkState, internal: bool) -> WorkInFlight {
        WorkInFlight {
            run_id: "r-1".into(),
            kind: WorkKind::Run,
            label: None,
            state,
            placed: false,
            internal,
        }
    }

    #[test]
    fn only_elsecalls_own_queued_work_is_not_waited_for() {
        assert!(work(WorkState::Running, false).awaited());
        assert!(work(WorkState::Queued, false).awaited());
        assert!(work(WorkState::Running, true).awaited());
        assert!(!work(WorkState::Queued, true).awaited());
    }

    #[test]
    fn work_recorded_before_the_internal_flag_reads_as_the_operators() {
        let recorded =
            r#"{"run_id":"r-1","kind":"lease","label":null,"state":"queued","placed":false}"#;
        let work: WorkInFlight = serde_json::from_str(recorded).unwrap();
        assert!(!work.internal);
        assert!(work.awaited());
    }
}
