//! Add simulator trust and high-severity drift findings to the fleet report.
use serde_json::{Value, json};
pub fn include(mut report: Value, evidence: Value) -> Value {
    if let Some(findings) = report["findings"].as_array_mut() {
        let drift=evidence["members"].as_array().into_iter().flatten().filter(|member|member["evidence"]["revoked"]==true).map(|member|json!({"kind":"simulator_drift","severity":"high","member":member["member"],"message":"Recorded-day fidelity failed; simulator trust is revoked until a new rehearsal passes.","metric":"tuning_evidence.revoked","value":1,"threshold":0}));
        findings.splice(0..0, drift);
        findings.truncate(super::findings::MAXIMUM);
    }
    report["tuning_evidence"] = evidence;
    report
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn report_shows_high_severity_revocation_and_keeps_not_checkable_explicit() {
        let evidence = json!({"members":[{"member":"alpha","evidence":{"revoked":true,"days":[{"status":"not_checkable"}]}}]});
        let report = include(json!({"findings":[]}), evidence.clone());
        assert_eq!(
            report,
            json!({"findings":[{"kind":"simulator_drift","severity":"high","member":"alpha","message":"Recorded-day fidelity failed; simulator trust is revoked until a new rehearsal passes.","metric":"tuning_evidence.revoked","value":1,"threshold":0}],"tuning_evidence":evidence})
        );
        let clear = include(
            json!({"findings":[]}),
            json!({"members":[{"member":"alpha","evidence":{"revoked":false}}]}),
        );
        assert_eq!(clear["findings"], json!([]));
    }
}
