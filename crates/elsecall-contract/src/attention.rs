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
    /// `<category>:<member>`, followed by `:<subject>` for a category that can
    /// hold several items per member.
    pub id: String,
    /// The member the condition is about.
    pub member: String,
    pub category: Category,
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
/// rather than widening an existing one; readers show categories they do not
/// know and never accept them automatically.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// A member enrolled before its tools were installed: placement gives it no
    /// work until it is marked ready.
    AwaitingProvisioning,
    /// A member runs another build than the master: placement and the other work
    /// members exchange are refused between them.
    VersionSkew,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingProvisioning => "awaiting_provisioning",
            Self::VersionSkew => "version_skew",
        }
    }
}

/// One way to resolve an item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Action {
    /// What it does, in one sentence.
    pub summary: String,
    /// The `elsecall` command that does it, ready to run.
    pub command: String,
    /// The member to run the command on, such as the master for fleet changes.
    pub run_on: String,
    /// Whether only a person can complete it: the command asks for a password or
    /// shows a prompt on a member's screen, or the step is a decision an agent
    /// must not make alone. An agent may still run the command to start it.
    pub person_only: bool,
}

impl Item {
    /// The identity of `category`'s item about `member`, narrowed to one
    /// `subject` (such as a tool's name) for a category that can hold several
    /// items per member.
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
            code: "member_awaiting_provisioning".into(),
            message: "waits".into(),
            hint: "provision it".into(),
            since: None,
            actions: vec![Action {
                summary: "Mark it ready.".into(),
                command: "elsecall fleet ready macbook-pro".into(),
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
                "code": "member_awaiting_provisioning",
                "message": "waits",
                "hint": "provision it",
                "since": null,
                "actions": [{
                    "summary": "Mark it ready.",
                    "command": "elsecall fleet ready macbook-pro",
                    "run_on": "mac-mini",
                    "person_only": false
                }]
            })
        );
        assert_eq!(serde_json::from_value::<Item>(rendered).unwrap(), item);
    }

    #[test]
    fn a_subject_narrows_the_identity_and_categories_name_themselves() {
        assert_eq!(
            Item::id(Category::VersionSkew, "mac-mini", Some("xcode")),
            "version_skew:mac-mini:xcode"
        );
        for category in [Category::AwaitingProvisioning, Category::VersionSkew] {
            assert_eq!(
                serde_json::to_value(category).unwrap(),
                json!(category.as_str())
            );
        }
    }
}
