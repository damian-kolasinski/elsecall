//! `elsecall skill`: print the guide, set it up for the agents on this machine,
//! and the reports `install`, `uninstall` and `doctor` include.
use super::{
    agents::{self, Agent},
    folder::{self, State},
    guide,
    targets::{self, Done, Outcome, Target},
};
use clap::{Args, Subcommand};
use elsecall_contract::failure::{Failure, Result};
use elsecall_local_state::paths::Paths;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Args)]
pub struct SkillArgs {
    #[command(subcommand)]
    pub command: SkillCommand,
}

#[derive(Subcommand)]
pub enum SkillCommand {
    /// Print the guide the skill points agents at, or its topic NAME, as text
    Show {
        /// One of the topics the guide names, as in integrate
        name: Option<String>,
        /// Print one JSON document instead of text
        #[arg(long)]
        json: bool,
    },
    /// Install the skill into the agents on this machine, or those named with
    /// --agent; again, update what an older build installed
    Setup {
        /// Only AGENT; repeat for more [default: every agent found]
        #[arg(long = "agent", value_enum)]
        agents: Vec<Agent>,
        /// Report what would change without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Replace a skill named elsecall that Elsecall didn't write (never a link)
        #[arg(long)]
        force: bool,
    },
    /// Report whether each agent has the skill
    Status,
    /// Remove the skill Elsecall wrote from every agent, or those named with --agent
    Remove {
        /// Only AGENT; repeat for more [default: every agent]
        #[arg(long = "agent", value_enum)]
        agents: Vec<Agent>,
        /// Report what would change without changing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Write the whole skill folder for an agent setup doesn't know, and print
    /// where it is; run it again after upgrading
    Install {
        /// Write it to PATH/elsecall [default: Elsecall's data folder/skill]
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

/// What the command prints: the guide as text, or one JSON document and whether
/// the command did all it was asked.
pub enum Printed {
    Text(&'static str),
    Json(Value, bool),
}

const LOADED: &str = "New agent sessions load the skill; sessions running now don't.";

pub fn run(args: SkillArgs) -> Result<Printed> {
    match args.command {
        SkillCommand::Show { name, json } => show(name.as_deref(), json),
        SkillCommand::Setup {
            agents,
            dry_run,
            force,
        } => {
            let home = Paths::current()?.home;
            let chosen: Vec<Agent> = if agents.is_empty() {
                agents::ALL.into_iter().filter(|a| a.found(&home)).collect()
            } else {
                agents
            };
            if chosen.is_empty() {
                return Err(Failure::new(
                    "no_agents_found",
                    "No coding agent Elsecall knows was found for this account.",
                    "Name one with --agent claude, codex or opencode, or run elsecall skill install and point your agent at the folder it prints.",
                ));
            }
            let (data, complete) = setup(&home, &chosen, dry_run, force);
            Ok(Printed::Json(data, complete))
        }
        SkillCommand::Status => {
            let home = Paths::current()?.home;
            let agents: Vec<Value> = targets::inspect(&home, &agents::ALL)
                .iter()
                .map(|target| entry(&home, target))
                .collect();
            Ok(Printed::Json(json!({ "agents": agents }), true))
        }
        SkillCommand::Remove { agents, dry_run } => {
            let home = Paths::current()?.home;
            let chosen = if agents.is_empty() {
                agents::ALL.to_vec()
            } else {
                agents
            };
            let (data, complete) = remove(&home, &chosen, dry_run);
            Ok(Printed::Json(data, complete))
        }
        SkillCommand::Install { dir } => install(dir),
    }
}

fn show(name: Option<&str>, json: bool) -> Result<Printed> {
    let text = match name {
        None => guide::guide(),
        Some(name) => {
            guide::topic(name)
                .ok_or_else(|| {
                    let names = guide::names();
                    Failure::new(
                        "invalid_request",
                        format!(
                            "The guide has no such topic; its topics are: {}.",
                            names.join(", ")
                        ),
                        format!("Run elsecall skill show {}.", names[0]),
                    )
                })?
                .text
        }
    };
    Ok(if json {
        Printed::Json(json!({ "name": name, "text": text }), true)
    } else {
        Printed::Text(text)
    })
}

/// `setup` for `chosen`, and whether it left no conflict or failure.
pub fn setup(home: &Path, chosen: &[Agent], dry_run: bool, force: bool) -> (Value, bool) {
    let targets = targets::inspect(home, chosen);
    let outcomes = targets::setup(&targets, dry_run, force);
    let complete = settled(&outcomes);
    let mut data = json!({
        "dry_run": dry_run,
        "agents": described(home, &targets, &outcomes),
        "note": LOADED,
    });
    let conflicts: Vec<State> = targets
        .iter()
        .zip(&outcomes)
        .filter(|(_, outcome)| outcome.done == Done::Conflict)
        .filter_map(|(target, _)| target.before.as_ref().ok().copied())
        .collect();
    if conflicts.contains(&State::Foreign) {
        data["hint"] = json!(
            "A skill named elsecall that Elsecall did not write was left alone. Run elsecall skill setup --force to replace its SKILL.md."
        );
    } else if conflicts.contains(&State::Linked) {
        data["hint"] = json!(
            "A linked skill folder was left alone. Remove the link, then run elsecall skill setup again."
        );
    }
    (data, complete)
}

/// `remove` for `chosen`, and whether nothing failed.
pub fn remove(home: &Path, chosen: &[Agent], dry_run: bool) -> (Value, bool) {
    let targets = targets::inspect(home, chosen);
    let outcomes = targets::remove(&targets, dry_run);
    let complete = settled(&outcomes);
    let data = json!({ "dry_run": dry_run, "agents": described(home, &targets, &outcomes) });
    (data, complete)
}

fn settled(outcomes: &[Outcome]) -> bool {
    !outcomes
        .iter()
        .any(|o| matches!(o.done, Done::Conflict | Done::Failed))
}

fn install(dir: Option<PathBuf>) -> Result<Printed> {
    let root = match dir {
        Some(dir) => std::path::absolute(&dir).map_err(|e| unwritable(&dir, &e))?,
        None => default_copy(&Paths::current()?)?,
    };
    let target = root.join(folder::NAME);
    let state = folder::state(&target).map_err(|e| unwritable(&target, &e))?;
    if matches!(state, State::Foreign | State::Linked) {
        return Err(Failure::new(
            "skill_foreign",
            format!("{} holds a skill Elsecall did not write.", target.display()),
            "Choose another --dir, or remove that folder if it is no longer needed.",
        ));
    }
    folder::replace(&target, &folder::exported()).map_err(|e| unwritable(&target, &e))?;
    let note = "This copy does not update itself: run elsecall skill install again after upgrading Elsecall.";
    Ok(Printed::Json(json!({ "path": target, "note": note }), true))
}

/// Where `skill install` writes by default: inside Elsecall's own data folder,
/// which is created private, as installing Elsecall requires it to be.
pub fn default_copy(paths: &Paths) -> Result<PathBuf> {
    elsecall_local_state::private_files::owned_tree(&paths.home, &paths.data)?;
    elsecall_local_state::private_files::directory(&paths.data)?;
    Ok(paths.data.join("skill"))
}

fn unwritable(folder: &Path, e: &std::io::Error) -> Failure {
    Failure::new(
        "skill_write_failed",
        format!(
            "The skill in {} could not be written: {e}.",
            folder.display()
        ),
        "Check the folder's permissions, then run the command again.",
    )
}

fn entry(home: &Path, target: &Target) -> Value {
    let mut item = json!({
        "agent": target.agent,
        "label": target.agent.label(),
        "found": target.agent.found(home),
        "skill": { "path": target.folder },
    });
    match &target.before {
        Ok(state) => item["skill"]["state"] = json!(state),
        Err(error) => {
            item["skill"]["state"] = json!("unreadable");
            item["skill"]["error"] = json!(error);
        }
    }
    item
}

/// Each agent's entry and what was done to it.
fn described(home: &Path, targets: &[Target], outcomes: &[Outcome]) -> Vec<Value> {
    targets
        .iter()
        .zip(outcomes)
        .map(|(target, outcome)| {
            let mut item = entry(home, target);
            item["result"] = json!(outcome.done);
            if let Some(error) = &outcome.error {
                item["error"] = json!(error);
            }
            item
        })
        .collect()
}

/// What `elsecall doctor` reports: each agent found, its skill, and a warning
/// naming the fix when one lacks a current skill. Writes nothing.
pub fn health(home: &Path) -> Value {
    let found: Vec<Agent> = agents::ALL.into_iter().filter(|a| a.found(home)).collect();
    let targets = targets::inspect(home, &found);
    let lacking = |wanted: fn(&std::result::Result<State, String>) -> bool| -> Vec<&str> {
        targets
            .iter()
            .filter(|t| wanted(&t.before))
            .map(|t| t.agent.label())
            .collect()
    };
    let mut warnings = Vec::new();
    let setup = lacking(|s| matches!(s, Ok(State::Missing | State::Exported)));
    if !setup.is_empty() {
        warnings.push(format!(
            "{} {} no Elsecall skill: run elsecall skill setup.",
            setup.join(", "),
            if setup.len() == 1 { "has" } else { "have" }
        ));
    }
    let foreign = lacking(|s| matches!(s, Ok(State::Foreign)));
    if !foreign.is_empty() {
        warnings.push(format!(
            "{}: a skill named elsecall that Elsecall did not write is in the way; run elsecall skill setup --force to replace it.",
            foreign.join(", ")
        ));
    }
    let linked = lacking(|s| matches!(s, Ok(State::Linked)));
    if !linked.is_empty() {
        warnings.push(format!(
            "{}: the elsecall skill folder is a link; remove it, then run elsecall skill setup.",
            linked.join(", ")
        ));
    }
    let unreadable = lacking(|s| s.is_err());
    if !unreadable.is_empty() {
        warnings.push(format!(
            "{}: the skill folder could not be read; check its permissions.",
            unreadable.join(", ")
        ));
    }
    json!({
        "agents": targets.iter().map(|t| entry(home, t)).collect::<Vec<_>>(),
        "warning": (!warnings.is_empty()).then(|| warnings.join(" ")),
    })
}
