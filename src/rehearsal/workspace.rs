//! Stable scratch grouping for historical requests without workspace identities.
use crate::{
    scheduling::record::WorkspaceIdentity,
    workload::{demand::family, trace::Request},
};
use sha2::{Digest, Sha256};

pub(super) fn prepare(requests: &mut [Request]) {
    for request in requests {
        if request.workspace.is_some() {
            continue;
        }
        // Structured encoding distinguishes ambiguous concatenations and a
        // missing label from a label that happens to spell a sentinel value.
        let identity = |kind| {
            let key = serde_json::to_vec(&(
                "elsecall-rehearsal-workspace-v1",
                kind,
                &request.fact.calling_member,
                family(&request.fact),
            ))
            .expect("workspace grouping serializes");
            format!("{:x}", Sha256::digest(key))
        };
        request.workspace = Some(WorkspaceIdentity {
            workspace: identity("workspace"),
            repository: identity("repository"),
        });
        request.synthetic_workspace = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identities_reuse_family_and_caller_but_separate_other_work() {
        let mut first =
            Request::of(&crate::reporting::fixtures::base(1, "build target", "a")).unwrap();
        first.workspace = None;
        first.workspace_recorded = false;
        first.fact.family = Some("build *".into());
        let mut same = first.clone();
        same.fact.label = Some("build another target".into());
        same.fact.run_id = "r-000000000004".into();
        let mut caller = first.clone();
        caller.fact.calling_member = "b".into();
        let mut family = first.clone();
        family.fact.family = Some("test *".into());
        let mut recorded = first.clone();
        recorded.workspace = Some(WorkspaceIdentity {
            workspace: "a".repeat(64),
            repository: "b".repeat(64),
        });
        let mut requests = vec![first, same, caller, family, recorded.clone()];
        prepare(&mut requests);
        let workspace = |index: usize| requests[index].workspace.as_ref().unwrap();
        assert_eq!(workspace(0), workspace(1));
        assert_ne!(workspace(0), workspace(2));
        assert_ne!(workspace(0), workspace(3));
        let expected = workspace(0).clone();
        assert!(requests[0].synthetic_workspace);
        assert!(!requests[0].workspace_recorded);
        assert_eq!(requests[4].workspace, recorded.workspace);
        assert!(!requests[4].synthetic_workspace);
        let before = serde_json::to_value(&requests).unwrap();
        prepare(&mut requests);
        assert_eq!(before, serde_json::to_value(&requests).unwrap());
        let mut fresh = requests[0].clone();
        fresh.workspace = None;
        prepare(std::slice::from_mut(&mut fresh));
        assert_eq!(fresh.workspace, Some(expected));
    }

    #[test]
    fn exact_labels_are_the_fallback_when_no_family_was_recorded() {
        let make = |label| Request::of(&crate::reporting::fixtures::base(1, label, "a")).unwrap();
        let mut requests = vec![
            make("build target"),
            make("build target"),
            make("build other"),
        ];
        assert!(requests.iter().all(|r| r.fact.family.is_none()));
        prepare(&mut requests);
        assert_eq!(requests[0].workspace, requests[1].workspace);
        assert_ne!(requests[0].workspace, requests[2].workspace);
    }
}
