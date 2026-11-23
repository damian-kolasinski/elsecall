//! What in a member's latest apply of its tool plan needs attention: one
//! `provisioning_step_failed` item per failed step, and one
//! `xcode_apple_id_required` item per Xcode version no member could copy
//! because the master lacks it. Derived from each member's reported apply.
use crate::{
    fleet_attention::elsecall,
    provisioning_model::{FailedStep, ProvisioningSummary},
};
use elsecall_contract::attention::{Action, Category, Item};
use elsecall_peers::roster::MemberName;

/// A step that copies an Xcode the master does not have.
const NOT_ON_MASTER: &str = "xcode_not_on_master";

/// One `xcode_apple_id_required` item per Xcode version some member's latest
/// apply could not copy from the master, about the master, where a person gets
/// it with an Apple ID: only a person can sign in.
pub(crate) fn xcode_downloads<'a>(
    applies: impl Iterator<Item = &'a ProvisioningSummary>,
    master: &MemberName,
) -> Vec<Item> {
    let mut versions: Vec<(
        &str,
        elsecall_contract::scheduling_model::timestamp::Timestamp,
    )> = Vec::new();
    for failed in applies.flat_map(|summary| &summary.failed) {
        if failed.code != NOT_ON_MASTER || failed.manager != "xcodes" {
            continue;
        }
        match versions
            .iter_mut()
            .find(|(version, _)| *version == failed.version)
        {
            Some((_, since)) => *since = (*since).min(failed.failed_at),
            None => versions.push((&failed.version, failed.failed_at)),
        }
    }
    versions
        .into_iter()
        .map(|(version, since)| {
            let category = Category::XcodeAppleIdRequired;
            Item {
                id: Item::id(category, master.as_str(), Some(version)),
                member: master.to_string(),
                category,
                subject: Some(version.to_owned()),
                code: NOT_ON_MASTER.into(),
                message: format!(
                    "Xcode {version} is pinned, and the master, {master}, has none to copy: members get Xcode only from the master."
                ),
                hint: format!(
                    "A person downloads Xcode {version} onto {master} with an Apple ID (xcodes install {version}, or developer.apple.com/download), or copies it there from a Mac that has it; then run elsecall tools apply on {master}."
                ),
                since: Some(since),
                actions: vec![Action {
                    summary: format!(
                        "Once Xcode {version} is installed on {master}, apply the plan again to copy it to the members that need it."
                    ),
                    command: elsecall(&["tools", "apply"]),
                    run_on: master.to_string(),
                    person_only: true,
                }],
            }
        })
        .collect()
}

/// One `provisioning_step_failed` item per failed step of `member`'s latest
/// apply. The text is written here from the code: another member's report holds
/// no text this member shows.
pub(crate) fn failed_steps(
    member: &MemberName,
    summary: &ProvisioningSummary,
    master: &MemberName,
) -> Vec<Item> {
    summary
        .failed
        .iter()
        // A person gets the master an Xcode it lacks; one item asks for it.
        .filter(|failed| failed.code != NOT_ON_MASTER)
        .map(|failed| failed_step(member, failed, master))
        .collect()
}

fn failed_step(member: &MemberName, failed: &FailedStep, master: &MemberName) -> Item {
    let category = Category::ProvisioningStepFailed;
    let subject = failed.subject();
    let what = format!("{} {} {}", failed.manager, failed.tool, failed.version);
    let (why, hint) = step_failure(member, failed);
    // Only a person grants root steps: it asks for an administrator's password.
    let privileges = (failed.code == "xcode_privileges_missing").then(|| Action {
        summary: format!(
            "On {member}, let Elsecall place, select and license Xcode: installs its helper and a sudoers rule for it alone, with an administrator's password."
        ),
        command: elsecall(&["tools", "privileges", "install"]),
        run_on: member.to_string(),
        person_only: true,
    });
    Item {
        id: Item::id(category, member.as_str(), Some(&subject)),
        member: member.to_string(),
        category,
        subject: Some(subject),
        code: failed.code.clone(),
        message: format!(
            "Installing {what} on {member} failed at its {} stage: {why}.",
            failed.stage
        ),
        hint,
        since: Some(failed.failed_at),
        actions: privileges
            .into_iter()
            .chain([Action {
                summary: format!("Apply {member}'s plan again, which retries this step."),
                command: elsecall(&["tools", "apply", "--member", member.as_str()]),
                run_on: master.to_string(),
                person_only: false,
            }])
            .collect(),
    }
}

/// Why a step failed, and what resolves it, by its code.
fn step_failure(member: &MemberName, failed: &FailedStep) -> (&'static str, String) {
    let tool = format!("{} {}", failed.tool, failed.version);
    let retry = format!("then run elsecall tools apply --member {member} on the master to retry");
    match failed.code.as_str() {
        "mise_attestation_missing" => (
            "the prebuilt artifact has no GitHub attestation",
            format!(
                "On {member}, install {tool} from a verified source build if its mise backend supports it, {retry}."
            ),
        ),
        "mise_ruby_psych_failed" => (
            "the Ruby build could not configure its Psych YAML extension",
            format!("Check for libyaml and its headers on {member}, {retry}."),
        ),
        "tool_install_failed" => (
            "mise install exited with an error",
            format!(
                "Install {tool} with mise on {member}, with the options elsecall tools plan lists, to see why; fix the cause, {retry}."
            ),
        ),
        "tool_install_timeout" => (
            "mise install ran for 30 minutes",
            format!("Install {tool} with mise on {member} to see where it stalls, {retry}."),
        ),
        "mise_missing" => (
            "mise is not installed there",
            format!("Install mise on {member}, {retry}."),
        ),
        "xcode_missing" => (
            "no Xcode installed there knows the runtime",
            format!(
                "Install Xcode {} or later on {member}; a plan that pins it in .xcode-version installs it before its runtime, {retry}.",
                failed.version
            ),
        ),
        "xcode_unreadable" => (
            "its Xcodes could not be read",
            format!("Run elsecall tools inventory on {member} to see why, {retry}."),
        ),
        "runtime_download_failed" => (
            "xcodebuild -downloadPlatform exited with an error",
            format!(
                "Run xcodebuild -downloadPlatform {} -buildVersion {} -architectureVariant arm64 on {member} to see why, {retry}.",
                failed.tool, failed.version
            ),
        ),
        "runtime_download_timeout" => (
            "the runtime's download ran for an hour",
            format!("Check {member}'s network, {retry}."),
        ),
        "xcode_privileges_missing" => (
            "Elsecall's Xcode helper or its sudoers rule is not installed there",
            format!(
                "A person runs elsecall tools privileges install on {member} with an administrator's password, {retry}."
            ),
        ),
        "xcode_copy_failed" => (
            "the copy of the master's Xcode could not be unpacked",
            format!("Check that {member} has room for Xcode in its home folder, {retry}."),
        ),
        "xcode_not_verified" => (
            "the copied Xcode did not verify as Apple's",
            format!(
                "Check the master's Xcode {} with codesign --verify --deep --strict, {retry}.",
                failed.version
            ),
        ),
        "xcode_place_failed" => (
            "Elsecall's helper could not place the copied Xcode",
            format!(
                "Run elsecall tools privileges on {member} to check the helper; an Xcode {} already in /Library/Elsecall/Xcodes is never placed twice; {retry}.",
                failed.version
            ),
        ),
        "xcode_unsupported" => (
            "Elsecall places Xcode 16.0 or later, numbered with two or three numbers",
            format!(
                "Pin a version of Xcode Elsecall places, or leave Xcode {} out on {member} with elsecall tools leave-out, {retry}.",
                failed.version
            ),
        ),
        "xcode_license_failed" => (
            "its license or first-launch tasks failed",
            format!(
                "Run sudo xcodebuild -runFirstLaunch with that Xcode on {member} to see why, {retry}."
            ),
        ),
        "xcode_select_failed" => (
            "xcode-select did not select it",
            format!("Run elsecall tools privileges on {member}, {retry}."),
        ),
        "artifact_on_master" => (
            "it is the master, which has no other member to copy mise from",
            format!("Install mise on {member} (curl https://mise.run | sh), {retry}."),
        ),
        "artifact_unavailable" => (
            "the master has no mise to copy",
            format!("Install mise on the master, {retry}."),
        ),
        "artifact_unreachable" => (
            "the master could not be reached to copy from, or this member has no fleet",
            format!("Check the master and {member} with elsecall fleet, {retry}."),
        ),
        "artifact_busy" => (
            "the master was already sending as many copies as it sends at once",
            format!("Wait for the other members' copies, {retry}."),
        ),
        "artifact_unsupported" => (
            "the master runs another build, which cannot send it",
            format!("Run elsecall fleet upgrade on the master, {retry}."),
        ),
        "artifact_cut" => (
            "the copy from the master did not arrive whole",
            format!("Nothing partial was kept; {retry}."),
        ),
        "mise_copy_failed" => (
            "the copy of the master's mise could not be put in place",
            format!(
                "Check that {member} can write the folder mise goes in (~/.local/bin, or ELSECALL_MISE's), {retry}."
            ),
        ),
        "mise_copy_unusable" => (
            "the master's mise does not run there",
            format!("Install mise on {member} (curl https://mise.run | sh), {retry}."),
        ),
        "mise_unusable" => (
            "mise could not be started there",
            format!("Run mise --version on {member} and reinstall mise if it fails, {retry}."),
        ),
        "install_not_admitted" => (
            "its scheduler did not admit the step",
            format!("Run elsecall doctor on {member}, {retry}."),
        ),
        "apply_interrupted" => (
            "the service stopped while the step waited or ran",
            format!("Once {member}'s service runs again, {retry}."),
        ),
        "apply_failed" => (
            "the apply stopped unexpectedly",
            format!("Read elsecall doctor on {member}, {retry}."),
        ),
        _ => (
            "a later build's failure this build does not describe",
            format!(
                "Run elsecall fleet upgrade on the master so every member runs its build, {retry}."
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use crate::provisioning_attention::*;
    use serde_json::json;

    fn summary(failed: serde_json::Value) -> ProvisioningSummary {
        serde_json::from_value(json!({
            "state": "finished", "started_at": "2026-11-20T09:00:00.000Z",
            "finished_at": "2026-11-20T10:00:00.000Z", "steps": 2, "installed": 0,
            "failed": failed,
        }))
        .unwrap()
    }

    #[test]
    fn each_failed_install_step_is_an_item_naming_its_tool_and_stage() {
        let summary: ProvisioningSummary = serde_json::from_value(json!({
            "state": "finished", "started_at": "2026-11-20T09:00:00.000Z", "finished_at": "2026-11-20T10:00:00.000Z", "steps": 2, "installed": 0,
            "failed": [
                {"manager": "mise", "tool": "node", "version": "22.19.0", "stage": "install",
                 "code": "tool_install_failed", "failed_at": "2026-11-20T10:00:00.000Z"},
                {"manager": "mise", "tool": "bun", "version": "1.3.14", "stage": "apply",
                 "code": "a_later_code", "failed_at": "2026-11-20T10:00:00.000Z"},
            ],
        }))
        .unwrap();
        let items = failed_steps(
            &"macbook-pro".parse().unwrap(),
            &summary,
            &"mac-mini".parse().unwrap(),
        );
        assert_eq!(
            serde_json::to_value(&items[0]).unwrap(),
            json!({
                "id": "provisioning_step_failed:macbook-pro:mise:node@22.19.0",
                "member": "macbook-pro",
                "category": "provisioning_step_failed",
                "subject": "mise:node@22.19.0",
                "code": "tool_install_failed",
                "message": "Installing mise node 22.19.0 on macbook-pro failed at its install stage: mise install exited with an error.",
                "hint": "Install node 22.19.0 with mise on macbook-pro, with the options elsecall tools plan lists, to see why; fix the cause, then run elsecall tools apply --member macbook-pro on the master to retry.",
                "since": "2026-11-20T10:00:00.000Z",
                "actions": [{
                    "summary": "Apply macbook-pro's plan again, which retries this step.",
                    "command": ["elsecall", "tools", "apply", "--member", "macbook-pro"],
                    "run_on": "mac-mini",
                    "person_only": false
                }]
            })
        );
        assert_eq!(items[1].code, "a_later_code");
        assert!(items[1].message.contains("this build does not describe"));
    }

    #[test]
    fn a_missing_privilege_is_granted_by_a_person_on_the_member() {
        let items = failed_steps(
            &"macbook-pro".parse().unwrap(),
            &summary(
                json!([{"manager": "xcodes", "tool": "xcode", "version": "26.2",
                "stage": "privileges", "code": "xcode_privileges_missing",
                "failed_at": "2026-11-20T10:00:00.000Z"}]),
            ),
            &"mac-mini".parse().unwrap(),
        );
        let actions = &items[0].actions;
        assert_eq!(actions.len(), 2);
        assert_eq!(
            actions[0].command,
            ["elsecall", "tools", "privileges", "install"]
        );
        assert_eq!(
            (actions[0].run_on.as_str(), actions[0].person_only),
            ("macbook-pro", true)
        );
        assert!(!actions[1].person_only, "retrying is an agent's");
    }

    #[test]
    fn an_xcode_no_member_can_copy_is_one_item_for_a_person_on_the_master() {
        let not_on_master = |at: &str| {
            json!({"manager": "xcodes", "tool": "xcode", "version": "26.4", "stage": "copy",
                   "code": "xcode_not_on_master", "failed_at": at})
        };
        let first = summary(json!([not_on_master("2026-11-20T10:00:00.000Z")]));
        let second = summary(json!([not_on_master("2026-11-20T09:30:00.000Z")]));
        let master: MemberName = "mac-mini".parse().unwrap();
        assert!(
            failed_steps(&"macbook-pro".parse().unwrap(), &first, &master).is_empty(),
            "not a failed step to retry"
        );
        let items = xcode_downloads([&first, &second].into_iter(), &master);
        assert_eq!(items.len(), 1, "one per version, whichever members need it");
        let item = serde_json::to_value(&items[0]).unwrap();
        assert_eq!(item["id"], "xcode_apple_id_required:mac-mini:26.4");
        assert_eq!(item["category"], "xcode_apple_id_required");
        assert_eq!(item["member"], "mac-mini");
        assert_eq!(item["since"], "2026-11-20T09:30:00.000Z");
        assert_eq!(item["actions"][0]["person_only"], true);
        assert_eq!(
            item["actions"][0]["command"],
            json!(["elsecall", "tools", "apply"])
        );
    }
}
