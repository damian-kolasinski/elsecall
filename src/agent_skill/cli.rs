//! `elsecall skill`: print the guide, and install it for the agents on this machine.
use super::{
    agents::{self, Agent},
    folder::{self, State},
    guide,
    targets::{self, Failed, Outcome, Target},
};
use crate::{
    failure::{Failure, Result},
    paths::Paths,
};
use clap::{Args, Subcommand};
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
        /// Replace a skill named elsecall that Elsecall didn't write
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
    },
    /// Write the skill folder for an agent setup doesn't know, and print where it is
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

pub fn run(args: SkillArgs) -> Result<Printed> {
    match args.command {
        SkillCommand::Show { name, json } => show(name.as_deref(), json),
        SkillCommand::Setup {
            agents,
            dry_run,
            force,
        } => setup(&Paths::current()?.home, &agents, dry_run, force),
        SkillCommand::Status => {
            let home = Paths::current()?.home;
            let targets = targets::inspect(&home, &agents::ALL).map_err(unreadable)?;
            let agents: Vec<Value> = targets.iter().map(|t| entry(&home, t)).collect();
            Ok(Printed::Json(json!({ "agents": agents }), true))
        }
        SkillCommand::Remove { agents } => {
            let home = Paths::current()?.home;
            let chosen = if agents.is_empty() {
                agents::ALL.to_vec()
            } else {
                agents
            };
            let targets = targets::inspect(&home, &chosen).map_err(unreadable)?;
            let outcomes =
                targets::remove(&targets).map_err(|failed| unwritable(failed, "removed"))?;
            let agents = described(&home, &targets, &outcomes);
            Ok(Printed::Json(json!({ "agents": agents }), true))
        }
        SkillCommand::Install { dir } => {
            let root = match dir {
                Some(dir) => {
                    std::path::absolute(&dir).map_err(|e| unwritable((dir, e), "written"))?
                }
                None => Paths::current()?.data.join("skill"),
            };
            let target = root.join(folder::NAME);
            match folder::state(&target).map_err(|e| unreadable((target.clone(), e)))? {
                State::Foreign | State::Linked => return Err(foreign(&target)),
                State::Current => {}
                State::Missing | State::Outdated => folder::write(&target)
                    .map_err(|e| unwritable((target.clone(), e), "written"))?,
            }
            Ok(Printed::Json(json!({ "path": target }), true))
        }
    }
}

fn show(name: Option<&str>, json: bool) -> Result<Printed> {
    let text = match name {
        None => guide::GUIDE,
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

fn setup(home: &Path, named: &[Agent], dry_run: bool, force: bool) -> Result<Printed> {
    let chosen: Vec<Agent> = if named.is_empty() {
        agents::ALL.into_iter().filter(|a| a.found(home)).collect()
    } else {
        named.to_vec()
    };
    if chosen.is_empty() {
        return Err(Failure::new(
            "no_agents_found",
            "No coding agent Elsecall knows was found for this account.",
            "Name one with --agent claude, codex or opencode, or run elsecall skill install and point your agent at the folder it prints.",
        ));
    }
    let targets = targets::inspect(home, &chosen).map_err(unreadable)?;
    let outcomes =
        targets::setup(&targets, dry_run, force).map_err(|failed| unwritable(failed, "written"))?;
    let skipped = |o: &Outcome| matches!(o, Outcome::SkippedForeign | Outcome::SkippedLinked);
    let complete = !outcomes.iter().any(skipped);
    let mut data = json!({ "dry_run": dry_run, "agents": described(home, &targets, &outcomes) });
    if outcomes.contains(&Outcome::SkippedForeign) {
        data["hint"] = json!(
            "A skill named elsecall that Elsecall did not write was left alone. Run elsecall skill setup --force to replace its SKILL.md."
        );
    } else if !complete {
        data["hint"] = json!(
            "A linked skill folder was left alone. Remove the link, then run elsecall skill setup again."
        );
    }
    Ok(Printed::Json(data, complete))
}

fn entry(home: &Path, target: &Target) -> Value {
    json!({
        "agent": target.agent,
        "label": target.agent.label(),
        "found": target.agent.found(home),
        "skill": { "path": target.folder, "state": target.before },
    })
}

/// Each agent's entry, what was done to it, and why it was skipped if it was.
fn described(home: &Path, targets: &[Target], outcomes: &[Outcome]) -> Vec<Value> {
    targets
        .iter()
        .zip(outcomes)
        .map(|(target, outcome)| {
            let mut item = entry(home, target);
            item["changed"] = json!(*outcome == Outcome::Changed);
            match outcome {
                Outcome::SkippedForeign => item["skipped"] = json!("foreign"),
                Outcome::SkippedLinked => item["skipped"] = json!("linked"),
                Outcome::Unchanged | Outcome::Changed => {}
            }
            item
        })
        .collect()
}

fn unreadable((folder, e): Failed) -> Failure {
    Failure::new(
        "skill_unreadable",
        format!(
            "The skill folder {} could not be read: {e}.",
            folder.display()
        ),
        "Check the folder's permissions, then run the command again.",
    )
}

fn unwritable((folder, e): Failed, what: &str) -> Failure {
    Failure::new(
        "skill_write_failed",
        format!(
            "The skill in {} could not be {what}: {e}.",
            folder.display()
        ),
        "Check the folder's permissions, then run the command again.",
    )
}

fn foreign(target: &Path) -> Failure {
    Failure::new(
        "skill_foreign",
        format!("{} holds a skill Elsecall did not write.", target.display()),
        "Choose another --dir, or remove that folder if it is no longer needed.",
    )
}
