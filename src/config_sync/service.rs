//! Configuration sync as a service on one member. Nothing runs in the background:
//! sync happens on explicit events.
//!
//! On the master, enabling or disabling a path, `sync push`, the handover before a
//! master move, and every placed run (for its member) scan the enabled paths, issue a
//! generation if the master's files changed, and push it. Scanning and issuing
//! happen one at a time; exchanges happen one at a time per member, so a slow
//! member never holds up syncs to the others. Other members accept
//! pushes only from the recorded master and apply them, laying their own overrides
//! over the mirrored files each time.
#[path = "service_configuration.rs"]
mod service_configuration;

use crate::{
    config_sync::{
        manifest::{Manifest, Tree},
        member_locks::MemberLocks,
        pins::Pin,
        protocol::{self, Message, Purpose},
        replica::{ApplyAs, Replica},
        report::{Applied, PathReport, SyncRole, SyncState, SyncSummary},
        synced_path::SyncedPath,
    },
    configuration,
    failure::{Failure, Result},
    fleet::{Fleet, RoleHandover, SyncReport},
    peer_client,
    roster::{Member, MemberName, Roster},
    workspace::digest::Digest,
};
use hyper::Method;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncWrite};

/// How long one push or plan exchange with a member may take.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(120);
/// How long asking the master to push may take, its pushes included.
const PUSH_REQUEST_TIMEOUT: Duration = Duration::from_secs(150);

pub const STATUS_ROUTE: &str = "/peer/v1/config-sync/status";
pub const PUSH_ROUTE: &str = "/peer/v1/config-sync/push";

pub struct ConfigSync {
    /// The scheduler is immutable until service relaunch. Never hash desired
    /// settings here: they may still be waiting for idle application.
    effective_fingerprint: OnceLock<String>,
    activity: super::activity::Activity,
    replica: Arc<Replica>,
    fleet: Arc<Fleet>,
    /// The service configuration, read for `[sync] paths` whenever they matter.
    configuration: PathBuf,
    /// Serialises the master's scans, issues and changes to the synced set, and
    /// role changes: local work only, never held during an exchange with a
    /// member, so every push sends a whole issued generation.
    issuing: tokio::sync::Mutex<()>,
    /// One exchange at a time per member, so a slow member delays only its own
    /// syncs and never a sync to, or a run placed on, another member.
    exchanges: MemberLocks,
    /// Serialises incoming pushes on a member, so one push's cleanup of the store
    /// never removes content another is receiving.
    receiving: tokio::sync::Mutex<()>,
    /// After a handover's final push, the master issues nothing until this time,
    /// so the role moves with exactly the state every member was checked to hold.
    held_until: Mutex<Option<Instant>>,
}

/// How long a master holds still after a handover's final push.
const HANDOVER_HOLD: Duration = Duration::from_secs(30);

/// What a member asks of the master's push route.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushRequest {
    /// The push before a master move: hold still afterwards.
    #[serde(default)]
    pub handover: bool,
    /// Sync only this member, pushing only if it differs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<MemberName>,
}

/// What bringing one member up to date did.
#[derive(Debug)]
pub struct MemberSync {
    /// The generation the member now holds.
    pub generation: u64,
    /// The synced paths the master issues from its own files; paused ones are
    /// not among them, since the master's files may differ from what they hold.
    pub paths: Vec<SyncedPath>,
    /// What the push changed, when one was needed.
    pub applied: Option<Applied>,
}

/// How a push to one member went.
#[derive(Debug, Serialize, Deserialize)]
pub struct Delivery {
    pub member: MemberName,
    #[serde(flatten)]
    pub outcome: DeliveryOutcome,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum DeliveryOutcome {
    Applied { applied: Applied },
    Failed { error: Failure },
}

/// What one round of pushing did.
#[derive(Debug, Serialize, Deserialize)]
pub struct PushRound {
    pub master: MemberName,
    pub generation: Option<u64>,
    pub manifest_id: Option<String>,
    pub paths: Vec<SyncedPath>,
    pub deliveries: Vec<Delivery>,
    pub complete: bool,
    pub next_step: String,
}

/// Where a planned path stands on the master.
#[derive(Debug, Serialize)]
pub struct PlannedPath {
    pub path: SyncedPath,
    /// Listed in this member's `[sync] paths`.
    pub declared: bool,
    pub enabled: bool,
    /// Whether the master has anything at the path.
    pub present: bool,
    pub files: usize,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
    /// Files the master's own overrides decide, whose fleet values stay as issued.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub overridden_on_master: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct MemberPlan {
    pub member: MemberName,
    pub reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Failure>,
    pub paths: Vec<PathReport>,
}

/// A plan with what enabling it needs: the trees scanned for it, and the paths
/// enabled when it was made.
struct Planned {
    plan: Plan,
    trees: BTreeMap<SyncedPath, Tree>,
    enabled: BTreeSet<SyncedPath>,
}

#[derive(Debug, Serialize)]
pub struct Plan {
    /// Pass it to `--confirm` to apply exactly this plan.
    pub plan_id: String,
    pub master: MemberName,
    pub generation: Option<u64>,
    pub paths: Vec<PlannedPath>,
    pub members: Vec<MemberPlan>,
    pub next_step: String,
}

/// What `sync status` shows for one member.
#[derive(Debug, Serialize)]
pub struct MemberStatus {
    pub member: MemberName,
    pub master: bool,
    pub reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<SyncState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Failure>,
}

#[derive(Debug, Serialize)]
pub struct FleetSync {
    pub master: MemberName,
    pub generation: Option<u64>,
    pub manifest_id: Option<String>,
    /// Paths `[sync] paths` declares on this member.
    pub declared_here: Vec<SyncedPath>,
    pub members: Vec<MemberStatus>,
    pub next_step: String,
}

fn not_master(master: &MemberName) -> Failure {
    Failure::new(
        "not_master",
        format!("Only the master, {master}, plans and changes what the fleet syncs."),
        format!("Run this command on {master}."),
    )
}

fn log(event: &str, fields: serde_json::Value) {
    let mut line = serde_json::json!({ "event": event });
    if let (Some(line), serde_json::Value::Object(fields)) = (line.as_object_mut(), fields) {
        line.extend(fields);
    }
    crate::service_log!("{line}");
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work).await.unwrap_or_else(|_| {
        Err(Failure::new(
            "sync_failed",
            "Configuration sync failed unexpectedly.",
            "Run elsecall service restart, then elsecall sync status.",
        ))
    })
}

impl ConfigSync {
    pub fn new(replica: Arc<Replica>, fleet: Arc<Fleet>, configuration: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            effective_fingerprint: OnceLock::new(),
            activity: Default::default(),
            replica,
            fleet,
            configuration,
            issuing: tokio::sync::Mutex::new(()),
            exchanges: MemberLocks::default(),
            receiving: tokio::sync::Mutex::new(()),
            held_until: Mutex::new(None),
        })
    }

    fn held(&self) -> bool {
        self.held_until
            .lock()
            .expect("hold lock")
            .is_some_and(|until| Instant::now() < until)
    }

    /// The enabled paths the master's own `[sync] paths` declares; the others
    /// are paused, keeping their last issued state.
    /// An unreadable configuration is an error, never "nothing declared".
    fn active(&self, manifest: &Manifest) -> Result<Vec<SyncedPath>> {
        let declared = if let Some(node) = manifest.service {
            let bytes = self.replica.store().read(&manifest.key, &node)?;
            let config = crate::service_configuration::Configuration::parse(&bytes)?;
            let roster = self.fleet.roster()?;
            config.resolve(&self.fleet.name_in(&roster))?.sync_paths
        } else {
            self.declared()?
        };
        Ok(manifest
            .paths
            .keys()
            .filter(|p| declared.contains(p))
            .cloned()
            .collect())
    }

    /// The trees to issue: `scanned` for active paths, the manifest's for paused ones.
    fn with_paused(
        manifest: &Manifest,
        scanned: BTreeMap<SyncedPath, crate::config_sync::replica::Scanned>,
    ) -> BTreeMap<SyncedPath, Tree> {
        let mut trees = manifest.paths.clone();
        for (path, scanned) in scanned {
            trees.insert(path, scanned.tree);
        }
        trees
    }

    fn is_master(&self, roster: &Roster) -> bool {
        roster.master().key == self.fleet.public_key()
    }

    fn others(&self, roster: &Roster) -> Vec<Member> {
        roster
            .members()
            .iter()
            .filter(|m| m.key != self.fleet.public_key())
            .cloned()
            .collect()
    }

    /// This member's paths declared in `[sync] paths`, read fresh so a change
    /// needs no restart.
    fn declared(&self) -> Result<Vec<SyncedPath>> {
        if self.replica.manifest().is_some_and(|m| m.service.is_some()) {
            let bytes = crate::service_configuration::read(
                &self
                    .configuration
                    .with_file_name(crate::service_configuration::SOURCE),
            )?;
            let config = crate::service_configuration::Configuration::parse(&bytes)?;
            let roster = self.fleet.roster()?;
            return Ok(config.resolve(&self.fleet.name_in(&roster))?.sync_paths);
        }
        Ok(configuration::load(&self.configuration)?.sync_paths)
    }

    fn require_master(&self) -> Result<(Roster, MemberName)> {
        let roster = self.fleet.roster()?;
        if !self.is_master(&roster) {
            return Err(not_master(&roster.master().name));
        }
        let own = self.fleet.name_in(&roster);
        Ok((roster, own))
    }

    // The master's side.

    /// Brings the master's issued state up to date with its files: lays its own
    /// overrides, then issues a new generation if its files changed. Returns the
    /// manifest to push. During a handover hold, the checked state stays as it is.
    async fn refresh(&self, roster: &Roster, frozen: bool) -> Result<Option<Manifest>> {
        let Some(manifest) = self.replica.manifest() else {
            return Ok(None);
        };
        if frozen {
            return Ok(Some(manifest));
        }
        let replica = self.replica.clone();
        blocking(move || replica.reconcile_master()).await?;
        let service = if manifest.service.is_some() {
            match self.scan_service(roster) {
                Ok(node) => Some(node),
                Err(e) => {
                    self.replica.service_rejected(e.clone());
                    return Err(e);
                }
            }
        } else {
            None
        };
        let mut snapshot = manifest.clone();
        snapshot.service = service;
        let paths = self.active(&snapshot)?;
        let replica = self.replica.clone();
        let scanned = blocking(move || replica.scan(&paths)).await?;
        let trees = Self::with_paused(&manifest, scanned);
        if trees == manifest.paths && service == manifest.service {
            return Ok(Some(manifest));
        }
        self.issue_service(roster, trees, service).await.map(Some)
    }

    fn refuse_while_held(&self) -> Result<()> {
        if self.held() {
            return Err(Failure::new(
                "handover_in_progress",
                "The master role is moving; the synced set cannot change until it has.",
                "Run elsecall fleet to see the master, then run this on it.",
            ));
        }
        Ok(())
    }

    /// Restores this member's replica if it has just become the source.
    async fn ensure_source(&self) -> Result<()> {
        let replica = self.replica.clone();
        blocking(move || replica.take_over()).await.map(|_| ())
    }

    async fn issue(&self, roster: &Roster, trees: BTreeMap<SyncedPath, Tree>) -> Result<Manifest> {
        let service = if self.replica.manifest().is_some_and(|m| m.service.is_some()) {
            Some(self.scan_service(roster)?)
        } else {
            None
        };
        self.issue_service(roster, trees, service).await
    }

    async fn issue_service(
        &self,
        roster: &Roster,
        trees: BTreeMap<SyncedPath, Tree>,
        service: Option<crate::config_sync::manifest::FileNode>,
    ) -> Result<Manifest> {
        let own = self.fleet.name_in(roster);
        let replica = self.replica.clone();
        let manifest = blocking(move || replica.issue_with_service(trees, own, service)).await?;
        log(
            "config_sync_issued",
            serde_json::json!({"generation": manifest.generation, "manifest_id": manifest.id().short()}),
        );
        Ok(manifest)
    }

    async fn push_all(&self, roster: &Roster, manifest: &Manifest) -> Vec<Delivery> {
        let others = self.others(roster);
        let results =
            futures_util::future::join_all(others.iter().map(|m| self.deliver(m, manifest))).await;
        deliveries(others, results)
    }

    /// The master's latest issued generation (`issued` or a later one), its
    /// content pinned until the exchange that sends it ends. The role is checked
    /// again, since waiting for a member's lock can take a while.
    async fn latest(&self, issued: &Manifest) -> Result<(Manifest, Pin)> {
        let _issuing = self.issuing.lock().await;
        self.require_master()?;
        let manifest = self.replica.manifest().unwrap_or_else(|| issued.clone());
        let pin = self.replica.pin(&manifest);
        Ok((manifest, pin))
    }

    /// Pushes to `member` once no other exchange with it is in progress: the
    /// latest generation, which is `issued` or one issued since. Taken under the
    /// member's lock, so a member never goes back to an earlier generation after
    /// a pre-run sync brought it further meanwhile.
    async fn deliver(&self, member: &Member, issued: &Manifest) -> Result<Applied> {
        let _exchange = self.exchanges.lock(&member.name).await;
        let (manifest, _pin) = self.latest(issued).await?;
        self.push_to(member, &manifest).await
    }

    /// Pushes `manifest` to `member` over a sealed channel. The caller holds the
    /// member's lock and a pin on the manifest's content.
    async fn push_to(&self, member: &Member, manifest: &Manifest) -> Result<Applied> {
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(EXCHANGE_TIMEOUT, self.exchange_push(member, manifest))
            .await
            .unwrap_or_else(|_| {
                Err(crate::peer_diagnostics::operation_timeout(
                    member,
                    protocol::ROUTE,
                    started,
                ))
            });
        log(
            "config_sync_pushed",
            serde_json::json!({
                "member": member.name,
                "generation": manifest.generation,
                "outcome": match &result { Ok(_) => "applied".to_owned(), Err(e) => e.code.clone() },
            }),
        );
        self.replica.record_push(&member.name, manifest, &result);
        result
    }

    async fn exchange_push(&self, member: &Member, manifest: &Manifest) -> Result<Applied> {
        match self.exchange_offer(member, manifest, Purpose::Push).await? {
            Message::Applied { applied } => Ok(applied),
            Message::Failed { failure } => Err(failure),
            _ => Err(protocol::protocol_error()),
        }
    }

    async fn exchange_offer(
        &self,
        member: &Member,
        manifest: &Manifest,
        purpose: Purpose,
    ) -> Result<Message> {
        let mut channel = peer_client::channel(
            self.fleet.member_key(),
            member,
            protocol::ROUTE,
            protocol::PURPOSE,
            protocol::PROTOCOL,
        )
        .await?;
        protocol::send(
            &mut channel,
            &Message::Offer {
                purpose,
                manifest: manifest.clone(),
            },
        )
        .await?;
        let wanted = match protocol::receive(&mut channel).await? {
            Message::Need { fingerprints } => fingerprints,
            Message::Failed { failure } => return Err(failure),
            _ => return Err(protocol::protocol_error()),
        };
        let offered: BTreeMap<_, _> = manifest
            .files()
            .map(|file| (file.fingerprint, file))
            .collect();
        let mut sent = BTreeSet::new();
        for fingerprint in wanted {
            // Only content of this manifest is ever sent, each at most once.
            let Some(node) = offered.get(&fingerprint).copied() else {
                return Err(protocol::protocol_error());
            };
            if !sent.insert(fingerprint) {
                return Err(protocol::protocol_error());
            }
            let replica = self.replica.clone();
            let key = manifest.key.clone();
            let content = blocking(move || replica.store().read(&key, &node)).await?;
            protocol::send_blob(&mut channel, &content).await?;
        }
        protocol::send(&mut channel, &Message::Commit).await?;
        protocol::receive(&mut channel).await
    }

    async fn plan_with(&self, member: &Member, manifest: &Manifest) -> Result<Vec<PathReport>> {
        let _exchange = self.exchanges.lock(&member.name).await;
        let started = std::time::Instant::now();
        let exchange = async {
            let mut channel = peer_client::channel(
                self.fleet.member_key(),
                member,
                protocol::ROUTE,
                protocol::PURPOSE,
                protocol::PROTOCOL,
            )
            .await?;
            protocol::send(
                &mut channel,
                &Message::Offer {
                    purpose: Purpose::Plan,
                    manifest: manifest.clone(),
                },
            )
            .await?;
            match protocol::receive(&mut channel).await? {
                Message::Planned { paths } => Ok(paths),
                Message::Failed { failure } => Err(failure),
                _ => Err(protocol::protocol_error()),
            }
        };
        tokio::time::timeout(EXCHANGE_TIMEOUT, exchange)
            .await
            .unwrap_or_else(|_| {
                Err(crate::peer_diagnostics::operation_timeout(
                    member,
                    protocol::ROUTE,
                    started,
                ))
            })
    }

    /// Makes `member` hold the master's current files, pushing only if it differs:
    /// `sync push --member`. `None` when nothing is synced or the member already
    /// held them.
    pub async fn sync_member(&self, member: &MemberName) -> Result<Option<Applied>> {
        Ok(self
            .bring_up_to_date(member)
            .await?
            .and_then(|synced| synced.applied))
    }

    /// Makes `member` hold the master's current files: placed runs call this
    /// before they start there. The master issues any change to its files, asks
    /// the member which generation it holds, and pushes only if it differs, so
    /// the usual cost is a scan of the master's files and one request. `None`
    /// when nothing is synced.
    pub async fn bring_up_to_date(&self, member: &MemberName) -> Result<Option<MemberSync>> {
        let _activity = self.activity.enter()?;
        let (roster, _) = self.require_master()?;
        let target = roster
            .member(member)
            .cloned()
            .ok_or_else(|| crate::roster::not_a_member(member))?;
        let _exchange = self.exchanges.lock(&target.name).await;
        let (manifest, _pin) = {
            let _issuing = self.issuing.lock().await;
            // Waiting for the member's lock can take a while: still master?
            let (roster, _) = self.require_master()?;
            self.ensure_source().await?;
            let Some(manifest) = self.refresh(&roster, self.held()).await? else {
                return Ok(None);
            };
            let pin = self.replica.pin(&manifest);
            (manifest, pin)
        };
        let paths = self.active(&manifest)?;
        let synced = |applied| MemberSync {
            generation: manifest.generation,
            paths: paths.clone(),
            applied,
        };
        if target.key == self.fleet.public_key() {
            if self.service_pending() {
                return Err(crate::service_configuration::pending());
            }
            return Ok(Some(synced(None)));
        }
        let held = peer_client::call::<SyncSummary>(
            self.fleet.member_key(),
            &target,
            Method::GET,
            STATUS_ROUTE,
            None::<&()>,
            peer_client::PEER_TIMEOUT,
        )
        .await?;
        // Any difference there (a local edit, an override not yet laid) is put
        // right by the same push.
        if held.service.pending {
            return Err(Failure::new(
                "service_configuration_pending",
                "This member is finishing work before applying its service configuration.",
                "Retry after its work completes; it restarts automatically.",
            ));
        }
        let settled = held.service.rejection.is_none()
            && !held.applying
            && held.manifest_id == Some(manifest.id().short())
            && held.paths.iter().all(|p| p.differences.is_empty());
        if settled {
            return Ok(Some(synced(None)));
        }
        let applied = self.push_to(&target, &manifest).await?;
        if manifest.service.is_some_and(|node| {
            held.service.fingerprint.as_deref() != Some(node.fingerprint.short().as_str())
        }) {
            return Err(crate::service_configuration::pending());
        }
        Ok(Some(synced(Some(applied))))
    }

    /// Issues the master's current files if they changed, then pushes to every
    /// member: `sync push`.
    pub async fn push_now(&self) -> Result<PushRound> {
        self.push_round(false).await
    }

    /// Pushes now; for a handover, then holds still until the role has moved.
    pub async fn push_round(&self, handover: bool) -> Result<PushRound> {
        let _activity = self.activity.enter()?;
        let (roster, _) = self.require_master()?;
        // A handover first takes every member's lock, in name order, so its hold
        // starts only once no other exchange can reach a member, and every member
        // gets exactly the state that is then checked.
        let mut exchanges = Vec::new();
        if handover {
            let mut others = self.others(&roster);
            others.sort_by(|a, b| a.name.cmp(&b.name));
            for member in &others {
                exchanges.push(self.exchanges.lock(&member.name).await);
            }
        }
        let (roster, own, manifest) = {
            let _issuing = self.issuing.lock().await;
            let (roster, own) = self.require_master()?;
            self.ensure_source().await?;
            // While a handover holds the master still, a push sends what was checked.
            let frozen = !handover && self.held();
            if handover {
                *self.held_until.lock().expect("hold lock") = Some(Instant::now() + HANDOVER_HOLD);
            }
            let manifest = self.refresh(&roster, frozen).await?;
            (roster, own, manifest)
        };
        let Some(manifest) = manifest else {
            return Ok(PushRound {
                master: own,
                generation: None,
                manifest_id: None,
                paths: Vec::new(),
                deliveries: Vec::new(),
                complete: true,
                next_step:
                    "Nothing is synced. Declare paths in [sync] paths, then run elsecall sync plan."
                        .into(),
            });
        };
        if !handover {
            let deliveries = self.push_all(&roster, &manifest).await;
            return Ok(round(own, &manifest, deliveries));
        }
        let _pin = self.replica.pin(&manifest);
        let others = self.others(&roster);
        let results =
            futures_util::future::join_all(others.iter().map(|m| self.push_to(m, &manifest))).await;
        drop(exchanges);
        Ok(round(own, &manifest, deliveries(others, results)))
    }

    /// A dry run: how syncing `requested` (by default every declared or enabled
    /// path) would change each member, by fingerprints only.
    pub async fn plan(&self, requested: Vec<SyncedPath>) -> Result<Plan> {
        let _activity = self.activity.enter()?;
        self.planned(requested).await.map(|planned| planned.plan)
    }

    /// The plan, and the trees scanned for it. Scanning holds the master's
    /// issuing lock; asking members does not, so a slow member delays only its
    /// own exchanges.
    async fn planned(&self, requested: Vec<SyncedPath>) -> Result<Planned> {
        let issuing = self.issuing.lock().await;
        let (roster, own) = self.require_master()?;
        let declared = self.declared()?;
        let current = self.replica.manifest();
        let enabled: BTreeSet<SyncedPath> = current
            .iter()
            .flat_map(|m| m.paths.keys().cloned())
            .collect();
        let paths: Vec<SyncedPath> = if requested.is_empty() {
            let all: BTreeSet<SyncedPath> =
                declared.iter().cloned().chain(enabled.clone()).collect();
            all.into_iter().collect()
        } else {
            for path in &requested {
                if !declared.contains(path) && !enabled.contains(path) {
                    return Err(not_declared(path, &self.configuration));
                }
            }
            requested
        };
        if paths.is_empty() {
            return Err(Failure::new(
                "nothing_declared",
                "No path is declared for sync on this member.",
                format!(
                    "Add [sync] paths = [\"~/.config/…\"] to {}, then run elsecall sync plan.",
                    self.configuration.display()
                ),
            ));
        }
        let replica = self.replica.clone();
        let scan_paths = paths.clone();
        let scanned = blocking(move || replica.scan(&scan_paths)).await?;
        let trees: BTreeMap<SyncedPath, Tree> = scanned
            .iter()
            .map(|(p, s)| (p.clone(), s.tree.clone()))
            .collect();
        let replica = self.replica.clone();
        let draft_trees = trees.clone();
        let draft_by = own.clone();
        let draft = blocking(move || replica.draft(draft_trees, draft_by)).await?;
        drop(issuing);
        let others = self.others(&roster);
        let results =
            futures_util::future::join_all(others.iter().map(|m| self.plan_with(m, &draft))).await;
        let members: Vec<MemberPlan> = others
            .into_iter()
            .zip(results)
            .map(|(member, result)| match result {
                Ok(paths) => MemberPlan {
                    member: member.name,
                    reachable: true,
                    error: None,
                    paths,
                },
                Err(error) => MemberPlan {
                    member: member.name,
                    reachable: error.code != "member_unavailable",
                    error: Some(error),
                    paths: Vec::new(),
                },
            })
            .collect();
        let planned: Vec<PlannedPath> = scanned
            .into_iter()
            .map(|(path, scanned)| PlannedPath {
                declared: declared.contains(&path),
                enabled: enabled.contains(&path),
                present: scanned.tree != Tree::Absent,
                files: scanned.tree.files().len(),
                bytes: scanned.tree.bytes(),
                skipped: scanned.skipped,
                overridden_on_master: scanned.overridden,
                path,
            })
            .collect();
        let plan_id =
            Digest::of(&serde_json::to_vec(&(&planned, &members)).expect("plans serialize")).hex()
                [..16]
                .to_owned();
        let next_step = match planned.iter().find(|p| !p.enabled) {
            Some(path) => format!(
                "Review the changes, then run: elsecall sync enable {} --confirm {plan_id}",
                path.path
            ),
            None => "Every planned path is enabled; run elsecall sync push to push now.".into(),
        };
        Ok(Planned {
            plan: Plan {
                plan_id,
                master: own,
                generation: current.as_ref().map(|m| m.generation),
                paths: planned,
                members,
                next_step,
            },
            trees,
            enabled,
        })
    }

    /// Enables `path` once `confirm` names the plan the operator reviewed; without
    /// it, shows that plan.
    pub async fn enable(
        &self,
        path: SyncedPath,
        confirm: Option<String>,
    ) -> Result<serde_json::Value> {
        let _activity = self.activity.enter()?;
        let Planned {
            plan,
            trees,
            enabled,
        } = self.planned(vec![path.clone()]).await?;
        let Some(confirm) = confirm else {
            return Ok(serde_json::json!({ "status": "planned", "plan": plan,
                "next_step": format!("Review the plan, then run: elsecall sync enable {path} --confirm {}", plan.plan_id) }));
        };
        if confirm != plan.plan_id {
            return Err(Failure::new(
                "plan_changed",
                "That ID does not match the current plan (a different plan, or something changed since it was shown); nothing was enabled.",
                format!(
                    "Review the new plan, then run: elsecall sync enable {path} --confirm {}",
                    plan.plan_id
                ),
            )
            .with_details(serde_json::json!({ "plan": plan })));
        }
        if !self.declared()?.contains(&path) {
            return Err(not_declared(&path, &self.configuration));
        }
        if trees.get(&path) == Some(&Tree::Absent) {
            return Err(Failure::new(
                "path_missing",
                format!(
                    "{path} does not exist on the master, so enabling it would remove it everywhere."
                ),
                "Create it on the master first, then plan again.",
            ));
        }
        let (roster, own, manifest) = {
            let _issuing = self.issuing.lock().await;
            let (roster, own) = self.require_master()?;
            self.refuse_while_held()?;
            self.ensure_source().await?;
            // The synced set must still be the one planned against: a path
            // enabled or disabled meanwhile changes what enabling means.
            let now_enabled: BTreeSet<SyncedPath> = self
                .replica
                .manifest()
                .iter()
                .flat_map(|m| m.paths.keys().cloned())
                .collect();
            if now_enabled != enabled {
                return Err(Failure::new(
                    "plan_changed",
                    "The synced set changed while enabling (another path was enabled or disabled); nothing was enabled.",
                    format!("Run elsecall sync enable {path} again to see the current plan."),
                ));
            }
            // Issued from the master's state now: anything issued since the plan
            // is kept, and the planned path's tree is exactly what was shown.
            let mut paths = match self.replica.manifest() {
                Some(manifest) => {
                    let active = self.active(&manifest)?;
                    let replica = self.replica.clone();
                    let rescanned = blocking(move || replica.scan(&active)).await?;
                    Self::with_paused(&manifest, rescanned)
                }
                None => BTreeMap::new(),
            };
            paths.insert(path.clone(), trees[&path].clone());
            let manifest = self.issue(&roster, paths).await?;
            (roster, own, manifest)
        };
        let deliveries = self.push_all(&roster, &manifest).await;
        Ok(
            serde_json::json!({ "status": "enabled", "path": path, "round": round(own, &manifest, deliveries) }),
        )
    }

    /// Stops syncing `path` once `confirm` matches; files stay on every member.
    pub async fn disable(
        &self,
        path: SyncedPath,
        confirm: Option<String>,
    ) -> Result<serde_json::Value> {
        let _activity = self.activity.enter()?;
        let issuing = self.issuing.lock().await;
        let (roster, own) = self.require_master()?;
        let manifest = self.replica.manifest();
        let Some(manifest) = manifest.filter(|m| m.paths.contains_key(&path)) else {
            return Err(Failure::new(
                "not_enabled",
                format!("{path} is not synced."),
                "Run elsecall sync status to see what is synced.",
            ));
        };
        let id = Digest::of(format!("disable\0{path}\0{}", manifest.id().hex()).as_bytes()).hex()
            [..16]
            .to_owned();
        let effect = format!(
            "Elsecall stops syncing {path}. Every member keeps its files there as they are now, overrides and backups included; replicas of the path are dropped."
        );
        let Some(confirm) = confirm else {
            return Ok(
                serde_json::json!({ "status": "planned", "path": path, "effect": effect, "plan_id": id,
                "next_step": format!("Run: elsecall sync disable {path} --confirm {id}") }),
            );
        };
        if confirm != id {
            return Err(Failure::new(
                "plan_changed",
                "That ID does not match the current plan (a different plan, or the synced set changed since it was shown); nothing was disabled.",
                format!("Run elsecall sync disable {path} again to see the current plan."),
            ));
        }
        self.refuse_while_held()?;
        self.ensure_source().await?;
        let mut paths = manifest.paths.clone();
        paths.remove(&path);
        let manifest = self.issue(&roster, paths).await?;
        drop(issuing);
        let deliveries = self.push_all(&roster, &manifest).await;
        Ok(
            serde_json::json!({ "status": "disabled", "path": path, "effect": effect, "round": round(own, &manifest, deliveries) }),
        )
    }

    // The member's side.

    /// Serves one sync channel the recorded master opened, signed by `sender`.
    pub async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        mut channel: S,
        master: MemberName,
        sender: crate::member_key::PublicKey,
    ) {
        let Ok(_activity) = self.activity.enter() else {
            let _ = protocol::send(
                &mut channel,
                &Message::Failed {
                    failure: crate::service_configuration::pending(),
                },
            )
            .await;
            return;
        };
        let result = tokio::time::timeout(
            EXCHANGE_TIMEOUT,
            self.session(&mut channel, &master, &sender),
        )
        .await
        .unwrap_or_else(|_| {
            Err(Failure::new(
                "connection_lost",
                "The sync exchange took too long; nothing changed.",
                "Run elsecall sync push on the master.",
            ))
        });
        if let Err(failure) = result {
            log(
                "config_sync_refused",
                serde_json::json!({"master": master, "code": failure.code}),
            );
            let _ = protocol::send(&mut channel, &Message::Failed { failure }).await;
        }
    }

    async fn session<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        channel: &mut S,
        master: &MemberName,
        sender: &crate::member_key::PublicKey,
    ) -> Result<()> {
        let (purpose, manifest) = match protocol::receive(channel).await? {
            Message::Offer { purpose, manifest } => (purpose, manifest),
            _ => return Err(protocol::protocol_error()),
        };
        match purpose {
            Purpose::Plan => {
                let replica = self.replica.clone();
                let paths = blocking(move || Ok(replica.plan(&manifest))).await?;
                protocol::send(channel, &Message::Planned { paths }).await
            }
            Purpose::Push | Purpose::ServicePlan => {
                let _receiving = self.receiving.lock().await;
                let replica = self.replica.clone();
                let offered = manifest.clone();
                let missing = blocking(move || {
                    Ok(if purpose == Purpose::ServicePlan {
                        offered
                            .service
                            .into_iter()
                            .filter(|f| !replica.store().contains(&f.fingerprint))
                            .map(|f| f.fingerprint)
                            .collect()
                    } else {
                        replica.missing(&offered)
                    })
                })
                .await?;
                protocol::send(
                    channel,
                    &Message::Need {
                        fingerprints: missing.clone(),
                    },
                )
                .await?;
                let nodes: BTreeMap<_, _> = manifest
                    .files()
                    .map(|file| (file.fingerprint, file))
                    .collect();
                for fingerprint in missing {
                    let node = nodes[&fingerprint];
                    let content = protocol::receive_blob(channel, node.size as usize).await?;
                    let replica = self.replica.clone();
                    let key = manifest.key.clone();
                    blocking(move || replica.store().put(&key, &node, &content)).await?;
                }
                match protocol::receive(channel).await? {
                    Message::Commit => (),
                    _ => return Err(protocol::protocol_error()),
                }
                // The role may have moved while content arrived: the key that
                // opened the channel must still be the recorded master's.
                self.fleet.authorize_sync(sender)?;
                let roster = self.fleet.roster()?;
                self.replica.configure_service(self.fleet.name_in(&roster));
                if purpose == Purpose::ServicePlan {
                    let replica = self.replica.clone();
                    let plan = blocking(move || replica.plan_service(&manifest)).await?;
                    return protocol::send(channel, &Message::ServicePlanned { plan }).await;
                }
                let replica = self.replica.clone();
                // Applying a master's push makes this member a replica again; if it
                // becomes master later, it restores this replica before acting.
                let applied = blocking(move || {
                    replica.stand_down()?;
                    let result = replica.apply(&manifest, ApplyAs::Member);
                    if let Err(error) = &result {
                        replica.service_rejected(error.clone());
                    }
                    result
                })
                .await?;
                log(
                    "config_sync_applied",
                    serde_json::json!({"master": master, "generation": applied.generation, "backups": applied.backups()}),
                );
                protocol::send(channel, &Message::Applied { applied }).await
            }
        }
    }

    /// Records the immutable running scheduler's identity before serving requests.
    pub fn report_effective_settings(&self, fingerprint: String) {
        self.effective_fingerprint
            .set(fingerprint)
            .expect("scheduler starts once");
    }

    /// This member's own sync state.
    pub fn summary(&self) -> SyncSummary {
        let role = match self.fleet.roster() {
            Ok(roster) if self.is_master(&roster) => SyncRole::Source,
            _ => SyncRole::Replica,
        };
        self.summary_as(role)
    }

    fn summary_as(&self, role: SyncRole) -> SyncSummary {
        let mut summary = self.replica.summary(role);
        summary.service.effective_fingerprint = self.effective_fingerprint.get().cloned();
        if role == SyncRole::Source {
            summary.paused = self.summary_paused(&summary);
        }
        summary
    }

    /// Synced paths the master's own `[sync] paths` does not declare: paused.
    fn summary_paused(&self, summary: &SyncSummary) -> Vec<SyncedPath> {
        let declared = self.declared().unwrap_or_default();
        summary
            .paths
            .iter()
            .map(|p| p.path.clone())
            .filter(|p| !declared.contains(p))
            .collect()
    }

    /// Every member's sync state, each asked over the member channel.
    pub async fn fleet_status(&self) -> Result<FleetSync> {
        let roster = self.fleet.roster()?;
        let role = if self.is_master(&roster) {
            SyncRole::Source
        } else {
            SyncRole::Replica
        };
        let own_summary = {
            let sync = self.replica.clone();
            let mut summary = blocking(move || Ok(sync.summary(role))).await?;
            summary.service.effective_fingerprint = self.effective_fingerprint.get().cloned();
            if role == SyncRole::Source {
                summary.paused = self.summary_paused(&summary);
            }
            summary
        };
        let others = self.others(&roster);
        let answers = peer_client::call_each::<SyncSummary>(
            self.fleet.member_key(),
            &others,
            Method::GET,
            STATUS_ROUTE,
            None::<&()>,
        )
        .await;
        let mut found: BTreeMap<MemberName, Result<SyncSummary>> =
            others.iter().map(|m| m.name.clone()).zip(answers).collect();
        let own_name = self.fleet.name_in(&roster);
        found.insert(own_name, Ok(own_summary));
        let master_name = roster.master().name.clone();
        let master_summary = found
            .get(&master_name)
            .and_then(|r| r.as_ref().ok())
            .cloned();
        let members: Vec<MemberStatus> = roster
            .members()
            .iter()
            .map(|member| {
                let master = member.name == master_name;
                match found.remove(&member.name) {
                    Some(Ok(mut summary)) => {
                        summary.service_against(master_summary.as_ref());
                        MemberStatus {
                            member: member.name.clone(),
                            master,
                            reachable: true,
                            state: Some(summary.state_against(master_summary.as_ref())),
                            sync: Some(summary),
                            error: None,
                        }
                    }
                    other => MemberStatus {
                        member: member.name.clone(),
                        master,
                        reachable: false,
                        state: None,
                        sync: None,
                        error: Some(
                            other
                                .and_then(|r| r.err())
                                .unwrap_or_else(|| peer_client::unavailable(member)),
                        ),
                    },
                }
            })
            .collect();
        let next_step = if members.iter().any(|m| {
            m.sync
                .as_ref()
                .is_some_and(|s| s.service.rejection.is_some())
        }) {
            "A service configuration was rejected. Fix fleet-config.toml on the master, then run elsecall sync push; members keep their working settings.".into()
        } else if members
            .iter()
            .any(|m| m.sync.as_ref().is_some_and(|s| s.service.pending))
        {
            "Service configuration is waiting for running/queued work, leftovers or sync exchanges to finish; services restart automatically at idle.".into()
        } else if members
            .iter()
            .any(|m| matches!(m.state, Some(SyncState::Pending)) || !m.reachable)
        {
            format!(
                "Run elsecall sync push (it asks {master_name} to push), and check unreachable members with elsecall fleet."
            )
        } else if members
            .iter()
            .any(|m| matches!(m.state, Some(SyncState::LocallyEdited)))
        {
            "Locally edited files are backed up and replaced at the next sync; move values a member must keep into its overrides directory.".into()
        } else {
            "No action needed.".into()
        };
        Ok(FleetSync {
            generation: master_summary.as_ref().and_then(|s| s.generation),
            manifest_id: master_summary.as_ref().and_then(|s| s.manifest_id.clone()),
            master: master_name,
            declared_here: self.declared().unwrap_or_default(),
            members,
            next_step,
        })
    }

    /// Asks the master to push now, wherever this runs.
    pub async fn request_push(&self, member: Option<MemberName>) -> Result<PushRound> {
        let _activity = self.activity.enter()?;
        let roster = self.fleet.roster()?;
        if self.is_master(&roster) {
            return self
                .serve_push(PushRequest {
                    handover: false,
                    member,
                })
                .await;
        }
        let master = roster.master();
        peer_client::call(
            self.fleet.member_key(),
            master,
            Method::POST,
            PUSH_ROUTE,
            Some(&PushRequest {
                handover: false,
                member,
            }),
            PUSH_REQUEST_TIMEOUT,
        )
        .await
    }

    /// Answers a push request on the master: every member, or just one.
    pub async fn serve_push(&self, request: PushRequest) -> Result<PushRound> {
        let _activity = self.activity.enter()?;
        let Some(member) = request.member else {
            return self.push_round(request.handover).await;
        };
        let applied = self.sync_member(&member).await?;
        let (_, own) = self.require_master()?;
        let manifest = self.replica.manifest();
        let in_sync = applied.is_none();
        Ok(PushRound {
            master: own,
            generation: manifest.as_ref().map(|m| m.generation),
            manifest_id: manifest.as_ref().map(|m| m.id().short()),
            paths: manifest
                .map(|m| m.paths.keys().cloned().collect())
                .unwrap_or_default(),
            deliveries: applied
                .map(|applied| Delivery {
                    member: member.clone(),
                    outcome: DeliveryOutcome::Applied { applied },
                })
                .into_iter()
                .collect(),
            complete: true,
            next_step: if in_sync {
                format!("{member} already held the master's files; nothing was pushed.")
            } else {
                format!("{member} holds the master's files.")
            },
        })
    }

    pub fn history(&self, limit: usize) -> Result<Vec<crate::config_sync::history::Record>> {
        self.replica.history().latest(limit)
    }
}

fn deliveries(members: Vec<Member>, results: Vec<Result<Applied>>) -> Vec<Delivery> {
    members
        .into_iter()
        .zip(results)
        .map(|(member, result)| Delivery {
            member: member.name,
            outcome: match result {
                Ok(applied) => DeliveryOutcome::Applied { applied },
                Err(error) => DeliveryOutcome::Failed { error },
            },
        })
        .collect()
}

fn round(master: MemberName, manifest: &Manifest, deliveries: Vec<Delivery>) -> PushRound {
    let complete = deliveries
        .iter()
        .all(|d| matches!(d.outcome, DeliveryOutcome::Applied { .. }));
    PushRound {
        master,
        generation: Some(manifest.generation),
        manifest_id: Some(manifest.id().short()),
        paths: manifest.paths.keys().cloned().collect(),
        deliveries,
        complete,
        next_step: if complete {
            "Every member holds this generation, or a later one issued meanwhile (see each delivery's generation).".into()
        } else {
            "Members that failed keep their previous state; the master pushes again when they are reachable, or run elsecall sync push.".into()
        },
    }
}

fn not_declared(path: &SyncedPath, configuration: &std::path::Path) -> Failure {
    Failure::new(
        "not_declared",
        format!("{path} is not declared for sync on this member."),
        format!(
            "Add it to [sync] paths in {}, then run elsecall sync plan.",
            configuration.display()
        ),
    )
}

impl SyncReport for ConfigSync {
    fn summary(&self) -> SyncSummary {
        ConfigSync::summary(self)
    }
}

impl RoleHandover for ConfigSync {
    fn prepare<'a>(
        &'a self,
        roster: &'a Roster,
        to: &'a MemberName,
        force: bool,
    ) -> futures_util::future::BoxFuture<'a, Result<serde_json::Value>> {
        Box::pin(self.hand_over(roster, to, force))
    }
}

/// Follows this member's role: the moment it becomes master it restores its
/// replica (backing up local edits), so drift from before the move never becomes
/// the fleet's truth while edits made after it do; the moment it stops being
/// master it stands down, so a later return restores again.
pub struct RoleChanges(pub Arc<ConfigSync>);

impl crate::fleet::RoleObserver for RoleChanges {
    fn role_changed(&self, master: bool) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let sync = self.0.clone();
        let Ok(activity) = sync.activity.enter() else {
            return;
        };
        if let Err(error) = sync.replica.service_role_changed(master) {
            sync.replica.service_rejected(error);
        }
        runtime.spawn(async move {
            let _activity = activity;
            let _issuing = sync.issuing.lock().await;
            let replica = sync.replica.clone();
            let outcome = blocking(move || {
                if master {
                    replica
                        .take_over()
                        .map(|applied| applied.map(|a| a.backups()))
                } else {
                    replica.stand_down().map(|()| None)
                }
            })
            .await;
            log(
                "config_sync_role_changed",
                serde_json::json!({
                    "master": master,
                    "backups": outcome.as_ref().ok().copied().flatten(),
                    "error": outcome.err().map(|e| e.code),
                }),
            );
        });
    }
}

/// The handover of a member whose sync state could not be opened: it cannot
/// check that members hold the master's final state, so it refuses unless forced.
pub struct UnavailableHandover(pub Failure);

impl RoleHandover for UnavailableHandover {
    fn prepare<'a>(
        &'a self,
        _roster: &'a Roster,
        _to: &'a MemberName,
        force: bool,
    ) -> futures_util::future::BoxFuture<'a, Result<serde_json::Value>> {
        let error = self.0.clone();
        Box::pin(async move {
            if force {
                return Ok(serde_json::json!({
                    "status": "unchecked",
                    "error": error,
                    "warning": "Configuration sync is unavailable on this member, so nobody checked that members hold the master's final configuration.",
                }));
            }
            Err(Failure::new(
                "config_sync_handover_failed",
                "Configuration sync is unavailable on this member, so the master role was not moved.",
                format!(
                    "Fix it ({}: {}) and retry, run the move on another member, or pass --force.",
                    error.code, error.hint
                ),
            ))
        })
    }
}

impl ConfigSync {
    /// Before the role moves: the old master pushes its final state, and every
    /// member must then hold it. Forced, the move goes ahead regardless and says
    /// what may be lost.
    async fn hand_over(
        &self,
        roster: &Roster,
        to: &MemberName,
        force: bool,
    ) -> Result<serde_json::Value> {
        let _activity = self.activity.enter()?;
        let master = roster.master().clone();
        let round = if self.is_master(roster) {
            self.push_round(true).await
        } else {
            peer_client::call::<PushRound>(
                self.fleet.member_key(),
                &master,
                Method::POST,
                PUSH_ROUTE,
                Some(&PushRequest {
                    handover: true,
                    member: None,
                }),
                PUSH_REQUEST_TIMEOUT,
            )
            .await
        };
        let round = match round {
            Ok(round) => round,
            Err(error) if force => {
                return Ok(serde_json::json!({
                    "status": "old_master_unreachable",
                    "error": error,
                    "warning": format!(
                        "{} could not push its final configuration ({}). The replica on {to} is the truth from now on: changes {} never pushed are lost to the fleet. When {} returns as a member, files that differ there are backed up as local edits before they are replaced.",
                        master.name, error.code, master.name, master.name
                    ),
                }));
            }
            Err(error) => {
                return Err(Failure::new(
                    "config_sync_handover_failed",
                    format!(
                        "{} could not push its final configuration ({}): the master role was not moved.",
                        master.name, error.code
                    ),
                    format!("Fix it and retry, or pass --force to make the replica on {to} the truth."),
                )
                .with_details(serde_json::json!({ "error": error })));
            }
        };
        let Some(wanted) = round.manifest_id.clone() else {
            return Ok(serde_json::json!({ "status": "nothing_synced" }));
        };
        let others: Vec<Member> = roster
            .members()
            .iter()
            .filter(|m| m.name != master.name)
            .cloned()
            .collect();
        let statuses = peer_client::call_each::<SyncSummary>(
            self.fleet.member_key(),
            &others,
            Method::GET,
            STATUS_ROUTE,
            None::<&()>,
        )
        .await;
        let mut behind = Vec::new();
        for (member, status) in others.iter().zip(&statuses) {
            match status {
                Ok(s) if !s.applying && s.manifest_id.as_deref() == Some(wanted.as_str()) => (),
                Ok(_) => {
                    behind.push(serde_json::json!({"member": member.name, "reason": "not_in_sync"}))
                }
                Err(error) => {
                    behind.push(serde_json::json!({"member": member.name, "reason": error.code}))
                }
            }
        }
        if behind.is_empty() {
            return Ok(serde_json::json!({
                "status": "in_sync",
                "generation": round.generation,
                "manifest_id": wanted,
                "final_push": round,
            }));
        }
        if !force {
            return Err(Failure::new(
                "config_sync_incomplete",
                "Not every member holds the master's final configuration, so the master role was not moved.",
                "Bring them online and retry, or pass --force; members not in sync keep their previous files until the new master pushes.",
            )
            .with_details(serde_json::json!({ "not_in_sync": behind, "final_push": round })));
        }
        Ok(serde_json::json!({
            "status": "forced",
            "generation": round.generation,
            "manifest_id": wanted,
            "not_in_sync": behind,
            "warning": format!("Members not in sync get {to}'s replica when it pushes; if {to} itself was behind, changes it lacks are lost to the fleet."),
            "final_push": round,
        }))
    }
}
