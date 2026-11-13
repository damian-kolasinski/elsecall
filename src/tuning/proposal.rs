//! Reviewable projections of scheduling settings, never an installed-file read or
//! write. Unrelated fleet/consumer configuration cannot enter the artifact.
use super::space::{Settings, arguments};
use crate::{failure::Result, simulation::CandidateSettings, workload::trace::Trace};
use serde_json::{Value, json};
fn literal<T: serde::Serialize>(v: &T) -> String {
    toml::Value::try_from(v)
        .expect("setting serializes")
        .to_string()
}
pub fn render(trace: &Trace, settings: &Settings) -> Result<Value> {
    let mut effective = Settings::new();
    for (key, value) in settings {
        let single = Settings::from([(key.clone(), value.clone())]);
        if key.starts_with("label.")
            || key == "hold_limit"
            || super::space::change_count(trace, &single)? > 0
        {
            effective.insert(key.clone(), value.trim().to_owned());
        }
    }
    let settings = &effective;
    let candidate = CandidateSettings::parse(trace, &arguments(settings))?;
    let mut diff = String::from("--- recorded/fleet-config.toml\n+++ proposed/fleet-config.toml\n");
    let mut changes = Vec::new();
    let mut unknown = Vec::new();
    for member in &trace.members {
        let name = literal(&member.member);
        let Some(snapshot) = member.scheduler_settings.last() else {
            unknown.push(member.member.clone());
            continue;
        };
        let before = serde_json::to_value(&snapshot.parameters).expect("parameters serialize");
        let after = serde_json::to_value(candidate.parameters(&snapshot.parameters)?)
            .expect("parameters serialize");
        for section in ["placement", "admission", "estimates"] {
            for (key, value) in after[section].as_object().unwrap() {
                if before[section][key] != *value {
                    let old = literal(&before[section][key]);
                    let new = literal(value);
                    diff.push_str(&format!(
                        "@@ [members.{name}.{section}] @@\n-{key} = {old}\n+{key} = {new}\n"
                    ));
                    changes.push(json!({"member":member.member,"key":format!("{section}.{key}"),"before":before[section][key],"after":value,"baseline_at":snapshot.at,"reason":"Selected by the bounded simulator search; review uncertainty and held-out metrics before applying."}));
                }
            }
        }
        let capacity = candidate.capacity(&member.member, &snapshot.capacity)?;
        if capacity.reserved != snapshot.capacity.reserved {
            diff.push_str(&format!(
                "@@ [members.{name}.capacity] @@\n-reserved_cores = {}\n+reserved_cores = {}\n",
                snapshot.capacity.reserved, capacity.reserved
            ));
            changes.push(json!({"member":member.member,"key":"capacity.reserved_cores","before":snapshot.capacity.reserved,"after":capacity.reserved,"baseline_at":snapshot.at,"reason":"Search result; preserves every recorded approved reserve floor."}));
        }
    }
    let mut consumer = String::new();
    let mut labels = Vec::new();
    for (key, value) in settings {
        if let Some(pattern) = key
            .strip_prefix("label.")
            .and_then(|s| s.strip_suffix(".size"))
        {
            consumer.push_str(&format!(
                "[[labels]]\nmatch = {}\nsize = {}\n\n",
                literal(&pattern),
                literal(&value.trim())
            ));
            let before: std::collections::BTreeSet<_> = trace
                .requests
                .iter()
                .filter(|r| {
                    r.fact
                        .label
                        .as_ref()
                        .is_some_and(|l| crate::label_pattern::matches(pattern, l))
                })
                .map(|r| r.fact.size.to_string())
                .collect();
            labels.push(json!({"match":pattern,"before_recorded_sizes":before,"size":value,"reason":"Search-selected rule, including precedence guards and shadowed rules; preserve the complete ordered set. Effective demand remains independent of declared size."}));
        }
    }
    // Hold is a consumer/request option, not a service configuration key. Keep it
    // separate so it cannot silently make deferrable rules interactive.
    let hold = settings.get("hold_limit");
    if !consumer.is_empty() {
        crate::consumer_configuration::ConsumerConfiguration::parse(&consumer)
            .map_err(|e| super::invalid(e.detail))?;
    }
    let rollback = changes
        .iter()
        .map(|c| json!({"member":c["member"],"key":c["key"],"restore":c["before"]}))
        .collect::<Vec<_>>();
    Ok(
        json!({"application":"proposal_only","fleet_service_settings_diff":diff,"diff_basis":"Scheduling-only projection of the latest recorded settings per member; merge these keys into existing member overlays, preserving unrelated settings. This is not an exact patch of the current source file.","changes":changes,"unknown_service_baseline_members":unknown,
        "unresolved_service_overrides":if unknown.is_empty(){Vec::<String>::new()}else{arguments(settings).into_iter().filter(|s|!s.starts_with("label.")&&!s.starts_with("hold_limit=")).collect()},
        "consumer_elsecall_toml":consumer,"labels":labels,"interactive_hold_limit":hold,"hold_instructions":"If proposed, set hold_limit only on the existing interactive consumer rules/CLI requests; retain deferrable urgency and existing rule precedence.",
        "consumer_instructions":"Merge sizes into matching rules in each owning consumer's .elsecall.toml; trace labels do not identify a consumer file. Preserve rule order and other fields.",
        "rollback":{"service_settings":rollback,"consumer":"Restore the original consumer rules and explicit hold options; heterogeneous recorded sizes are listed per pattern."}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn trace() -> Trace {
        serde_json::from_value(json!({"trace_version":1,"since":"2026-11-03T00:00:00Z","until":"2026-11-04T00:00:00Z","incomplete":false,"master":"mini","members":[{"member":"mini","errors":[],"load":[],"scheduler_settings":[{"at":"2026-11-03T00:00:00Z","parameters":{},"capacity":{"weighted_cores":4,"reserved":1,"capacity":3}}]}],"requests":[],"leftovers":[],"demand":[],"contention":[],"limits":[]})).unwrap()
    }
    #[test]
    fn service_diff_and_rollback_golden() {
        let t = trace();
        let r = render(
            &t,
            &Settings::from([
                ("placement.keep_local_margin".into(), "0.2".into()),
                ("member.mini.reserved_cores".into(), "1.5".into()),
            ]),
        )
        .unwrap();
        assert_eq!(
            r["fleet_service_settings_diff"],
            "--- recorded/fleet-config.toml\n+++ proposed/fleet-config.toml\n@@ [members.\"mini\".placement] @@\n-keep_local_margin = 0.1\n+keep_local_margin = 0.2\n@@ [members.\"mini\".capacity] @@\n-reserved_cores = 1\n+reserved_cores = 1.5\n"
        );
        assert_eq!(
            r["rollback"]["service_settings"],
            json!([{"member":"mini","key":"placement.keep_local_margin","restore":0.1},{"member":"mini","key":"capacity.reserved_cores","restore":1.0}])
        );
        let projection = br#"[members.mini.placement]
keep_local_margin=0.2
[members.mini.capacity]
reserved_cores=1.5
"#;
        crate::service_configuration::Configuration::parse(projection).unwrap();
    }
    #[test]
    fn missing_baseline_and_noop_golden() {
        let t = trace();
        let r = render(
            &t,
            &Settings::from([("placement.keep_local_margin".into(), "0.10".into())]),
        )
        .unwrap();
        assert_eq!(r["changes"], json!([]));
        assert_eq!(r["consumer_elsecall_toml"], "");
        let mut t = t;
        t.members[0].scheduler_settings.clear();
        let r = render(
            &t,
            &Settings::from([("placement.keep_local_margin".into(), "0.2".into())]),
        )
        .unwrap();
        assert_eq!(r["unknown_service_baseline_members"], json!(["mini"]));
        assert_eq!(
            r["unresolved_service_overrides"],
            json!(["placement.keep_local_margin=0.2"])
        );
        assert_eq!(r["changes"], json!([]));
    }
    #[test]
    fn overlapping_label_guards_are_never_dropped() {
        let mut t = trace();
        t.requests.push(serde_json::from_value(json!({"run_id":"r-000000000001","kind":"run","label":"work","placement":"pinned","deferrable":false,"size":2,"calling_member":"mini","member":"mini","nested":false,"requested_at":t.since,"unfinished":false,"completed":true,"placement_request":"pinned","urgency":{"interactive":{"hold_limit_ms":10000}}})).unwrap());
        let settings = Settings::from([
            ("label.*.size".into(), "2".into()),
            ("label.work.size".into(), "1".into()),
        ]);
        let r = render(&t, &settings).unwrap();
        assert_eq!(
            r["consumer_elsecall_toml"],
            "[[labels]]\nmatch = \"*\"\nsize = \"2\"\n\n[[labels]]\nmatch = \"work\"\nsize = \"1\"\n\n"
        );
        assert_eq!(super::super::space::change_count(&t, &settings).unwrap(), 1);
    }
}
