//! Asking a person before syncing secret-looking files, on the master: holding
//! such changes back at every scan, showing what waits, and issuing a change once
//! a person confirms it (`consent`).
use super::{ConfigSync, blocking, log, round};
use crate::config_sync::{
    consent::{self, Confirmed},
    manifest::{Manifest, Tree},
    report::SyncSummary,
};
use elsecall_contract::failure::{Failure, Result};
use elsecall_fleet::config_sync_model::{consent::ConsentRequest, synced_path::SyncedPath};
use std::collections::BTreeMap;

impl ConfigSync {
    /// `trees` with every change that waits for consent held back, as
    /// `consent::hold` decides against `issued`. The requests replace those a
    /// status shows.
    pub(super) async fn hold_for_consent(
        &self,
        issued: &Manifest,
        trees: BTreeMap<SyncedPath, Tree>,
        confirmed: Option<(SyncedPath, String)>,
    ) -> Result<BTreeMap<SyncedPath, Tree>> {
        let replica = self.replica.clone();
        let issued = issued.clone();
        let (trees, requests) = blocking(move || {
            let mut trees = trees;
            let confirmed = confirmed
                .as_ref()
                .map(|(path, consent_id)| Confirmed { path, consent_id });
            let requests = consent::hold(&issued, &mut trees, &*replica, confirmed)?;
            Ok((trees, requests))
        })
        .await?;
        let mut waiting = self.waiting.lock().expect("waiting lock");
        if *waiting != requests && !requests.is_empty() {
            log(
                "config_sync_consent_waiting",
                serde_json::json!({ "paths": requests.iter().map(|r| r.path.to_string()).collect::<Vec<_>>() }),
            );
        }
        *waiting = requests;
        Ok(trees)
    }

    /// The requests a status shows: those for paths still synced.
    pub(super) fn waiting_consent(&self, summary: &SyncSummary) -> Vec<ConsentRequest> {
        self.waiting
            .lock()
            .expect("waiting lock")
            .iter()
            .filter(|request| summary.paths.iter().any(|p| p.path == request.path))
            .cloned()
            .collect()
    }

    /// What waits for a person in `path`; with `confirm` naming the request
    /// shown, issues that change and pushes it. With `always`, later changes to
    /// those files sync without asking. A person decides this, never an agent:
    /// the command line asks for a terminal before sending `confirm`.
    pub async fn consent(
        &self,
        path: SyncedPath,
        confirm: Option<String>,
        always: bool,
    ) -> Result<serde_json::Value> {
        let _activity = self.activity.enter()?;
        let issuing = self.issuing.lock().await;
        let (roster, own) = self.require_master()?;
        let Some(manifest) = self
            .replica
            .manifest()
            .filter(|m| m.paths.contains_key(&path))
        else {
            return Err(Failure::new(
                "not_enabled",
                format!("{path} is not synced."),
                "Run elsecall sync status to see what is synced.",
            ));
        };
        // A member that just became master restores its replica first, so its
        // own drift is never shown, or confirmed, as the fleet's change.
        self.ensure_source().await?;
        let replica = self.replica.clone();
        blocking(move || replica.reconcile_master()).await?;
        let active = self.active(&manifest)?;
        let replica = self.replica.clone();
        let scanned = blocking(move || replica.scan(&active)).await?;
        let trees = Self::with_paused(&manifest, scanned);
        self.hold_for_consent(&manifest, trees.clone(), None)
            .await?;
        let request = self
            .waiting
            .lock()
            .expect("waiting lock")
            .iter()
            .find(|r| r.path == path)
            .cloned();
        let Some(request) = request else {
            return Ok(serde_json::json!({
                "status": "nothing_waiting",
                "path": path,
                "next_step": "Nothing in this path waits for consent; elsecall sync push syncs it.",
            }));
        };
        let confirm_command = |always: bool| {
            format!(
                "elsecall sync consent {path} --confirm {}{}",
                request.consent_id,
                if always { " --always" } else { "" }
            )
        };
        let Some(confirm) = confirm else {
            return Ok(serde_json::json!({
                "status": "waiting",
                "request": request,
                "next_step": format!(
                    "A person reviews the files, then runs in their own terminal: {} (add --always to sync changes to these files automatically from now on).",
                    confirm_command(false)
                ),
            }));
        };
        if confirm != request.consent_id {
            return Err(Failure::new(
                "consent_changed",
                "That ID does not match what waits now (other files, or other content); nothing was synced.",
                format!("Review the files again, then run: {}", confirm_command(always)),
            )
            .with_details(serde_json::json!({ "request": request })));
        }
        self.refuse_while_held()?;
        let trees = self
            .hold_for_consent(&manifest, trees, Some((path.clone(), confirm)))
            .await?;
        let service = match manifest.service {
            Some(_) => Some(self.scan_service(&roster)?),
            None => None,
        };
        let replica = self.replica.clone();
        let by = own.clone();
        let (consented_path, files) = (path.clone(), request.files.clone());
        let manifest = blocking(move || {
            let names = files.into_iter().map(|f| f.file).filter(|_| always);
            replica.issue_consenting(trees, by, service, &consented_path, names)
        })
        .await?;
        log(
            "config_sync_consented",
            serde_json::json!({"generation": manifest.generation, "path": path.to_string(), "always": always}),
        );
        drop(issuing);
        let deliveries = self.push_all(&roster, &manifest).await;
        Ok(serde_json::json!({
            "status": "confirmed",
            "path": path,
            "always": always,
            "files": request.files,
            "round": round(own, &manifest, deliveries),
        }))
    }
}
