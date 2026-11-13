//! Service configuration planning, opt-in and application status.
use super::*;
use crate::scheduling::timestamp::Timestamp;

impl ConfigSync {
    /// Loads the last accepted service settings before any work is admitted.
    pub fn service_settings(&self) -> Result<Option<crate::configuration::ServiceConfiguration>> {
        if !self.replica.manifest().is_some_and(|m| m.service.is_some()) {
            return Ok(None);
        }
        let name = self
            .fleet
            .roster()
            .ok()
            .map(|r| self.fleet.name_in(&r))
            .or_else(|| self.replica.service_member())
            .ok_or_else(|| {
                crate::service_configuration::invalid(
                    "Managed service settings have no saved member identity.",
                )
            })?;
        self.replica.configure_service(name);
        if self.fleet.roster().is_ok_and(|r| self.is_master(&r)) {
            self.replica.take_over()?;
        }
        self.replica.service_settings()
    }

    pub fn close_for_relaunch(&self) -> bool {
        self.activity.close_if_idle()
    }
    pub fn resume_sync(&self) {
        self.activity.reopen();
    }

    pub fn service_pending(&self) -> bool {
        self.replica.service_pending()
    }

    pub(super) fn scan_service(
        &self,
        roster: &Roster,
    ) -> Result<crate::config_sync::manifest::FileNode> {
        if let Some(manifest) = self.replica.manifest()
            && let Some(node) = manifest.service
        {
            let bytes = self.replica.store().read(&manifest.key, &node)?;
            let config = crate::service_configuration::Configuration::parse(&bytes)?;
            if config
                .temporary()
                .is_some_and(|t| t.until > Timestamp::now())
            {
                return Ok(node);
            }
        }
        let bytes = crate::service_configuration::read(
            &self
                .configuration
                .with_file_name(crate::service_configuration::SOURCE),
        )?;
        let config = crate::service_configuration::Configuration::parse(&bytes)?;
        if config
            .member_names()
            .any(|name| roster.member(name).is_none())
        {
            return Err(crate::service_configuration::invalid(
                "A service configuration member section does not match a roster name.",
            ));
        }
        // Store exactly the bytes validated above; never re-read a mutable source
        // between validation and issuing.
        let key = self.replica.key()?;
        let node = crate::config_sync::manifest::FileNode {
            mode: 0o600,
            size: bytes.len() as u64,
            fingerprint: key.fingerprint(&bytes),
        };
        self.replica.store().put(&key, &node, &bytes)?;
        self.replica.clear_service_rejection()?;
        Ok(node)
    }

    /// Review every member's changes over the existing sealed sync channel.
    pub async fn service_configuration(
        &self,
        confirm: Option<String>,
    ) -> Result<serde_json::Value> {
        let _activity = self.activity.enter()?;
        let (roster, own, node, base, draft, _pin, own_plan) = {
            let _issuing = self.issuing.lock().await;
            self.refuse_while_held()?;
            let (roster, own) = self.require_master()?;
            self.replica.configure_service(own.clone());
            self.ensure_source().await?;
            let node = self
                .scan_service(&roster)
                .inspect_err(|e| self.replica.service_rejected(e.clone()))?;
            let base = self.replica.manifest();
            let mut draft = self.replica.draft(
                base.as_ref().map(|m| m.paths.clone()).unwrap_or_default(),
                own.clone(),
            )?;
            draft.service = Some(node);
            let pin = self.replica.pin(&draft);
            let own_plan = self.replica.plan_service(&draft)?;
            (roster, own, node, base, draft, pin, own_plan)
        };
        let others = self.others(&roster);
        let answers = futures_util::future::join_all(others.iter().map(|member| async {
            let _exchange = self.exchanges.lock(&member.name).await;
            self.require_master()?;
            let answer = tokio::time::timeout(
                EXCHANGE_TIMEOUT,
                self.exchange_offer(member, &draft, Purpose::ServicePlan),
            )
            .await
            .unwrap_or_else(|_| Err(peer_client::unavailable(member)))?;
            match answer {
                Message::ServicePlanned { plan } => Ok(plan),
                Message::Failed { failure } => Err(failure),
                _ => Err(protocol::protocol_error()),
            }
        }))
        .await;
        let complete = answers.iter().all(Result::is_ok);
        let mut members = vec![serde_json::json!({"member": own, "settings": own_plan})];
        for (member, answer) in others.iter().zip(answers) {
            members.push(match answer {
                Ok(plan) => serde_json::json!({"member": member.name, "settings": plan}),
                Err(error) => serde_json::json!({"member": member.name, "error": error}),
            });
        }
        let base_id = base.as_ref().map(|m| (m.generation, m.id()));
        let plan_id = self
            .replica
            .key()?
            .digest_labelled(
                "service-plan",
                &serde_json::to_vec(&(&roster, node, &base_id, &members)).expect("plan serializes"),
            )
            .short();
        let plan = serde_json::json!({
            "plan_id": plan_id, "master": own, "complete": complete,
            "fingerprint": node.fingerprint.short(), "members": members,
            "local_config": "preserved_unused_while_managed", "application": "automatic_restart_when_idle",
            "next_step": "Review changed sections and defaults/per-member settings in fleet-config.toml, then repeat sync service --confirm <plan_id>. No values are shown here."
        });
        let Some(confirm) = confirm else {
            return Ok(serde_json::json!({"changed": false, "plan": plan}));
        };
        if confirm != plan_id {
            return Err(Failure::new("plan_changed", "The settings, member baselines, roster or base generation changed; nothing was enabled.", "Run elsecall sync service and confirm its new plan.").with_details(plan));
        }
        if !complete {
            return Err(Failure::new("service_plan_incomplete", "Every member must validate the service configuration before migration.", "Fix rejected settings or bring members online and run elsecall sync service again.").with_details(plan));
        }
        let manifest = {
            let _issuing = self.issuing.lock().await;
            self.refuse_while_held()?;
            let (now, _) = self.require_master()?;
            let current = self.replica.manifest().map(|m| (m.generation, m.id()));
            if serde_json::to_vec(&now).expect("roster serializes")
                != serde_json::to_vec(&roster).expect("roster serializes")
                || current != base_id
                || self.scan_service(&now)? != node
            {
                return Err(Failure::new(
                    "plan_changed",
                    "The master changed during the plan; nothing was enabled.",
                    "Run elsecall sync service again.",
                ));
            }
            self.issue_service(&roster, draft.paths, Some(node)).await?
        };
        let deliveries = self.push_all(&roster, &manifest).await;
        Ok(serde_json::json!({"changed": true, "round": round(own, &manifest, deliveries)}))
    }
}

/// Same-user, master-only scheduling lease. It never edits fleet-config.toml.
/// Every replica carries both baseline and bounded overlay in the sealed blob.
impl ConfigSync {
    pub async fn rehearsal_settings(
        &self,
        request: crate::service_configuration::RehearsalSettings,
    ) -> Result<serde_json::Value> {
        use crate::service_configuration::{Configuration, Projection, Temporary};
        let _activity = self.activity.enter()?;
        let (roster, manifest) = {
            let _issuing = self.issuing.lock().await;
            let (roster, own) = self.require_master()?;
            if self.held() || !crate::rehearsal::protocol::valid_session(&request.session) {
                return Err(crate::rehearsal::invalid(
                    "Invalid rehearsal or master handover in progress.",
                ));
            }
            let base = self.replica.manifest().filter(|m| m.service.is_some())
                .ok_or_else(|| crate::rehearsal::invalid("A/B rehearsals require fleet-synced service settings. Enable them before comparing."))?;
            let bytes = self
                .replica
                .store()
                .read(&base.key, &base.service.expect("filtered"))?;
            let config = Configuration::parse(&bytes)?;
            if config
                .temporary()
                .is_some_and(|t| t.session != request.session && t.until > Timestamp::now())
            {
                return Err(crate::rehearsal::invalid(
                    "Another settings rehearsal is active.",
                ));
            }
            let bytes = if let Some(until) = request.until {
                if until <= Timestamp::now() {
                    return Err(crate::rehearsal::invalid(
                        "A rehearsal lease must expire in the future.",
                    ));
                }
                if config.temporary().is_some() {
                    return Err(crate::rehearsal::invalid(
                        "A settings lease is already present; restore it before comparing.",
                    ));
                }
                let mut parameters = Vec::new();
                let mut reserves = BTreeMap::new();
                for (key, value) in &request.settings {
                    if let Some(member) = key
                        .strip_prefix("member.")
                        .and_then(|k| k.strip_suffix(".reserved_cores"))
                    {
                        if roster.members().iter().all(|m| m.name.as_str() != member) {
                            return Err(crate::rehearsal::invalid(
                                "Proposal names an unenrolled member.",
                            ));
                        }
                        let reserve =
                            crate::scheduling::capacity::Cores::parse(value).ok_or_else(|| {
                                crate::rehearsal::invalid("Invalid proposal reserve.")
                            })?;
                        reserves.insert(member.to_owned(), reserve);
                    } else if key.starts_with("placement.")
                        || key.starts_with("admission.")
                        || key.starts_with("estimates.")
                    {
                        parameters.push(format!("{key}={value}"));
                    } else {
                        return Err(crate::rehearsal::invalid(
                            "Only scheduling service settings may enter a rehearsal lease.",
                        ));
                    }
                }
                let overrides =
                    crate::scheduling::parameters::Overrides::parse_settings(&parameters)
                        .map_err(crate::rehearsal::invalid)?;
                let mut members = BTreeMap::new();
                for member in roster.members() {
                    let settings = config.resolve(&member.name)?;
                    members.insert(
                        member.name.clone(),
                        Projection {
                            reserved: reserves
                                .get(member.name.as_str())
                                .copied()
                                .unwrap_or(settings.reserved),
                            parameters: overrides
                                .apply(&settings.parameters)
                                .map_err(crate::rehearsal::invalid)?,
                        },
                    );
                }
                config.with_temporary(Temporary {
                    session: request.session,
                    until,
                    members,
                    baseline_len: None,
                })?
            } else {
                if config.temporary().is_none() {
                    // A previous removal may have reached only the master.
                    // Retry delivery without issuing another generation.
                    drop(_issuing);
                    return self.deliver_rehearsal_settings(&roster, &base).await;
                }
                config.baseline_bytes()
            };
            let node = crate::config_sync::manifest::FileNode {
                mode: 0o600,
                size: bytes.len() as u64,
                fingerprint: base.key.fingerprint(&bytes),
            };
            self.replica.store().put(&base.key, &node, &bytes)?;
            let mut draft = self.replica.draft(base.paths.clone(), own)?;
            draft.service = Some(node);
            // Validate this member and each recipient before changing the master.
            self.replica.plan_service(&draft)?;
            let others = self.others(&roster);
            let validation = async {
                futures_util::future::join_all(others.iter().map(|member| async {
                    let _exchange = self.exchanges.lock(&member.name).await;
                    match self
                        .exchange_offer(member, &draft, Purpose::ServicePlan)
                        .await?
                    {
                        Message::ServicePlanned { .. } => Ok(()),
                        Message::Failed { failure } => Err(failure),
                        _ => Err(protocol::protocol_error()),
                    }
                }))
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
            };
            tokio::time::timeout(Duration::from_secs(15), validation)
                .await
                .map_err(|_| {
                    crate::rehearsal::invalid(
                        "Rehearsal settings validation timed out; no settings were issued.",
                    )
                })??;
            let manifest = self.issue_service(&roster, base.paths, Some(node)).await?;
            (roster, manifest)
        };
        self.deliver_rehearsal_settings(&roster, &manifest).await
    }

    async fn deliver_rehearsal_settings(
        &self,
        roster: &Roster,
        manifest: &Manifest,
    ) -> Result<serde_json::Value> {
        let deliveries =
            tokio::time::timeout(Duration::from_secs(15), self.push_all(roster, manifest))
                .await
                .map_err(|_| {
                    crate::rehearsal::invalid(
                        "Rehearsal settings delivery is incomplete; restore or await lease expiry.",
                    )
                })?;
        Ok(
            serde_json::json!({"generation":manifest.generation,"round":round(self.fleet.name_in(roster), manifest, deliveries)}),
        )
    }
}
