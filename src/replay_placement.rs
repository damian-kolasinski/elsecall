//! Read-only placement counterfactuals. Keep measured work, label families and
//! urgency fixed; only the placement component of current consumer rules applies.
use crate::{
    consumer_configuration::{ConsumerConfiguration, RulePlacement},
    failure::{Failure, Result},
    scheduling::request::Placement,
    workload::trace::Trace,
};
use clap::{Args, ValueEnum};
use serde::Serialize;
use std::{io::Read, path::PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, serde::Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Historical,
    Current,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub mode: Mode,
    pub rules_digest: crate::build_identity::Digest,
}
#[derive(Args, Clone, Debug, Default)]
pub struct ReplayPlacement {
    /// Historical recorded placement, or current consumer label placement rules
    #[arg(long, value_enum, default_value = "historical")]
    pub placement: Mode,
    /// Consumer .elsecall.toml to read (required with --placement current)
    #[arg(long, value_name = "PATH")]
    pub rules: Option<PathBuf>,
}
impl ReplayPlacement {
    pub fn apply(&self, trace: &mut Trace) -> Result<serde_json::Value> {
        let invalid = || {
            Failure::new(
                "invalid_request",
                "Use --placement current with --rules PATH, or historical without --rules.",
                "Rules are read from the supplied file and never modified.",
            )
        };
        let digest = match (self.placement, &self.rules) {
            (Mode::Historical, None) => None,
            (Mode::Current, Some(path)) => {
                let mut bytes = Vec::new();
                std::fs::File::open(path)
                    .and_then(|f| f.take(65537).read_to_end(&mut bytes))
                    .map_err(|e| Failure::io("Read replay rules", e))?;
                if bytes.len() > 65536 {
                    return Err(invalid());
                }
                // Do not echo parser messages or file contents into artifacts.
                let configuration = std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(|s| ConsumerConfiguration::parse(s).ok())
                    .ok_or_else(|| {
                        Failure::new(
                            "invalid_request",
                            "Replay rules are not a valid consumer configuration.",
                            "Check the supplied .elsecall.toml.",
                        )
                    })?;
                for r in &mut trace.requests {
                    if r.fact.internal
                        || crate::rehearsal::internal(r)
                        || r.fact.synthetic
                        || crate::rehearsal::synthetic(r.fact.label.as_deref())
                    {
                        continue;
                    }
                    let rule = r
                        .fact
                        .label
                        .as_deref()
                        .and_then(|l| configuration.rule_for(l));
                    // Consumer default placement is pinned. Deferrable work is
                    // pinned by the admission contract even in a counterfactual.
                    r.placement_request = if r.fact.kind == crate::scheduling::request::Kind::Run
                        && !r.fact.deferrable
                        && rule.and_then(|r| r.placement) == Some(RulePlacement::Placeable)
                    {
                        Placement::Placeable
                    } else {
                        Placement::Pinned
                    };
                    r.fact.placement = match r.placement_request {
                        Placement::Placeable => crate::reporting::facts::PlacementKind::Placeable,
                        _ => crate::reporting::facts::PlacementKind::Pinned,
                    };
                }
                let digest = crate::build_identity::Digest::of(&bytes);
                trace.placement_replay = Some(Provenance {
                    mode: Mode::Current,
                    rules_digest: digest,
                });
                trace.limits.push(format!("Replay placement uses current consumer rules (digest {digest:?}); historical work, urgency and family remain fixed. Counterfactual placement cannot certify historical fidelity."));
                Some(digest)
            }
            _ => return Err(invalid()),
        };
        Ok(serde_json::json!({"mode":self.placement,"rules_digest":digest,"split":split(trace)}))
    }
}
pub fn split(trace: &Trace) -> serde_json::Value {
    let roots: Vec<_> = trace
        .requests
        .iter()
        .filter(|r| {
            !r.fact.nested
                && r.fact.requested_at >= trace.since
                && r.fact.requested_at < trace.until
                && !r.fact.internal
                && !crate::rehearsal::internal(r)
        })
        .collect();
    serde_json::json!({"pinned":roots.iter().filter(|r| r.placement_request == Placement::Pinned).count(),"placeable":roots.iter().filter(|r| r.placement_request == Placement::Placeable).count(),"required":roots.iter().filter(|r| matches!(r.placement_request, Placement::Member(_))).count()})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{reporting::fixtures, scheduling::timestamp::Timestamp, workload::trace::Request};
    fn trace() -> Trace {
        let mut r = Request::of(&fixtures::pinned(1, "acme test Checkout", "master", 1.0)).unwrap();
        r.fact.family = Some("acme test *".into());
        r.workspace = Some(crate::scheduling::record::WorkspaceIdentity {
            workspace: "a".repeat(64),
            repository: "b".repeat(64),
        });
        r.execution_reference = Some(crate::workload::trace::ExecutionReference::Known {
            milliseconds: 1000.0,
            identified: true,
            member_speed: Some(1.0),
        });
        Trace {
            placement_replay: None,
            trace_version: 1,
            incomplete: false,
            since: r.fact.requested_at,
            until: r
                .fact
                .requested_at
                .after(std::time::Duration::from_secs(10)),
            master: Some("master".into()),
            members: ["master", "devbox"]
                .into_iter()
                .map(|m| crate::workload::trace::Member {
                    member: m.into(),
                    load: vec![],
                    errors: vec![],
                    scheduler_settings: vec![],
                })
                .collect(),
            requests: vec![r],
            leftovers: vec![],
            demand: vec![],
            contention: vec![],
            limits: vec![],
            rehearsal: None,
        }
    }
    #[test]
    fn acme_current_rules_make_historical_pinned_tests_eligible_on_devbox_without_writing_rules() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rules.toml");
        let bytes = include_bytes!("../tests/fixtures/acme.elsecall.toml");
        std::fs::write(&path, bytes).unwrap();
        let mut t = trace();
        let before = serde_json::to_vec(&t).unwrap();
        ReplayPlacement::default().apply(&mut t).unwrap();
        assert_eq!(before, serde_json::to_vec(&t).unwrap());
        assert_eq!(
            crate::simulation::structural_members(&t, &t.requests[0]),
            vec!["master"]
        );
        let original = t.requests[0].clone();
        let report = ReplayPlacement {
            placement: Mode::Current,
            rules: Some(path.clone()),
        }
        .apply(&mut t)
        .unwrap();
        assert_eq!(report["split"]["placeable"], 1);
        assert_eq!(
            crate::simulation::structural_members(&t, &t.requests[0]),
            vec!["master", "devbox"]
        );
        assert_eq!(t.requests[0].fact.family, original.fact.family);
        assert_eq!(t.requests[0].fact.size, original.fact.size);
        assert_eq!(t.requests[0].urgency, original.urgency);
        assert_eq!(
            t.requests[0].execution_reference,
            original.execution_reference
        );
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    #[test]
    fn rejects_missing_incompatible_oversized_and_invalid_rules_without_echoing_contents() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rules.toml");
        for options in [
            ReplayPlacement {
                placement: Mode::Current,
                rules: None,
            },
            ReplayPlacement {
                placement: Mode::Historical,
                rules: Some(path.clone()),
            },
        ] {
            assert!(options.apply(&mut trace()).is_err());
        }
        for contents in ["SECRET_SENTINEL".into(), "x".repeat(65537)] {
            std::fs::write(&path, contents).unwrap();
            let e = ReplayPlacement {
                placement: Mode::Current,
                rules: Some(path.clone()),
            }
            .apply(&mut trace())
            .unwrap_err();
            assert!(!e.message.contains("SECRET_SENTINEL"));
        }
    }
    #[test]
    fn unmatched_and_deferrable_requests_use_consumer_pinned_default_and_internal_work_is_preserved()
     {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rules.toml");
        std::fs::write(
            &path,
            "[[labels]]\nmatch = \"acme test *\"\nplacement = \"placeable\"\n",
        )
        .unwrap();
        let mut t = trace();
        t.requests[0].fact.deferrable = true;
        ReplayPlacement {
            placement: Mode::Current,
            rules: Some(path.clone()),
        }
        .apply(&mut t)
        .unwrap();
        assert_eq!(t.requests[0].placement_request, Placement::Pinned);
        t.requests[0].fact.deferrable = false;
        t.requests[0].fact.label = Some("unmatched".into());
        t.requests[0].placement_request = Placement::Member("devbox".into());
        ReplayPlacement {
            placement: Mode::Current,
            rules: Some(path),
        }
        .apply(&mut t)
        .unwrap();
        assert_eq!(t.requests[0].placement_request, Placement::Pinned);
        assert!(t.since < Timestamp::now());
    }
}
