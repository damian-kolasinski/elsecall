//! A consumer repository's `.elsecall.toml`: the only place consumer-specific
//! knowledge enters Elsecall. It lives at the root of the working tree and is
//! validated as untrusted input; without one, defaults apply.
//!
//! ```toml
//! [outputs]
//! paths = ["build/reports", "build/logs/test.log"]
//!
//! [workspace]
//! refs = ["refs/remotes/origin/release/*"]
//!
//! [environment]
//! names = ["CI", "BUILD_NUMBER"]
//! prefixes = ["APP_"]
//!
//! [consistency]
//! configuration = ["~/.gradle/gradle.properties"]
//! [consistency.toolchains]
//! xcode = ["xcodebuild", "-version"]
//!
//! [admission]
//! when_unavailable = "run"
//!
//! [[labels]]
//! match = "build *"
//! size = 4
//! [[labels]]
//! match = "warm *"
//! size = 4
//! urgency = "deferrable"
//! deadline = "30m"
//! at_deadline = "start"
//! ```
use crate::{
    failure::{Failure, Result},
    label_pattern,
    scheduling::{
        capacity::Cores,
        request::{AtDeadline, MAXIMUM_DEADLINE, MAXIMUM_HOLD_LIMIT},
    },
    workspace::{
        carried_refs::{self, RefPattern},
        path::WorkspacePath,
    },
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

pub const FILE_NAME: &str = ".elsecall.toml";
const LIMIT: u64 = 64 * 1024;

const MAXIMUM_OUTPUTS: usize = 64;
const MAXIMUM_VARIABLES: usize = 256;
const MAXIMUM_TOOLCHAINS: usize = 16;
const MAXIMUM_ARGUMENTS: usize = 16;
/// Most configuration locations a consumer may declare.
pub const MAXIMUM_LOCATIONS: usize = 32;
const MAXIMUM_LABELS: usize = 64;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConsumerConfiguration {
    /// Produced paths brought back to the caller after a placed run.
    pub outputs: Vec<WorkspacePath>,
    /// Refs a copy carries besides HEAD, its upstream and remotes' default
    /// branches.
    pub workspace_refs: Vec<RefPattern>,
    /// Variables placed work receives from the caller, by exact name.
    pub forward_names: Vec<String>,
    /// Variables placed work receives from the caller, by name prefix.
    pub forward_prefixes: Vec<String>,
    /// Commands whose output identifies a toolchain, compared across members.
    pub toolchains: BTreeMap<String, Vec<String>>,
    /// Machine-level configuration files or directories compared across members:
    /// absolute, or relative to the account's home with `~/`.
    pub configuration: Vec<ConfigurationLocation>,
    /// Label rules in file order; the first whose pattern matches applies.
    pub labels: Vec<LabelRule>,
    /// What `run` and `lease` do when the local service cannot be reached.
    pub when_unavailable: Option<WhenUnavailable>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigurationLocation {
    Absolute(PathBuf),
    /// Relative to the home directory of the account that runs the work.
    Home(PathBuf),
}
impl ConfigurationLocation {
    pub fn display(&self) -> String {
        match self {
            ConfigurationLocation::Absolute(path) => path.display().to_string(),
            ConfigurationLocation::Home(path) => format!("~/{}", path.display()),
        }
    }
    pub fn resolve(&self, home: &Path) -> PathBuf {
        match self {
            ConfigurationLocation::Absolute(path) => path.clone(),
            ConfigurationLocation::Home(path) => home.join(path),
        }
    }
}

/// What a request does when the local service cannot be reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WhenUnavailable {
    /// Fail with `service_unavailable`; the command does not run.
    #[default]
    Fail,
    /// Proceed unmanaged: `run` runs the command directly, and a lease is not held.
    Run,
}
impl WhenUnavailable {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "fail" => Some(Self::Fail),
            "run" => Some(Self::Run),
            _ => None,
        }
    }
}

/// Request properties for labels that match `pattern`, where `*` matches any
/// text and `?` any one character. Explicit CLI flags override them.
#[derive(Clone, Debug, PartialEq)]
pub struct LabelRule {
    /// The rule's position among the file's rules, from 1.
    pub number: usize,
    pub pattern: String,
    pub size: Option<Cores>,
    pub urgency: Option<RuleUrgency>,
    pub placement: Option<RulePlacement>,
}

/// The urgency a rule gives, with the properties that belong to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleUrgency {
    /// Set by `urgency = "interactive"`, or implied by `hold_limit`.
    Interactive { hold_limit: Option<Duration> },
    Deferrable {
        /// How long after the request its deadline is.
        deadline: Duration,
        at_deadline: AtDeadline,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RulePlacement {
    Pinned,
    Placeable,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    outputs: Outputs,
    #[serde(default)]
    workspace: Workspace,
    #[serde(default)]
    environment: Environment,
    #[serde(default)]
    consistency: Consistency,
    #[serde(default)]
    admission: Admission,
    #[serde(default)]
    labels: Vec<RawRule>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Outputs {
    #[serde(default)]
    paths: Vec<String>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Workspace {
    #[serde(default)]
    refs: Vec<String>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Environment {
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    prefixes: Vec<String>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Consistency {
    #[serde(default)]
    toolchains: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    configuration: Vec<String>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Admission {
    #[serde(default)]
    when_unavailable: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    /// `pattern` is this key's earlier name.
    #[serde(rename = "match", alias = "pattern")]
    pattern: String,
    #[serde(default)]
    size: Option<Size>,
    #[serde(default)]
    urgency: Option<String>,
    #[serde(default)]
    hold_limit: Option<String>,
    #[serde(default)]
    deadline: Option<String>,
    #[serde(default)]
    at_deadline: Option<String>,
    #[serde(default)]
    placement: Option<String>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Size {
    Number(f64),
    Text(String),
}

/// Why a configuration is invalid and, when it is specific, how to fix it.
#[derive(Debug, PartialEq)]
pub struct Problem {
    pub detail: String,
    pub fix: Option<String>,
}
impl From<String> for Problem {
    fn from(detail: String) -> Self {
        Self { detail, fix: None }
    }
}

fn invalid(path: &Path, problem: impl Into<Problem>) -> Failure {
    let Problem { detail, fix } = problem.into();
    let file = path.display();
    Failure::new(
        "invalid_request",
        format!("{file} is invalid: {detail}"),
        match fix {
            Some(fix) => format!("{fix} in {file}, then rerun the same command."),
            None => format!(
                "Fix {file} (README, \"Consumer configuration\", lists every setting), then rerun the same command."
            ),
        },
    )
}

fn variable_name(name: &str, prefix: bool) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (prefix || !bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

impl ConsumerConfiguration {
    /// Reads `.elsecall.toml` at the working tree `root`; defaults without one.
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(FILE_NAME);
        let mut text = String::new();
        match std::fs::File::open(&path) {
            Ok(file) => file
                .take(LIMIT + 1)
                .read_to_string(&mut text)
                .map_err(|e| invalid(&path, format!("it could not be read ({e})")))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(invalid(&path, format!("it could not be read ({e})"))),
        };
        if text.len() as u64 > LIMIT {
            return Err(invalid(&path, "it is larger than 64 KiB.".to_owned()));
        }
        Self::parse(&text).map_err(|problem| invalid(&path, problem))
    }

    /// Finds the configuration for work requested in `directory`: the one at the
    /// root of the working tree containing it, found by its `.git` entry.
    pub fn discover(directory: &Path) -> Result<Self> {
        let mut current = Some(directory);
        while let Some(dir) = current {
            if std::fs::symlink_metadata(dir.join(".git")).is_ok() {
                return Self::load(dir);
            }
            current = dir.parent();
        }
        Ok(Self::default())
    }

    pub fn parse(text: &str) -> std::result::Result<Self, Problem> {
        let file: File = toml::from_str(text).map_err(|e| {
            let line = e
                .span()
                .map(|span| text[..span.start.min(text.len())].lines().count().max(1));
            Problem::from(match line {
                Some(line) => format!("line {line}: {}", e.message().trim_end()),
                None => e.message().trim_end().to_owned(),
            })
        })?;
        let mut outputs = Vec::new();
        if file.outputs.paths.len() > MAXIMUM_OUTPUTS {
            return Err(format!("at most {MAXIMUM_OUTPUTS} output paths.").into());
        }
        for path in &file.outputs.paths {
            let path = WorkspacePath::parse(path.trim_end_matches('/')).ok_or_else(|| {
                format!(
                    "output {path:?} must be a relative path inside the working tree, outside .git."
                )
            })?;
            if outputs
                .iter()
                .any(|o: &WorkspacePath| o.is_within(&path) || path.is_within(o))
            {
                return Err(format!("output {path} overlaps another output.").into());
            }
            outputs.push(path);
        }
        if file.workspace.refs.len() > carried_refs::MAXIMUM_PATTERNS {
            return Err(format!(
                "at most {} ref patterns under [workspace] refs.",
                carried_refs::MAXIMUM_PATTERNS
            )
            .into());
        }
        let workspace_refs = file
            .workspace
            .refs
            .iter()
            .map(|pattern| {
                RefPattern::parse(pattern).ok_or_else(|| Problem {
                    detail: format!(
                        "[workspace] refs: {pattern:?} is not a full ref name under refs/remotes/ or refs/tags/, where * matches within one name component."
                    ),
                    fix: Some(
                        "Write it as a pattern such as \"refs/remotes/origin/release/*\" or \"refs/tags/v*\" under [workspace] refs".into(),
                    ),
                })
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let environment = &file.environment;
        if environment.names.len() + environment.prefixes.len() > MAXIMUM_VARIABLES {
            return Err(format!("at most {MAXIMUM_VARIABLES} variable names and prefixes.").into());
        }
        for (list, prefix) in [(&environment.names, false), (&environment.prefixes, true)] {
            for name in list {
                if !variable_name(name, prefix) {
                    return Err(format!(
                        "{name:?} is not a variable name of letters, digits and underscores."
                    )
                    .into());
                }
                if name.starts_with("ELSECALL_")
                    || (prefix && "ELSECALL_".starts_with(name.as_str()))
                {
                    return Err(format!(
                        "{name} would forward Elsecall's own variables, which Elsecall sets."
                    )
                    .into());
                }
            }
        }
        let consistency = file.consistency;
        check_toolchains(&consistency.toolchains)?;
        if consistency.configuration.len() > MAXIMUM_LOCATIONS {
            return Err(format!("at most {MAXIMUM_LOCATIONS} configuration locations.").into());
        }
        let configuration = consistency
            .configuration
            .iter()
            .map(|location| parse_location(location))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if file.labels.len() > MAXIMUM_LABELS {
            return Err(format!("at most {MAXIMUM_LABELS} label rules.").into());
        }
        let labels = file
            .labels
            .into_iter()
            .enumerate()
            .map(|(index, rule)| parse_rule(index + 1, rule))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let when_unavailable = file
            .admission
            .when_unavailable
            .map(|text| {
                WhenUnavailable::parse(&text).ok_or_else(|| Problem {
                    detail: format!("[admission] when_unavailable = {text:?} is not run or fail."),
                    fix: Some(
                        "Set when_unavailable = \"run\" or \"fail\" under [admission]".into(),
                    ),
                })
            })
            .transpose()?;
        Ok(Self {
            outputs,
            workspace_refs,
            forward_names: environment.names.clone(),
            forward_prefixes: environment.prefixes.clone(),
            toolchains: consistency.toolchains,
            configuration,
            labels,
            when_unavailable,
        })
    }

    /// The first label rule whose pattern matches `label`.
    pub fn rule_for(&self, label: &str) -> Option<&LabelRule> {
        self.labels
            .iter()
            .find(|rule| label_pattern::matches(&rule.pattern, label))
    }

    /// Whether the caller's variable `name` is forwarded to placed work.
    pub fn forwards(&self, name: &str) -> bool {
        self.forward_names.iter().any(|n| n == name)
            || self
                .forward_prefixes
                .iter()
                .any(|p| name.starts_with(p.as_str()))
    }
}

/// Reads one label rule, saying exactly what to change when it is invalid.
fn parse_rule(number: usize, rule: RawRule) -> std::result::Result<LabelRule, Problem> {
    let name = format!("[[labels]] rule {number} (match = {:?})", rule.pattern);
    let problem = |detail: &str, fix: &str| Problem {
        detail: format!("{name}: {detail}"),
        fix: Some(format!("{fix} in rule {number}")),
    };
    if rule.pattern.is_empty()
        || rule.pattern.chars().count() > 200
        || rule.pattern.chars().any(char::is_control)
    {
        return Err(problem(
            "match must be 1 to 200 characters without control characters.",
            "Correct match",
        ));
    }
    let size = rule
        .size
        .map(|size| {
            match size {
                Size::Number(n) => Cores::from_f64(n),
                Size::Text(t) => Cores::parse(&t),
            }
            .filter(|c| *c != Cores::ZERO)
            .ok_or_else(|| {
                problem(
                    "size must be a positive number of cores, such as 4 or 0.5.",
                    "Correct size",
                )
            })
        })
        .transpose()?;
    let duration = |key: &str, text: &str, maximum: Duration, most: &str| {
        humantime::parse_duration(text)
            .ok()
            .filter(|d| *d <= maximum)
            .ok_or_else(|| {
                problem(
                    &format!(
                        "{key} = {text:?} must be a duration such as 90s, 30m or 2h, at most {most}."
                    ),
                    &format!("Correct {key}"),
                )
            })
    };
    let hold_limit = rule
        .hold_limit
        .as_deref()
        .map(|text| duration("hold_limit", text, MAXIMUM_HOLD_LIMIT, "24h"))
        .transpose()?;
    let deadline = rule
        .deadline
        .as_deref()
        .map(|text| duration("deadline", text, MAXIMUM_DEADLINE, "7 days"))
        .transpose()?;
    let at_deadline = rule
        .at_deadline
        .as_deref()
        .map(|text| match text {
            "start" => Ok(AtDeadline::Start),
            "skip" => Ok(AtDeadline::Skip),
            _ => Err(problem(
                &format!("at_deadline = {text:?} is not start or skip."),
                "Set at_deadline = \"start\" or \"skip\"",
            )),
        })
        .transpose()?;
    let urgency = match (rule.urgency.as_deref(), hold_limit, deadline, at_deadline) {
        (None | Some("interactive"), hold_limit, None, None) => (rule.urgency.is_some()
            || hold_limit.is_some())
        .then_some(RuleUrgency::Interactive { hold_limit }),
        (Some("deferrable"), None, Some(deadline), Some(at_deadline)) => {
            Some(RuleUrgency::Deferrable {
                deadline,
                at_deadline,
            })
        }
        (Some("deferrable"), Some(_), _, _) => {
            return Err(problem(
                "hold_limit applies to interactive work; deferrable work waits until its deadline.",
                "Remove hold_limit, or set urgency = \"interactive\",",
            ));
        }
        (Some("deferrable"), None, _, _) => {
            return Err(problem(
                "urgency = \"deferrable\" needs both deadline and at_deadline.",
                "Add deadline (such as \"30m\") and at_deadline (\"start\" or \"skip\")",
            ));
        }
        (None | Some("interactive"), _, _, _) => {
            return Err(problem(
                "deadline and at_deadline apply only with urgency = \"deferrable\".",
                "Add urgency = \"deferrable\", or remove deadline and at_deadline,",
            ));
        }
        (Some(other), _, _, _) => {
            return Err(problem(
                &format!("urgency = {other:?} is not interactive or deferrable."),
                "Set urgency = \"interactive\" or \"deferrable\"",
            ));
        }
    };
    let placement = rule
        .placement
        .as_deref()
        .map(|text| match text {
            "pinned" => Ok(RulePlacement::Pinned),
            "placeable" => Ok(RulePlacement::Placeable),
            _ => Err(problem(
                &format!("placement = {text:?} is not pinned or placeable."),
                "Set placement = \"pinned\" or \"placeable\"",
            )),
        })
        .transpose()?;
    if placement == Some(RulePlacement::Placeable)
        && matches!(urgency, Some(RuleUrgency::Deferrable { .. }))
    {
        return Err(problem(
            "deferrable work is pinned only, so it cannot be placeable.",
            "Remove placement = \"placeable\" or the deferrable urgency",
        ));
    }
    Ok(LabelRule {
        number,
        pattern: rule.pattern,
        size,
        urgency,
        placement,
    })
}

/// Checks declared toolchains: at most 16, each a lowercase name and a command of
/// one to 16 arguments.
pub fn check_toolchains(
    toolchains: &BTreeMap<String, Vec<String>>,
) -> std::result::Result<(), String> {
    if toolchains.len() > MAXIMUM_TOOLCHAINS {
        return Err(format!("at most {MAXIMUM_TOOLCHAINS} toolchains."));
    }
    for (name, command) in toolchains {
        let valid_name = !name.is_empty()
            && name.len() <= 32
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'));
        if !valid_name {
            return Err(format!(
                "toolchain name {name:?} must be 1-32 lowercase letters, digits, - or _."
            ));
        }
        if command.is_empty()
            || command.len() > MAXIMUM_ARGUMENTS
            || command[0].is_empty()
            || command.iter().any(|a| a.len() > 1024 || a.contains('\0'))
        {
            return Err(format!(
                "toolchain {name} needs a command of 1 to {MAXIMUM_ARGUMENTS} arguments."
            ));
        }
    }
    Ok(())
}

/// Reads a configuration location: absolute, or starting with `~/`, normalized.
pub fn parse_location(location: &str) -> std::result::Result<ConfigurationLocation, String> {
    let normal = |path: &str| {
        !path.is_empty()
            && path.len() <= 1024
            && !path.contains('\0')
            && path
                .split('/')
                .all(|c| !c.is_empty() && c != "." && c != "..")
    };
    match location.strip_prefix("~/") {
        Some(relative) if normal(relative) => Ok(ConfigurationLocation::Home(relative.into())),
        None if location.starts_with('/') && normal(&location[1..]) => {
            Ok(ConfigurationLocation::Absolute(location.into()))
        }
        _ => Err(format!(
            "configuration location {location:?} must be absolute or start with ~/, without . or .."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_configuration() {
        let configuration = ConsumerConfiguration::parse(
            r#"
            [outputs]
            paths = ["build/reports/", "logs/test.log"]
            [environment]
            names = ["CI"]
            prefixes = ["APP_"]
            [consistency]
            configuration = ["~/.gradle/gradle.properties", "/etc/hosts"]
            [consistency.toolchains]
            xcode = ["xcodebuild", "-version"]
            [admission]
            when_unavailable = "run"
            [[labels]]
            match = "test *"
            size = 4
            hold_limit = "5m"
            [[labels]]
            match = "warm *"
            urgency = "deferrable"
            deadline = "30m"
            at_deadline = "start"
            [[labels]]
            match = "build *"
            placement = "placeable"
            urgency = "interactive"
            [[labels]]
            pattern = "*"
            size = "0.5"
            "#,
        )
        .unwrap();
        assert_eq!(configuration.outputs[0].as_str(), "build/reports");
        assert!(configuration.forwards("CI") && configuration.forwards("APP_TOKEN"));
        assert!(!configuration.forwards("CIX") && !configuration.forwards("HOME"));
        assert_eq!(configuration.when_unavailable, Some(WhenUnavailable::Run));
        let test = configuration.rule_for("test FeatureModule").unwrap();
        assert_eq!(test.number, 1);
        assert_eq!(test.size, Some(Cores::from_millis(4000)));
        assert_eq!(
            test.urgency,
            Some(RuleUrgency::Interactive {
                hold_limit: Some(Duration::from_secs(300))
            })
        );
        assert_eq!(
            configuration.rule_for("warm Cache").unwrap().urgency,
            Some(RuleUrgency::Deferrable {
                deadline: Duration::from_secs(1800),
                at_deadline: AtDeadline::Start
            })
        );
        let build = configuration.rule_for("build App").unwrap();
        assert_eq!(build.placement, Some(RulePlacement::Placeable));
        assert_eq!(
            build.urgency,
            Some(RuleUrgency::Interactive { hold_limit: None })
        );
        let fallback = configuration.rule_for("lint").unwrap();
        assert_eq!(
            (fallback.number, fallback.size, fallback.urgency),
            (4, Some(Cores::from_millis(500)), None),
            "the earlier key name still works, and nothing implies an urgency"
        );
        assert_eq!(
            configuration.configuration[0].resolve(Path::new("/Users/x")),
            PathBuf::from("/Users/x/.gradle/gradle.properties")
        );
        assert_eq!(
            ConsumerConfiguration::parse("").unwrap(),
            ConsumerConfiguration::default()
        );
    }

    #[test]
    fn the_first_matching_rule_wins() {
        let configuration = ConsumerConfiguration::parse(
            "[[labels]]\nmatch = \"build App*\"\nsize = 6\n[[labels]]\nmatch = \"build *\"\nsize = 4\n",
        )
        .unwrap();
        assert_eq!(configuration.rule_for("build AppTests").unwrap().number, 1);
        assert_eq!(configuration.rule_for("build Widget").unwrap().number, 2);
        assert!(configuration.rule_for("test App").is_none());
    }

    #[test]
    fn refuses_unsafe_or_unknown_settings() {
        for bad in [
            "[outputs]\npaths = [\"../outside\"]",
            "[outputs]\npaths = [\"/etc\"]",
            "[outputs]\npaths = [\".git/hooks\"]",
            "[outputs]\npaths = [\"build\", \"build/logs\"]",
            "[environment]\nnames = [\"A-B\"]",
            "[environment]\nprefixes = [\"ELSECALL_\"]",
            "[environment]\nprefixes = [\"ELSE\"]",
            "[consistency]\nconfiguration = [\"relative/path\"]",
            "[consistency]\nconfiguration = [\"~/../etc\"]",
            "[consistency.toolchains]\nXcode = [\"x\"]",
            "[consistency.toolchains]\nxcode = []",
            "[admission]\nwhen_unavailable = \"maybe\"",
            "[unknown]",
            "[outputs]\npath = []",
            "[workspace]\nref = []",
        ] {
            assert!(ConsumerConfiguration::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn workspace_refs_are_patterns_under_remotes_or_tags() {
        let configuration = ConsumerConfiguration::parse(
            "[workspace]\nrefs = [\"refs/remotes/origin/release/*\", \"refs/tags/v*\"]\n",
        )
        .unwrap();
        let declared: Vec<&str> = configuration
            .workspace_refs
            .iter()
            .map(RefPattern::as_str)
            .collect();
        assert_eq!(declared, ["refs/remotes/origin/release/*", "refs/tags/v*"]);
        for bad in [
            "refs/heads/main",
            "origin/main",
            "refs/remotes/origin/a..b",
            "refs/tags/v[0-9]*",
            "--upload-pack=x",
        ] {
            let problem = ConsumerConfiguration::parse(&format!("[workspace]\nrefs = [{bad:?}]\n"))
                .unwrap_err();
            assert!(
                problem.detail.contains(bad) && problem.detail.contains("refs/remotes/"),
                "{problem:?}"
            );
            assert!(
                problem
                    .fix
                    .unwrap()
                    .contains("refs/remotes/origin/release/*")
            );
        }
        let many: Vec<String> = (0..=carried_refs::MAXIMUM_PATTERNS)
            .map(|n| format!("\"refs/tags/t{n}\""))
            .collect();
        let text = format!("[workspace]\nrefs = [{}]\n", many.join(", "));
        assert!(ConsumerConfiguration::parse(&text).is_err());
    }

    #[test]
    fn invalid_rules_say_which_rule_and_how_to_fix_it() {
        for (rule, detail, fix) in [
            (
                "size = 0",
                "size must be a positive number",
                "Correct size in rule 2",
            ),
            (
                "hold_limit = \"soon\"",
                "hold_limit = \"soon\" must be a duration",
                "Correct hold_limit in rule 2",
            ),
            (
                "hold_limit = \"25h\"",
                "at most 24h",
                "Correct hold_limit in rule 2",
            ),
            (
                "urgency = \"deferrable\"\ndeadline = \"30m\"",
                "needs both deadline and at_deadline",
                "Add deadline",
            ),
            (
                "urgency = \"deferrable\"\ndeadline = \"30m\"\nat_deadline = \"start\"\nhold_limit = \"1m\"",
                "hold_limit applies to interactive work",
                "Remove hold_limit",
            ),
            (
                "deadline = \"30m\"\nat_deadline = \"skip\"",
                "apply only with urgency = \"deferrable\"",
                "Add urgency = \"deferrable\"",
            ),
            (
                "urgency = \"deferrable\"\ndeadline = \"8 days\"\nat_deadline = \"skip\"",
                "at most 7 days",
                "Correct deadline",
            ),
            (
                "urgency = \"deferrable\"\ndeadline = \"30m\"\nat_deadline = \"later\"",
                "is not start or skip",
                "Set at_deadline",
            ),
            (
                "urgency = \"urgent\"",
                "is not interactive or deferrable",
                "Set urgency",
            ),
            (
                "placement = \"anywhere\"",
                "is not pinned or placeable",
                "Set placement",
            ),
            (
                "placement = \"placeable\"\nurgency = \"deferrable\"\ndeadline = \"30m\"\nat_deadline = \"skip\"",
                "pinned only",
                "Remove placement",
            ),
        ] {
            let text = format!("[[labels]]\nmatch = \"a\"\n[[labels]]\nmatch = \"x *\"\n{rule}\n");
            let problem = ConsumerConfiguration::parse(&text).unwrap_err();
            assert!(
                problem
                    .detail
                    .starts_with("[[labels]] rule 2 (match = \"x *\"): ")
                    && problem.detail.contains(detail),
                "{rule}: {problem:?}"
            );
            let given = problem.fix.unwrap();
            assert!(
                given.starts_with(fix) && given.ends_with(" in rule 2"),
                "{rule}: {given}"
            );
        }
        let unknown = ConsumerConfiguration::parse("[[labels]]\nmatch = \"x\"\nsise = 4\n")
            .unwrap_err()
            .detail;
        assert!(
            unknown.starts_with("line ") && unknown.contains("sise"),
            "{unknown}"
        );
    }

    #[test]
    fn the_readme_examples_are_valid() {
        let readme = include_str!("../README.md");
        let examples: Vec<&str> = readme
            .split("```toml\n")
            .skip(1)
            .map(|block| block.split("```").next().unwrap())
            .filter(|block| block.contains("[[labels]]"))
            .collect();
        assert_eq!(
            examples.len(),
            2,
            "the reference and the integration example"
        );
        for example in examples {
            let configuration = ConsumerConfiguration::parse(example)
                .unwrap_or_else(|problem| panic!("{problem:?} in\n{example}"));
            assert_eq!(configuration.when_unavailable, Some(WhenUnavailable::Run));
            assert!(matches!(
                configuration.rule_for("warm App").unwrap().urgency,
                Some(RuleUrgency::Deferrable { .. })
            ));
        }
    }

    #[test]
    fn discovers_the_file_at_the_working_tree_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        std::fs::create_dir_all(root.path().join("a/b")).unwrap();
        std::fs::write(
            root.path().join(FILE_NAME),
            "[environment]\nnames = [\"CI\"]\n",
        )
        .unwrap();
        let found = ConsumerConfiguration::discover(&root.path().join("a/b")).unwrap();
        assert_eq!(found.forward_names, ["CI"]);
        std::fs::write(root.path().join(FILE_NAME), "nonsense = 1\n").unwrap();
        let failure = ConsumerConfiguration::discover(&root.path().join("a")).unwrap_err();
        assert_eq!(failure.code, "invalid_request");
        assert!(failure.hint.contains(".elsecall.toml"), "{}", failure.hint);
    }
}
