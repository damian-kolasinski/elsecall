//! What in the fleet needs attention: conditions Elsecall cannot resolve by
//! itself, each naming its member and the actions that resolve it. `elsecall
//! fleet` and `GET /v1/fleet` list them as `attention`; `docs/fleet-attention.md`
//! describes the model and its categories.
//!
//! Items are derived from the fleet's state each time they are read, so one
//! disappears as soon as its condition ends. Its `id` is built from what the
//! condition is about, so it stays the same across reads while the condition
//! lasts, and a condition that returns gets the same `id` again.
use crate::scheduling_model::timestamp::Timestamp;
use serde::{Deserialize, Serialize};

/// One condition that needs an agent or a person.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Item {
    /// `<category>:<member>`, followed by `:<subject>` when the item has one.
    pub id: String,
    /// The member the condition is about.
    pub member: String,
    pub category: Category,
    /// What on the member the condition is about, for a category that can hold
    /// several items per member, such as a tool's name; null otherwise.
    pub subject: Option<String>,
    /// The condition, in the vocabulary of error codes, such as `version_skew`.
    pub code: String,
    pub message: String,
    pub hint: String,
    /// When the condition began, when the member records it; null otherwise.
    pub since: Option<Timestamp>,
    /// What resolves it, the first action first.
    pub actions: Vec<Action>,
}

/// What kind of attention an item needs. A category is the unit an operator
/// trusts an agent with: every item in one has the same kind of cause and is
/// resolved by the same kind of action, so accepting a category's future items
/// automatically means one thing. A new kind of condition gets a new category
/// rather than widening an existing one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// A member enrolled before its tools were installed: placement gives it no
    /// work until it is marked ready.
    AwaitingProvisioning,
    /// A member runs another build than the master: placement and the other work
    /// members exchange are refused between them.
    VersionSkew,
    /// A run placed on a member failed because a tool it called is missing
    /// there, and no pin asks for it: pin it, so every member installs it.
    MissingToolPin,
    /// A change to files that look like secrets waits on the master before
    /// configuration sync sends it: only a person confirms it.
    SecretConsent,
    /// A step of a member's tool plan failed to install there: the step names
    /// the tool and the stage that failed, never what a command printed.
    /// Applying the plan again retries it.
    ProvisioningStepFailed,
    /// A member needs an Xcode the master does not have, so cannot copy: a
    /// person downloads it onto the master, with an Apple ID.
    XcodeAppleIdRequired,
    /// A category a newer build added, as this build reads it: shown, never
    /// accepted automatically. No producer makes it.
    #[serde(other)]
    Unknown,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingProvisioning => "awaiting_provisioning",
            Self::VersionSkew => "version_skew",
            Self::MissingToolPin => "missing_tool_pin",
            Self::SecretConsent => "secret_consent",
            Self::ProvisioningStepFailed => "provisioning_step_failed",
            Self::XcodeAppleIdRequired => "xcode_apple_id_required",
            Self::Unknown => "unknown",
        }
    }
}

/// One way to resolve an item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Action {
    /// What it does, in one sentence.
    pub summary: String,
    /// The `elsecall` command that does it, as its arguments, `elsecall` first:
    /// ready to run without a shell, whatever its subject holds.
    pub command: Vec<String>,
    /// The member to run the command on, such as the master for fleet changes.
    pub run_on: String,
    /// Whether only a person can complete it: the command asks for a password or
    /// shows a prompt on a member's screen, or the step is a decision an agent
    /// must not make alone. An agent may still run the command to start it.
    pub person_only: bool,
}

impl Item {
    /// The identity of `category`'s item about `member`, and its `subject` if any.
    pub fn id(category: Category, member: &str, subject: Option<&str>) -> String {
        match subject {
            None => format!("{}:{member}", category.as_str()),
            Some(subject) => format!("{}:{member}:{subject}", category.as_str()),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::attention::*;
    use serde_json::json;

    #[test]
    fn an_item_reads_as_agents_see_it() {
        let category = Category::AwaitingProvisioning;
        let item = Item {
            id: Item::id(category, "macbook-pro", None),
            member: "macbook-pro".into(),
            category,
            subject: None,
            code: "member_awaiting_provisioning".into(),
            message: "waits".into(),
            hint: "provision it".into(),
            since: None,
            actions: vec![Action {
                summary: "Mark it ready.".into(),
                command: vec![
                    "elsecall".into(),
                    "fleet".into(),
                    "ready".into(),
                    "macbook-pro".into(),
                ],
                run_on: "mac-mini".into(),
                person_only: false,
            }],
        };
        let rendered = serde_json::to_value(&item).unwrap();
        assert_eq!(
            rendered,
            json!({
                "id": "awaiting_provisioning:macbook-pro",
                "member": "macbook-pro",
                "category": "awaiting_provisioning",
                "subject": null,
                "code": "member_awaiting_provisioning",
                "message": "waits",
                "hint": "provision it",
                "since": null,
                "actions": [{
                    "summary": "Mark it ready.",
                    "command": ["elsecall", "fleet", "ready", "macbook-pro"],
                    "run_on": "mac-mini",
                    "person_only": false
                }]
            })
        );
        assert_eq!(serde_json::from_value::<Item>(rendered).unwrap(), item);
    }

    #[test]
    fn a_subject_narrows_the_identity_and_unknown_categories_still_read() {
        assert_eq!(
            Item::id(Category::VersionSkew, "mac-mini", Some("xcode")),
            "version_skew:mac-mini:xcode"
        );
        for category in [
            Category::AwaitingProvisioning,
            Category::VersionSkew,
            Category::MissingToolPin,
            Category::SecretConsent,
            Category::ProvisioningStepFailed,
            Category::XcodeAppleIdRequired,
        ] {
            assert_eq!(
                serde_json::to_value(category).unwrap(),
                json!(category.as_str())
            );
        }
        assert_eq!(
            serde_json::from_value::<Category>(json!("tool_drift")).unwrap(),
            Category::Unknown,
            "a newer build's category reads, as one this build does not know"
        );
    }
}
