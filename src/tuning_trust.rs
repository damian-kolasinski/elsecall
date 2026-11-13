//! Durable simulator certificates and drift revocation. Passing days never clear
//! a revocation: only a subsequent successful rehearsal can do that.
use crate::{
    build_identity::{Build, Digest},
    failure::{Failure, Result},
    paths::Paths,
    scheduling::timestamp::Timestamp,
    workload::trace::Trace,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

pub const MAX_AGE: Duration = Duration::from_secs(7 * 86400);
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Certificate {
    pub rehearsal: String,
    pub trace_digest: Digest,
    pub source_digest: Digest,
    pub build: Build,
    pub issued_at: Timestamp,
    pub rehearsal_started_at: Timestamp,
    pub rehearsal_finished_at: Timestamp,
    pub expires_at: Timestamp,
    pub settings: Value,
    pub fidelity: Value,
    pub fidelity_digest: Digest,
}
impl Certificate {
    fn current(&self, build: &Build, now: Timestamp) -> bool {
        self.build == *build
            && self.issued_at <= now
            && self.expires_at > now
            && self.expires_at.since(self.issued_at) <= MAX_AGE
    }
    pub fn from_rehearsal(trace: &Trace, simulation: &Value) -> Option<Self> {
        let recording = trace.rehearsal.as_ref()?;
        if !recording.complete_for(trace) || simulation["fidelity"]["trusted_for_tuning"] != true {
            return None;
        }
        let issued_at = Timestamp::now();
        Some(Self {
            rehearsal: recording.session.clone(),
            trace_digest: Digest::of(&serde_json::to_vec(trace).expect("trace serializes")),
            source_digest: recording.source_digest,
            build: recording.build.clone(),
            issued_at,
            rehearsal_started_at: trace.since,
            rehearsal_finished_at: trace.until,
            expires_at: issued_at.after(MAX_AGE),
            settings: json!(
                trace
                    .members
                    .iter()
                    .map(|m| json!({"member":m.member,"latest":m.scheduler_settings.last(),"snapshot_count":m.scheduler_settings.len(),"history_digest":Digest::of(&serde_json::to_vec(&m.scheduler_settings).expect("settings serialize"))}))
                    .collect::<Vec<_>>()
            ),
            fidelity: summary(&simulation["fidelity"]),
            fidelity_digest: Digest::of(&serde_json::to_vec(&simulation["fidelity"]).expect("fidelity serializes")),
        })
    }
}
fn database(paths: &Paths) -> Result<rusqlite::Connection> {
    crate::private_files::directory(&paths.data)?;
    let path = paths.data.join("tuning-evidence.sqlite");
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|e| Failure::io("Open tuning evidence", e))?;
    let db = rusqlite::Connection::open(path).map_err(error)?;
    db.busy_timeout(Duration::from_secs(5)).map_err(error)?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS certificate (id INTEGER PRIMARY KEY CHECK(id=1), value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS drift (day INTEGER PRIMARY KEY, checked_at INTEGER NOT NULL, value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS revocation (id INTEGER PRIMARY KEY CHECK(id=1), at INTEGER NOT NULL);").map_err(error)?;
    Ok(db)
}
fn error(e: rusqlite::Error) -> Failure {
    Failure::new(
        "tuning_evidence_unavailable",
        format!("Cannot access tuning evidence: {e}"),
        "Inspect this member's tuning evidence database.",
    )
}
pub fn certify(paths: &Paths, certificate: &Certificate) -> Result<()> {
    if certificate.build != crate::build_identity::this_build()?
        || certificate.fidelity["trusted_for_tuning"] != true
        || !crate::rehearsal::protocol::valid_session(&certificate.rehearsal)
        || certificate.expires_at.since(certificate.issued_at) > MAX_AGE
    {
        return Err(crate::rehearsal::invalid(
            "Invalid or different-build rehearsal certificate.",
        ));
    }
    let mut db = database(paths)?;
    let tx = db.transaction().map_err(error)?;
    tx.execute(
        "INSERT OR REPLACE INTO certificate VALUES (1,?1)",
        [serde_json::to_string(certificate).expect("certificate serializes")],
    )
    .map_err(error)?;
    // Only a rehearsal begun after a detected failure can clear its revocation.
    tx.execute(
        "DELETE FROM revocation WHERE at <= ?1",
        [certificate.rehearsal_started_at.millis() as i64],
    )
    .map_err(error)?;
    tx.commit().map_err(error)
}
pub fn monitored(paths: &Paths, day: Timestamp) -> Result<bool> {
    let db = database(paths)?;
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM drift WHERE day=?1 AND (json_extract(value, '$.status') != 'not_checkable' OR checked_at>=?2 OR day<?3))",
        rusqlite::params![day.millis() as i64, Timestamp::now().millis().saturating_sub(86_400_000) as i64, (Timestamp::now().millis()/86_400_000*86_400_000).saturating_sub(2*86_400_000) as i64],
        |r| r.get(0),
    )
    .map_err(error)
}
pub fn record_day(paths: &Paths, day: Timestamp, fidelity: &Value) -> Result<()> {
    let now = Timestamp::now();
    let result = json!({"day":day,"checked_at":now,"build":crate::build_identity::this_build()?,
        "status":match fidelity["status"].as_str(){Some("pass")=>"pass",Some("fail")=>"fail",_=>"not_checkable"},"fidelity":fidelity});
    let mut db = database(paths)?;
    let tx = db.transaction().map_err(error)?;
    tx.execute(
        "INSERT OR REPLACE INTO drift VALUES (?1,?2,?3)",
        rusqlite::params![day.millis() as i64, now.millis() as i64, result.to_string()],
    )
    .map_err(error)?;
    if result["status"] == "fail" {
        tx.execute(
            "INSERT OR REPLACE INTO revocation VALUES (1,?1)",
            [now.millis() as i64],
        )
        .map_err(error)?;
    }
    tx.commit().map_err(error)
}
pub fn status(paths: &Paths) -> Result<Value> {
    use rusqlite::OptionalExtension;
    let db = database(paths)?;
    let certificate: Option<String> = db
        .query_row("SELECT value FROM certificate WHERE id=1", [], |r| r.get(0))
        .optional()
        .map_err(error)?;
    let certificate: Option<Certificate> = certificate
        .map(|s| {
            serde_json::from_str(&s)
                .map_err(|_| crate::rehearsal::invalid("Invalid stored certificate."))
        })
        .transpose()?;
    let revoked: bool = db
        .query_row("SELECT EXISTS(SELECT 1 FROM revocation)", [], |r| r.get(0))
        .map_err(error)?;
    let build = crate::build_identity::this_build()?;
    let current = certificate
        .as_ref()
        .is_some_and(|c| c.current(&build, Timestamp::now()));
    let mut statement = db
        .prepare("SELECT value FROM drift ORDER BY day DESC LIMIT 30")
        .map_err(error)?;
    let mut days = statement
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(error)?
        .map(|r| {
            r.map_err(error).and_then(|s| {
                serde_json::from_str::<Value>(&s)
                    .map_err(|_| crate::rehearsal::invalid("Invalid stored drift result."))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    for day in &mut days {
        day["fidelity"] = summary(&day["fidelity"]);
    }
    let certificate_summary = certificate
        .as_ref()
        .map(|certificate| serde_json::to_value(certificate).expect("certificate serializes"));
    Ok(
        json!({"trusted_for_tuning":current && !revoked,"revoked":revoked,"certificate":certificate_summary,"days":days,
        "status":if revoked{"revoked"}else if current{"current"}else if certificate.is_some(){"expired"}else{"uncertified"}}),
    )
}

/// Fleet diagnostics are bounded summaries. Full comparison evidence, including
/// timelines/outliers, remains in the member's private evidence database.
fn summary(fidelity: &Value) -> Value {
    // Stored drift rows retain the rule that actually evaluated them. Legacy
    // evidence predates the explicit count of minutes within tolerance.
    let sampling_tolerance = fidelity["queues"].as_array().is_some_and(|queues| {
        queues
            .iter()
            .any(|q| q.get("within_tolerance_count").is_some())
    });
    let mut tolerances = json!({"placement_fraction":0.99,"pinned_required_exact":true,"wait_median_ms":1000,"wait_p95_limit_ms":fidelity["waits"]["p95_limit_ms"],"queue_maximum_error":1,"charged_relative":0.05,"run_duration_relative":0.1});
    if sampling_tolerance {
        tolerances["queue_matching_fraction"] = json!(1.0);
        tolerances["queue_per_minute_tolerance"] = json!(1);
        tolerances["queue_peak_difference"] = json!(1);
    } else {
        tolerances["queue_exact_fraction"] = json!(0.95);
    }
    json!({"status":fidelity["status"],"trusted_for_tuning":fidelity["trusted_for_tuning"],
        "blocker_count":fidelity["blockers"].as_array().map(Vec::len),
        "blockers":fidelity["blockers"].as_array().map(|b|b.iter().take(10).collect::<Vec<_>>()),
        "placements":{"status":fidelity["placements"]["status"],"exact_count":fidelity["placements"]["exact_count"],"of":fidelity["placements"]["of"]},
        "waits":{"status":fidelity["waits"]["status"],"median_absolute_error_ms":fidelity["waits"]["median_absolute_error_ms"],"p95_absolute_error_ms":fidelity["waits"]["p95_absolute_error_ms"]},
        "terminal_verdicts":fidelity["terminal_verdicts"]["status"],"charged_mean":{"status":fidelity["charged_mean"]["status"],"count":fidelity["charged_mean"]["count"],"simulated_count":fidelity["charged_mean"]["simulated_count"],"recorded_ms":fidelity["charged_mean"]["recorded_ms"],"simulated_ms":fidelity["charged_mean"]["simulated_ms"],"unscorable_count":fidelity["charged_mean"]["unscorable_run_ids"].as_array().map(Vec::len)},"run_durations":fidelity["run_durations"]["status"],
        "full_evidence":"retained in the rehearsal trace artifact or tuning-evidence.sqlite",
        "tolerances":tolerances})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn summaries_preserve_the_evaluated_queue_policy_for_legacy_drift() {
        let legacy: Value =
            serde_json::from_str(include_str!("../tests/fixtures/run5-rehearsal.json")).unwrap();
        let mut fidelity = legacy["fidelity"].clone();
        let old = summary(&fidelity);
        assert_eq!(old["tolerances"]["queue_exact_fraction"], 0.95);
        assert!(old["tolerances"]["queue_matching_fraction"].is_null());
        fidelity["queues"][0]["within_tolerance_count"] = json!(6);
        let current = summary(&fidelity);
        assert_eq!(current["tolerances"]["queue_matching_fraction"], 1.0);
        assert_eq!(current["tolerances"]["queue_per_minute_tolerance"], 1);
        assert_eq!(current["tolerances"]["queue_peak_difference"], 1);
        assert!(current["tolerances"]["queue_exact_fraction"].is_null());
    }
    #[test]
    fn certificates_expire_at_the_age_boundary_and_on_build_change() {
        let build = crate::build_identity::this_build().unwrap();
        let issued_at = Timestamp::from_millis(1000);
        let certificate = Certificate {
            rehearsal: "a".repeat(24),
            trace_digest: Digest::of(b"trace"),
            source_digest: Digest::of(b"source"),
            build: build.clone(),
            issued_at,
            rehearsal_started_at: issued_at,
            rehearsal_finished_at: issued_at,
            expires_at: issued_at.after(MAX_AGE),
            settings: Value::Null,
            fidelity: json!({"trusted_for_tuning":true}),
            fidelity_digest: Digest::of(b"fidelity"),
        };
        assert!(certificate.current(&build, issued_at));
        assert!(!certificate.current(&build, certificate.expires_at));
        let other = Build {
            version: build.version,
            build: "0000000000000000".parse().unwrap(),
        };
        assert!(!certificate.current(&other, issued_at));
    }
    #[test]
    fn large_fidelity_reports_produce_bounded_certificate_writes() {
        let start = Timestamp::from_millis(crate::reporting::fixtures::T0);
        let record = crate::reporting::fixtures::pinned(0, "work", "a", 1.0);
        let mut trace:Trace=serde_json::from_value(json!({"trace_version":1,"incomplete":false,"since":start,"until":start.after(Duration::from_secs(60)),"master":"a","members":[{"member":"a","errors":[],"load":[crate::reporting::fixtures::bucket(0,0.1,0.0,0,0,1.0)],"scheduler_settings":[{"at":start,"capacity":{"weighted_cores":10,"reserved":0,"capacity":10},"parameters":{}}]}],"requests":[crate::workload::trace::Request::of(&record).unwrap()],"leftovers":[],"demand":[],"contention":[],"limits":[]})).unwrap();
        let session = "a".repeat(24);
        let id = trace.requests[0].fact.run_id.clone();
        trace.requests[0].fact.synthetic = true;
        trace.requests[0].fact.label = Some(format!("rehearsal:{session}:work"));
        trace.rehearsal = Some(crate::rehearsal::recording::Recording {
            calibration: std::collections::BTreeMap::from([(
                "a".into(),
                crate::rehearsal::worker::Calibration {
                    iterations: 1000,
                    elapsed_seconds: 1.0,
                    cpu_seconds: 1.0,
                    iterations_per_second: 1000.0,
                    logical_cores: 1,
                },
            )]),
            ideal_ms: std::collections::BTreeMap::from([(id.clone(), Some(1000.0))]),
            uncontended_ms: std::collections::BTreeMap::from([(
                id.clone(),
                std::collections::BTreeMap::from([("a".into(), 1000.0)]),
            )]),
            work_iterations: std::collections::BTreeMap::from([(id.clone(), 1000)]),
            session,
            source_digest: Digest::of(b"source"),
            build: crate::build_identity::this_build().unwrap(),
            load_factor: Default::default(),
            compress: 6.0,
            seed: 0,
            complete: true,
            planned: 1,
            requests: Value::Null,
            aggregated_nested: vec![],
            load_complete: trace
                .members
                .iter()
                .map(|m| (m.member.clone(), true))
                .collect(),
            demand: std::collections::BTreeMap::from([(
                id.clone(),
                std::collections::BTreeMap::from([("a".into(), 0.5)]),
            )]),
            duration_ms: std::collections::BTreeMap::from([(id, 1000)]),
            charge_floor_ms: Default::default(),
        });
        let fidelity = json!({"trusted_for_tuning":true,"status":"pass","run_durations":{"groups":vec![json!({"family":"x".repeat(200),"status":"pass"});10_000]},"sync":{"requests":vec![json!({"run_id":"r-000000000001","recorded_ms":10});10_000]}});
        assert!(serde_json::to_vec(&fidelity).unwrap().len() > 2 * 1024 * 1024);
        let certificate =
            Certificate::from_rehearsal(&trace, &json!({"fidelity":fidelity})).unwrap();
        assert!(serde_json::to_vec(&certificate).unwrap().len() < 128 * 1024);
    }
    #[test]
    fn drift_summary_is_bounded_even_with_all_missing_references() {
        let fidelity = json!({"status":"blocked_by_missing_evidence","blockers":vec!["missing";50_000],"charged_mean":{"status":"fail","unscorable_run_ids":vec!["r-000000000001";50_000]}});
        let summary = summary(&fidelity);
        assert_eq!(summary["charged_mean"]["unscorable_count"], 50_000);
        assert_eq!(summary["blocker_count"], 50_000);
        assert!(serde_json::to_vec(&summary).unwrap().len() < 2048);
    }
    #[test]
    fn not_checkable_retries_are_bounded_and_passing_days_do_not_clear_revocation() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let paths = Paths::for_home(temp.path()).unwrap();
        let day = Timestamp::from_millis(
            Timestamp::now().millis() / 86_400_000 * 86_400_000 - 86_400_000,
        );
        record_day(
            &paths,
            day,
            &json!({"status":"blocked_by_missing_evidence"}),
        )
        .unwrap();
        assert!(monitored(&paths, day).unwrap());
        record_day(&paths, day, &json!({"status":"fail"})).unwrap();
        record_day(
            &paths,
            day.after(Duration::from_secs(86400)),
            &json!({"status":"pass"}),
        )
        .unwrap();
        assert_eq!(status(&paths).unwrap()["revoked"], true);
    }
}
