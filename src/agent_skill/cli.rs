//! `elsecall skill`: print the guide, and install it for the agents on this machine.
use super::{
    agents::{self, Agent},
    folder::{self, State},
    guide,
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
            let agents = agents::ALL
                .iter()
                .map(|&agent| Ok(entry(&home, agent, inspect(&folder_of(&home, agent))?)))
                .collect::<Result<Vec<_>>>()?;
            Ok(Printed::Json(json!({ "agents": agents }), true))
        }
        SkillCommand::Remove { agents } => remove(&Paths::current()?.home, &agents),
        SkillCommand::Install { dir } => {
            let root = match dir {
                Some(dir) => dir,
                None => Paths::current()?.data.join("skill"),
            };
            let target = root.join(folder::NAME);
            match inspect(&target)? {
                State::Foreign => return Err(foreign(&target)),
                State::Current => {}
                State::Missing | State::Outdated => written(folder::write(&target), &target)?,
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
    let mut complete = true;
    let mut done: Vec<PathBuf> = Vec::new();
    let mut entries = Vec::new();
    // Every state before any change: Codex and OpenCode share a folder.
    for (agent, before) in inspected(home, &chosen)? {
        let target = folder_of(home, agent);
        let wanted = match before {
            State::Current => false,
            State::Missing | State::Outdated => true,
            State::Foreign => {
                complete &= force;
                force
            }
        };
        // Codex and OpenCode share a folder: write it once.
        if wanted && !dry_run && !done.contains(&target) {
            written(folder::write(&target), &target)?;
            done.push(target.clone());
        }
        let mut item = entry(home, agent, before);
        item["changed"] = json!(wanted);
        entries.push(item);
    }
    let data = json!({ "dry_run": dry_run, "agents": entries });
    Ok(Printed::Json(data, complete))
}

fn remove(home: &Path, named: &[Agent]) -> Result<Printed> {
    let chosen = if named.is_empty() {
        agents::ALL.to_vec()
    } else {
        named.to_vec()
    };
    let mut entries = Vec::new();
    let mut done: Vec<PathBuf> = Vec::new();
    for (agent, before) in inspected(home, &chosen)? {
        let target = folder_of(home, agent);
        let wanted = before.ours();
        if wanted && !done.contains(&target) {
            folder::remove(&target).map_err(|e| {
                Failure::new(
                    "skill_write_failed",
                    format!(
                        "The skill at {} could not be removed: {e}.",
                        target.display()
                    ),
                    "Check the folder's permissions, then run elsecall skill remove again.",
                )
            })?;
            done.push(target.clone());
        }
        let mut item = entry(home, agent, before);
        item["changed"] = json!(wanted);
        entries.push(item);
    }
    Ok(Printed::Json(json!({ "agents": entries }), true))
}

fn inspected(home: &Path, chosen: &[Agent]) -> Result<Vec<(Agent, State)>> {
    chosen
        .iter()
        .map(|&agent| Ok((agent, inspect(&folder_of(home, agent))?)))
        .collect()
}

fn folder_of(home: &Path, agent: Agent) -> PathBuf {
    agent.skills(home).join(folder::NAME)
}

fn entry(home: &Path, agent: Agent, state: State) -> Value {
    json!({
        "agent": agent,
        "label": agent.label(),
        "found": agent.found(home),
        "skill": { "path": folder_of(home, agent), "state": state },
    })
}

fn inspect(target: &Path) -> Result<State> {
    folder::state(target).map_err(|e| {
        Failure::new(
            "skill_unreadable",
            format!(
                "The skill folder {} could not be read: {e}.",
                target.display()
            ),
            "Check the folder's permissions, then run the command again.",
        )
    })
}

fn written(result: std::io::Result<()>, target: &Path) -> Result<()> {
    result.map_err(|e| {
        Failure::new(
            "skill_write_failed",
            format!(
                "The skill could not be written to {}: {e}.",
                target.display()
            ),
            "Check the folder's permissions, then run the command again.",
        )
    })
}

fn foreign(target: &Path) -> Failure {
    Failure::new(
        "skill_foreign",
        format!("{} holds a skill Elsecall did not write.", target.display()),
        "Choose another --dir, or remove that folder if it is no longer needed.",
    )
}
