//! Changing what the fleet syncs, on the master: planning paths by dry run, and
//! enabling or disabling one once the operator confirms the plan they saw.
use super::{ConfigSync, MemberPlan, Plan, Planned, PlannedPath, blocking, not_declared, round};
use crate::config_sync::{consent::secret_looking, manifest::Tree};
use elsecall_contract::failure::{Failure, Result};
use elsecall_fleet::config_sync_model::synced_path::SyncedPath;
use elsecall_workspace::digest::Digest;
use std::collections::{BTreeMap, BTreeSet};

impl ConfigSync {
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
        let replica = self.replica.clone();
        let looked: Vec<(SyncedPath, Tree)> = scanned
            .iter()
            .map(|(path, scanned)| (path.clone(), scanned.tree.clone()))
            .collect();
        let mut secret = blocking(move || {
            looked
                .iter()
                .map(|(path, tree)| Ok((path.clone(), secret_looking(path, tree, &*replica)?)))
                .collect::<Result<BTreeMap<_, _>>>()
        })
        .await?;
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
                secret_looking: secret.remove(&path).unwrap_or_default(),
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
                    let trees = Self::with_paused(&manifest, rescanned);
                    self.hold_for_consent(&manifest, trees, None).await?
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
}
