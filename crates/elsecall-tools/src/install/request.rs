//! The steps the master sends a member: one version of one tool each, as the
//! member's plan lists it, with the install options its pins carry.
//!
//! A member installs only what such a request names, and the request is
//! untrusted input even from the master: each step is checked against its
//! manager's grammar before anything runs, so nothing in it can read as a
//! command-line option or carry an option Elsecall does not.
use crate::{
    pins::{InstallOptions, Platform, dotted_version},
    plan::member::Step,
    tool_inventory::{Manager, ToolName, ToolVersion},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// More steps than one plan has; a bound on the request, which must also fit
/// the member channel's 64 KiB body.
pub(crate) const MAXIMUM_STEPS: usize = 256;

/// One version of one tool to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstallStep {
    pub(crate) manager: Manager,
    pub(crate) tool: ToolName,
    pub(crate) version: ToolVersion,
    /// Only mise steps carry options.
    pub(crate) options: InstallOptions,
}

impl InstallStep {
    pub(crate) fn of(step: &Step) -> Self {
        Self {
            manager: step.key.manager,
            tool: step.key.tool.clone(),
            version: step.version.clone(),
            options: step.options.clone(),
        }
    }
}

/// A step as it travels, and as records and output show it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireStep {
    pub(crate) manager: Manager,
    pub(crate) tool: String,
    pub(crate) version: String,
    #[serde(default)]
    pub(crate) options: BTreeMap<String, String>,
}

impl WireStep {
    pub(crate) fn of(step: &InstallStep) -> Self {
        Self {
            manager: step.manager,
            tool: step.tool.as_str().to_owned(),
            version: step.version.as_str().to_owned(),
            options: step.options.to_map(),
        }
    }

    /// The step, if it follows its manager's grammar; otherwise the broken rule.
    pub(crate) fn accept(self) -> Result<InstallStep, &'static str> {
        let plain = |text: &str| !text.starts_with('-') && text.len() <= 160;
        let tool = ToolName::new(self.tool.as_str())
            .filter(|_| plain(&self.tool))
            .ok_or("a tool is not a name")?;
        let version = match self.manager {
            Manager::Mise => {
                // A backend may name a scoped package (`npm:@scope/tool`); a
                // plain name never holds `@`, which would read as a version.
                if self.tool.contains(['[', ']'])
                    || (self.tool.contains('@') && !self.tool.contains(':'))
                {
                    return Err("a mise tool name holds options or a version");
                }
                ToolVersion::new(self.version.as_str())
                    .filter(|_| plain(&self.version) && !self.version.contains(['[', ']']))
            }
            Manager::Xcodes if self.tool == "xcode" => dotted_version(&self.version, 2),
            Manager::Simulators
                if Platform::ALL
                    .iter()
                    .any(|platform| platform.as_str() == self.tool) =>
            {
                dotted_version(&self.version, 2)
            }
            Manager::Xcodes | Manager::Simulators => {
                return Err("a tool is not one its manager installs");
            }
        }
        .ok_or("a version does not follow its manager's grammar")?;
        if self.manager != Manager::Mise && !self.options.is_empty() {
            return Err("only a mise step carries install options");
        }
        Ok(InstallStep {
            manager: self.manager,
            tool,
            version,
            options: InstallOptions::from_wire(self.options)?,
        })
    }
}

/// What the master sends: the member's steps, in plan order.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApplyRequest {
    pub(crate) steps: Vec<WireStep>,
}

impl ApplyRequest {
    pub(crate) fn accept(self) -> Result<Vec<InstallStep>, &'static str> {
        if self.steps.is_empty() || self.steps.len() > MAXIMUM_STEPS {
            return Err("a request names 1 to 256 steps");
        }
        let steps = self
            .steps
            .into_iter()
            .map(WireStep::accept)
            .collect::<Result<Vec<_>, _>>()?;
        for (index, step) in steps.iter().enumerate() {
            if steps[..index].contains(step) {
                return Err("a request names one step twice");
            }
        }
        Ok(steps)
    }
}

#[cfg(test)]
mod tests {
    use crate::install::request::*;
    use serde_json::json;

    fn step(manager: &str, tool: &str, version: &str) -> serde_json::Value {
        json!({"manager": manager, "tool": tool, "version": version})
    }

    fn accepted(steps: Vec<serde_json::Value>) -> Result<Vec<InstallStep>, &'static str> {
        serde_json::from_value::<ApplyRequest>(json!({ "steps": steps }))
            .unwrap()
            .accept()
    }

    #[test]
    fn steps_follow_their_managers_grammar() {
        let steps = accepted(vec![
            json!({"manager": "mise", "tool": "github:nextest-rs/nextest", "version": "0.9.150",
                   "options": {"version_prefix": "cargo-nextest-"}}),
            step("mise", "npm:@scope/tool", "1.0.0"),
            step("xcodes", "xcode", "26.2"),
            step("simulators", "iOS", "26.2"),
        ])
        .unwrap();
        assert_eq!(steps.len(), 4);
        assert_eq!(
            steps[0].options.to_map()["version_prefix"],
            "cargo-nextest-"
        );
        for bad in [
            step("mise", "-x", "1.0.0"),
            step("mise", "node", "--force"),
            step("mise", "node[postinstall=x]", "1.0.0"),
            step("mise", "node@22", "1.0.0"),
            step("xcodes", "xcode", "latest"),
            step("xcodes", "xcode", "26"),
            step("xcodes", "Xcode", "26.2"),
            step("simulators", "iOS", "26"),
            step("simulators", "macOS", "26.2"),
            json!({"manager": "xcodes", "tool": "xcode", "version": "26.2", "options": {"bin": "x"}}),
            json!({"manager": "mise", "tool": "node", "version": "1", "options": {"postinstall": "x"}}),
        ] {
            assert!(accepted(vec![bad.clone()]).is_err(), "{bad}");
        }
        assert!(accepted(Vec::new()).is_err());
        assert!(accepted(vec![step("mise", "node", "1.0.0"); 2]).is_err());
        assert!(accepted(vec![step("mise", "node", "1.0.0"); MAXIMUM_STEPS + 1]).is_err());
        assert!(
            serde_json::from_value::<ApplyRequest>(json!({"steps": [], "run": "sh"})).is_err(),
            "unknown fields are refused"
        );
    }
}
