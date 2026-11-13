//! Where a request's properties came from: the caller's flags, the caller's
//! environment, a label rule in the consumer configuration, or defaults. Records
//! keep it so `explain` shows which rule applied and what overrode it.
use crate::{
    failure::Result,
    scheduling::request::{Urgency, invalid},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// The `.elsecall.toml` label rule that matched the label, if any.
    pub rule: Option<AppliedRule>,
    pub size: Source,
    pub urgency: Source,
    /// Set for interactive requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_limit: Option<Source>,
    /// Set for deferrable requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<Source>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_deadline: Option<Source>,
    pub placement: Source,
}

/// A label rule, identified by its position among the file's rules and its pattern.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedRule {
    pub number: usize,
    #[serde(rename = "match")]
    pub pattern: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// A command-line flag.
    Cli,
    /// An `ELSECALL_*` variable of the caller.
    Environment,
    /// The applied label rule.
    Rule,
    /// Elsecall's default: for a size or hold limit, the service's own.
    Default,
}

const MAXIMUM_RULE: usize = 64;
const MAXIMUM_PATTERN: usize = 200;

impl Settings {
    /// Checks what arrived over the local API, as any request property is: a
    /// well-formed rule, sources that name a rule only when one applied, and
    /// exactly the properties of the request's urgency.
    pub fn validate(&self, urgency: &Urgency) -> Result<()> {
        let malformed = || Err(invalid("The request's settings are malformed."));
        if let Some(rule) = &self.rule
            && (!(1..=MAXIMUM_RULE).contains(&rule.number)
                || rule.pattern.is_empty()
                || rule.pattern.chars().count() > MAXIMUM_PATTERN
                || rule.pattern.chars().any(char::is_control))
        {
            return malformed();
        }
        let sources = [
            Some(self.size),
            Some(self.urgency),
            self.hold_limit,
            self.deadline,
            self.at_deadline,
            Some(self.placement),
        ];
        if self.rule.is_none() && sources.contains(&Some(Source::Rule)) {
            return malformed();
        }
        let consistent = match urgency {
            Urgency::Interactive { .. } => {
                self.hold_limit.is_some() && self.deadline.is_none() && self.at_deadline.is_none()
            }
            Urgency::Deferrable { .. } => {
                self.hold_limit.is_none() && self.deadline.is_some() && self.at_deadline.is_some()
            }
        };
        if consistent { Ok(()) } else { malformed() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduling::{request::AtDeadline, timestamp::Timestamp};

    fn interactive() -> Settings {
        Settings {
            rule: None,
            size: Source::Cli,
            urgency: Source::Default,
            hold_limit: Some(Source::Environment),
            deadline: None,
            at_deadline: None,
            placement: Source::Default,
        }
    }

    #[test]
    fn refuses_settings_that_contradict_the_request() {
        let urgency = Urgency::Interactive {
            hold_limit_ms: None,
        };
        let deferrable = Urgency::Deferrable {
            deadline: Timestamp::from_millis(1),
            at_deadline: AtDeadline::Skip,
        };
        assert!(interactive().validate(&urgency).is_ok());
        assert!(interactive().validate(&deferrable).is_err());
        let without_rule = Settings {
            size: Source::Rule,
            ..interactive()
        };
        assert!(without_rule.validate(&urgency).is_err());
        let with_rule = Settings {
            rule: Some(AppliedRule {
                number: 1,
                pattern: "build *".into(),
            }),
            ..without_rule
        };
        assert!(with_rule.validate(&urgency).is_ok());
        let control = Settings {
            rule: Some(AppliedRule {
                number: 1,
                pattern: "a\nb".into(),
            }),
            ..interactive()
        };
        assert!(control.validate(&urgency).is_err());
    }
}
