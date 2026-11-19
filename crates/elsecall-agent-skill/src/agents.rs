//! The coding agents `elsecall skill setup` knows, and where each keeps skills.
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    Claude,
    Codex,
    Opencode,
}

pub(crate) const ALL: [Agent; 3] = [Agent::Claude, Agent::Codex, Agent::Opencode];

impl Agent {
    pub fn label(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
            Agent::Opencode => "OpenCode",
        }
    }

    /// Whether the agent has been used by this account: its own settings folder.
    pub fn found(self, home: &Path) -> bool {
        home.join(match self {
            Agent::Claude => ".claude",
            Agent::Codex => ".codex",
            Agent::Opencode => ".config/opencode",
        })
        .is_dir()
    }

    /// The folder the agent reads skills from. Codex and OpenCode share one.
    pub fn skills(self, home: &Path) -> PathBuf {
        match self {
            Agent::Claude => home.join(".claude/skills"),
            Agent::Codex | Agent::Opencode => home.join(".agents/skills"),
        }
    }
}
