//! Paired live trials and the bounded synced-settings transaction between them.
use crate::{
    client,
    failure::{Failure, Result},
    paths::Paths,
    rehearsal::{
        invalid,
        protocol::{Action, Reply},
    },
    rehearse_cli::{self, RehearseArgs},
    scheduling::{records::SchedulerSettings, timestamp::Timestamp},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};

fn proposal(path: &std::path::Path) -> Result<BTreeMap<String, String>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(32 * 1024 * 1024 + 1).read_to_end(&mut bytes))
        .map_err(|e| Failure::io("Read rehearsal proposal", e))?;
    if bytes.len() > 32 * 1024 * 1024 {
        return Err(invalid("Proposal exceeds 32 MiB."));
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("Expected a tune JSON result or canonical settings map."))?;
    let value = value.get("data").unwrap_or(&value);
    let settings = value.pointer("/best/settings").unwrap_or(value);
    let settings: BTreeMap<String, String> = serde_json::from_value(settings.clone())
        .map_err(|_| invalid("Expected a tune JSON result or canonical settings map."))?;
    if settings.len() > 128 {
        return Err(invalid("Too many proposal settings."));
    }
    Ok(settings)
}
async fn inspect(paths: &Paths, members: &[String]) -> Result<Vec<Reply>> {
    rehearse_cli::all(
        paths,
        members,
        Action::Inspect {
            since: Timestamp::now(),
        },
    )
    .await
    .into_iter()
    .collect()
}
fn same(actual: &SchedulerSettings, expected: &SchedulerSettings) -> bool {
    actual.parameters == expected.parameters && actual.capacity == expected.capacity
}
fn fingerprints(settings: &BTreeMap<String, SchedulerSettings>) -> BTreeMap<String, String> {
    settings
        .iter()
        .map(|(member, settings)| {
            (
                member.clone(),
                crate::scheduling::settings_identity::fingerprint(
                    &settings.parameters,
                    settings.capacity,
                ),
            )
        })
        .collect()
}
async fn snapshot(
    paths: &Paths,
    members: &[String],
) -> Result<BTreeMap<String, SchedulerSettings>> {
    inspect(paths, members)
        .await?
        .into_iter()
        .map(|reply| {
            Ok((
                reply.member,
                reply
                    .settings
                    .ok_or_else(|| invalid("Current fleet settings are missing."))?,
            ))
        })
        .collect()
}
fn verify_restoration(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Result<()> {
    if before != after {
        return Err(Failure::new(
            "rehearsal_restoration_failed",
            "Active fleet scheduling settings or capacity differ from before the comparison.",
            "Check sync status and fleet service.effective_fingerprint for every member; restore the original settings before tuning again.",
        ).with_details(json!({"before":before,"after":after})));
    }
    Ok(())
}
async fn active(
    paths: &Paths,
    expected: &BTreeMap<String, SchedulerSettings>,
    deadline: tokio::time::Instant,
) -> Result<()> {
    let members: Vec<_> = expected.keys().cloned().collect();
    loop {
        if let Ok(replies) = inspect(paths, &members).await
            && replies.iter().all(|r| {
                r.settings
                    .as_ref()
                    .is_some_and(|s| same(s, &expected[&r.member]))
            })
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(invalid(
                "Fleet settings activation is incomplete; the bounded lease retains automatic rollback.",
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
async fn settings(
    paths: &Paths,
    session: &str,
    until: Option<Timestamp>,
    settings: BTreeMap<String, String>,
) -> Result<Value> {
    let result: Value = client::post_large(
        &paths.socket(),
        "/v1/sync/rehearsal",
        &crate::service_configuration::RehearsalSettings {
            session: session.into(),
            until,
            settings,
        },
    )
    .await?;
    if result["round"]["complete"] != true {
        return Err(Failure::new(
            "rehearsal_settings_incomplete",
            "Rehearsal settings have not reached every member.",
            "Retry restoration; check sync status before comparing again.",
        )
        .with_details(result));
    }
    Ok(result)
}
pub async fn run(paths: &Paths, mut args: RehearseArgs) -> Result<Value> {
    let mut signals = super::cancellation::Signals::register()?;
    let proposed = if args.aa {
        BTreeMap::new()
    } else {
        proposal(&args.compare.take().expect("compare path"))?
    };
    let aa = args.aa || proposed.is_empty();
    if args.noise_output.is_some() && !aa {
        return Err(invalid(
            "--noise-output requires --aa or an unchanged empty proposal.",
        ));
    }
    let noise = args
        .noise
        .as_deref()
        .map(crate::tuning_objective::Noise::read)
        .transpose()?;
    let trace = crate::rehearse_cli::source(&args)?;
    let source_digest =
        crate::build_identity::Digest::of(&serde_json::to_vec(&trace).expect("trace serializes"));
    if let Some(noise) = &noise
        && (noise.source_digest != source_digest
            || noise.seed != args.seed
            || noise.compress != args.compress
            || noise.load_factor != args.load_factor
            || noise.build != crate::build_identity::this_build()?)
    {
        return Err(invalid(
            "A/A noise must match the source trace, build, seed, compression and load factor.",
        ));
    }
    let mut noise_file = args
        .noise_output
        .as_ref()
        .map(|path| {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)
                .map_err(|e| Failure::io("Reserve A/A noise artifact", e))
        })
        .transpose()?;
    let candidate = crate::simulation::CandidateSettings::parse(
        &trace,
        &proposed
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>(),
    )?;
    let roster: crate::trace_api::Members =
        client::post_large(&paths.socket(), "/v1/trace/members", &json!({})).await?;
    let original = snapshot(paths, &roster.members).await?;
    let before_fingerprints = fingerprints(&original);
    if let Some(noise) = &noise
        && (original.len() != noise.baseline_settings.len()
            || original.iter().any(|(m, s)| {
                noise.baseline_settings.get(m).is_none_or(|n| {
                    !same(s, n) || s.capacity.weighted_cores != n.capacity.weighted_cores
                })
            }))
    {
        return Err(invalid(
            "A/A noise baseline settings/capacity no longer match the fleet.",
        ));
    }
    let mut expected = original.clone();
    for (name, snapshot) in &mut expected {
        snapshot.parameters = candidate.parameters(&snapshot.parameters)?;
        snapshot.capacity = candidate.capacity(name, &snapshot.capacity)?;
    }
    let baseline = rehearse_cli::single(
        paths,
        args.clone(),
        &BTreeMap::new(),
        &mut signals,
        &trace,
        noise.as_ref().map(|n| &n.calibration),
        noise.as_ref().map(|n| &n.ideals),
    )
    .await?;
    if baseline["recording_complete"] != true
        || baseline["stop_reason"] != "completed"
        || baseline["cleanup_complete"] != true
        || !baseline["rehearsal_error"].is_null()
    {
        let after = snapshot(paths, &roster.members)
            .await
            .map(|s| fingerprints(&s));
        let restoration = after
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|after| verify_restoration(&before_fingerprints, after));
        let error = restoration
            .as_ref()
            .err()
            .map(|e| json!(e))
            .unwrap_or_else(|| {
                if baseline["rehearsal_error"].is_null() {
                    json!(invalid(
                        "Baseline rehearsal is incomplete; no proposal was applied."
                    ))
                } else {
                    baseline["rehearsal_error"].clone()
                }
            });
        return Ok(
            json!({"aa":aa,"noise_floor":null,"baseline":baseline,"proposal":null,"settings_restored":restoration.is_ok(),"effective_fingerprints":{"before":before_fingerprints,"after":after.ok()},"rehearsal_error":error,"cleanup_complete":restoration.is_ok() && baseline["cleanup_complete"] == true,"comparison":{"status":"incomplete","accepted":null,"wins_within_noise":null}}),
        );
    }
    let frozen_calibration = serde_json::from_value(baseline["calibration"].clone())
        .map_err(|_| invalid("Missing baseline calibration."))?;
    let frozen_ideals: BTreeMap<String, Option<f64>> = baseline["requests"]
        .as_array()
        .expect("mappings")
        .iter()
        .map(|r| {
            (
                r["original_run_id"].as_str().expect("source ID").to_owned(),
                r["ideal_ms"].as_f64(),
            )
        })
        .collect();
    let session = baseline["session"].as_str().expect("session");
    let service: BTreeMap<_, _> = proposed
        .iter()
        .filter(|(k, _)| !k.starts_with("label.") && *k != "hold_limit")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let needs_lease = !service.is_empty();
    // Restore even when the apply response was lost. The replica's expiry also
    // restores effective settings after SIGKILL or controller disconnection.
    let until = Timestamp::now().after(args.limit + Duration::from_secs(25));
    let trial = async {
        if !needs_lease {
            return Ok(());
        }
        settings(paths, session, Some(until), service).await?;
        active(
            paths,
            &expected,
            tokio::time::Instant::now() + Duration::from_secs(20),
        )
        .await?;
        Ok(())
    };
    let applied = tokio::select! {
        biased;
        _=signals.cancelled()=>Err(invalid("A/B rehearsal interrupted.")),
        result=trial=>result,
    };
    let result = if let Err(error) = applied {
        Err(error)
    } else {
        if let Some(output) = &args.output {
            let mut name = output.as_os_str().to_owned();
            name.push(".proposal.json");
            args.output = Some(name.into());
        }
        rehearse_cli::single(
            paths,
            args,
            &proposed,
            &mut signals,
            &trace,
            Some(&frozen_calibration),
            Some(&frozen_ideals),
        )
        .await
    };
    // Do not cancel B's own cleanup on a signal: its supervisor/watchdog bounds
    // synthetic work; restoration waits for the normal idle settings boundary.
    let restore_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut restoration = None;
    if !needs_lease {
        restoration = Some(Ok(()));
    }
    while needs_lease && tokio::time::Instant::now() < restore_deadline {
        match settings(paths, session, None, BTreeMap::new()).await {
            Ok(_) => {
                restoration = Some(active(paths, &original, restore_deadline).await);
                break;
            }
            Err(error) => {
                restoration = Some(Err(error));
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    let after = snapshot(paths, &roster.members)
        .await
        .map(|s| fingerprints(&s));
    let restoration = restoration
        .unwrap_or_else(|| Err(invalid("Settings restoration timed out.")))
        .and_then(|()| {
            after
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|after| verify_restoration(&before_fingerprints, after))
        });
    let restored = restoration.is_ok();
    let (mut proposal, error) = match result {
        Ok(v) => (Some(v), None),
        Err(e) => (None, Some(e)),
    };
    if let Some(proposal) = &mut proposal {
        crate::simulation::constrain_failures(
            &mut proposal["metrics"],
            baseline["metrics"]["scheduling_objective"]["failures"]["count"].as_u64(),
            "measured baseline arm",
        );
    }
    let mut report = comparison(&baseline, proposal.as_ref(), noise.as_ref());
    if !restored {
        report["accepted"] = Value::Null;
        report["wins_within_noise"] = Value::Null;
    }
    let noise_floor = if aa && restored && report["status"] == "compared" {
        let a = &baseline["metrics"]["scheduling_objective"]["overhead"];
        let b = &proposal.as_ref().expect("complete pair")["metrics"]["scheduling_objective"]["overhead"];
        a["mean"]
            .as_f64()
            .zip(b["mean"].as_f64())
            .zip(a["p90"].as_f64().zip(b["p90"].as_f64()))
            .map(|((am, bm), (ap, bp))| crate::tuning_objective::Noise {
                noise_version: 1,
                source_digest,
                build: crate::build_identity::this_build().expect("rehearsal build"),
                seed: baseline["seed"].as_u64().expect("seed"),
                compress: baseline["compress"].as_f64().expect("compress"),
                load_factor: serde_json::from_value(baseline["load_factor"].clone())
                    .expect("load factor"),
                mean_overhead: (am - bm).abs(),
                p90_overhead: (ap - bp).abs(),
                calibration: frozen_calibration,
                ideals: frozen_ideals,
                baseline_settings: original,
            })
    } else {
        None
    };
    if let (Some(file), Some(noise)) = (&mut noise_file, &noise_floor) {
        use std::io::Write;
        file.write_all(&serde_json::to_vec(noise).expect("noise serializes"))
            .and_then(|_| file.sync_all())
            .map_err(|e| Failure::io("Write A/A noise", e))?;
    }
    let cleanup_complete = restored
        && proposal
            .as_ref()
            .is_none_or(|p| p["cleanup_complete"] == true);
    let error = restoration.err().or(error).map(|e| json!(e)).or_else(|| {
        proposal
            .as_ref()
            .and_then(|p| (!p["rehearsal_error"].is_null()).then(|| p["rehearsal_error"].clone()))
    });
    Ok(
        json!({"aa":aa,"noise_floor":noise_floor,"baseline":baseline,"proposal":proposal,"comparison":report,"settings_restored":restored,"rehearsal_error":error,
        "effective_fingerprints":{"before":before_fingerprints,"after":after.ok()},
        "cleanup_complete":cleanup_complete,"lease_expires_at":needs_lease.then_some(until),"application":"temporary rehearsal only; the operator applies a winning proposal"}),
    )
}
fn comparison(
    baseline: &Value,
    proposal: Option<&Value>,
    noise: Option<&crate::tuning_objective::Noise>,
) -> Value {
    let complete = |run: &Value| {
        run["recording_complete"] == true
            && run["stop_reason"] == "completed"
            && run["cleanup_complete"] == true
            && run["rehearsal_error"].is_null()
            && run["planned"] == run["submitted"]
            && run["requests"].as_array().is_some_and(|rs| {
                Some(rs.len() as u64) == run["planned"].as_u64()
                    && rs.iter().all(|r| r["recorded"] == true)
            })
    };
    let inputs = |run: &Value| {
        run["requests"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| {
                Some((
                    r["original_run_id"].as_str()?.to_owned(),
                    json!({"ideal_ms":r["ideal_ms"],"work":r["fixed_work"]}),
                ))
            })
            .collect::<BTreeMap<_, _>>()
    };
    let complete = complete(baseline)
        && proposal.is_some_and(|p| complete(p) && inputs(p) == inputs(baseline));
    let a = &baseline["metrics"]["scheduling_objective"];
    let b = proposal
        .map(|p| &p["metrics"]["scheduling_objective"])
        .unwrap_or(&Value::Null);
    let mut result = crate::tuning_objective::compare(a, b, noise);
    if !complete {
        result["accepted"] = Value::Null;
    }
    result["status"] = json!(if complete { "compared" } else { "incomplete" });
    result["wins_within_noise"] = result["accepted"].clone();
    result["metrics"] =
        json!({"baseline":baseline["metrics"],"proposal":proposal.map(|p|&p["metrics"])});
    result["simulation"] = json!({"baseline":baseline["simulation"]["metrics"],"proposal":proposal.map(|p|&p["simulation"]["metrics"])});
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn changed_or_missing_member_fingerprints_fail_restoration() {
        let before = BTreeMap::from([("alpha".into(), "original".into())]);
        assert!(verify_restoration(&before, &before).is_ok());
        for after in [
            BTreeMap::new(),
            BTreeMap::from([("alpha".into(), "changed".into())]),
        ] {
            assert_eq!(
                verify_restoration(&before, &after).unwrap_err().code,
                "rehearsal_restoration_failed"
            );
        }
    }
    fn arm() -> Value {
        json!({"recording_complete":true,"stop_reason":"completed","cleanup_complete":true,"rehearsal_error":null,"planned":1,"submitted":1,"requests":[{"original_run_id":"r1","recorded":true,"ideal_ms":1000,"fixed_work":{"iterations":100}}],"metrics":{"scheduling_objective":{"overhead":{"mean":0.3,"p90":0.5},"hard_limits":{"reserve":true,"fleet_busy":true,"contention_slowdown":true},"failures":{"count":0}}}})
    }
    #[test]
    fn incomplete_or_changed_work_has_no_verdict() {
        let a = arm();
        let mut b = arm();
        assert_eq!(comparison(&a, Some(&b), None)["status"], "compared");
        b["requests"][0]["ideal_ms"] = json!(999);
        assert_eq!(comparison(&a, Some(&b), None)["status"], "incomplete");
        assert!(comparison(&a, Some(&b), None)["accepted"].is_null());
        assert!(comparison(&a, None, None)["accepted"].is_null());
    }
}
