//! Messages of a placed run, after the workspace sync.
//!
//! The caller sends `Start`; the member answers `Started` or `StartFailed`. Then
//! standard input flows to the member and output back, each direction limited by a
//! window of unacknowledged bytes, so a slow reader slows the writer as a pipe
//! would. Signals go to the member. When the command has exited and its output is
//! drained, the member sends `Exited`, then the outputs from its snapshot at exit;
//! the caller says what did not come back (`Returned`) and ends the run with `Done`.
use crate::{
    failure::Failure,
    remote_run::consistency::{self, Fingerprints, Mismatches},
    wire,
    workspace::{
        not_returned::{Cause, NotReturned},
        outputs::MAXIMUM_DECLARED,
        path::WorkspacePath,
        sync::SyncMessage,
    },
};
use serde::{Deserialize, Serialize};

/// How many output or input bytes may be in flight unacknowledged per stream.
pub const WINDOW: u64 = 1024 * 1024;
const MAXIMUM_ARGUMENTS: usize = 65_536;
/// The command line and environment together, as macOS's ARG_MAX bounds them.
const MAXIMUM_COMMAND_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
/// Decode incoming JSON through `link::decode`, which isolates raw terminal metadata.
pub enum RunMessage {
    Sync(SyncMessage),
    Start(Start),
    /// The command started. With a consistency request, `fingerprints` are the
    /// member's own, under the caller's salt.
    Started {
        fingerprints: Option<Fingerprints>,
    },
    /// What the caller found to differ, for the member's record.
    Mismatched {
        mismatches: Mismatches,
    },
    StartFailed {
        failure: Failure,
    },
    /// A signal for the command's process group.
    Signal {
        signal: i32,
    },
    /// The caller's standard input ended.
    InputEnded,
    /// The command no longer reads standard input.
    InputClosed,
    Ack {
        stream: Stream,
        bytes: u64,
    },
    /// The caller's destination for this stream is gone, as when a pipe closed.
    OutputClosed {
        stream: Stream,
    },
    Exited {
        exit: Exited,
        /// CPU time the command and the descendants it waited for used.
        #[serde(default)]
        cpu_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan_observation: Option<Box<serde_json::value::RawValue>>,
    },
    /// After the exit: followed by one blob, the JSON list of the declared outputs
    /// the run changed, as the snapshot at exit holds them. `described` is false
    /// when no snapshot could be taken. `not_returned` names what the snapshot
    /// left out, and why, except links, which the list itself names.
    Outputs {
        described: bool,
        #[serde(default)]
        not_returned: Vec<NotReturned>,
    },
    /// Followed by one blob, the JSON list of digests the caller wants; the member
    /// then sends one blob each, or `OutputNotSent` in its place.
    WantOutputs,
    /// Stands for the rest of a wanted blob the member could not send; the next
    /// wanted blob follows.
    OutputNotSent {
        cause: Cause,
        detail: Option<String>,
    },
    /// What the caller did not bring back, and why, for the member's record;
    /// `nothing` is why it brought nothing back at all, when it gave up.
    Returned {
        not_returned: Vec<NotReturned>,
        #[serde(default)]
        nothing: Option<String>,
    },
    /// The member failed the run, such as when the sync did not apply.
    Failed {
        failure: Failure,
    },
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    Stdin,
    Stdout,
    Stderr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Exited {
    Code { code: i32 },
    Signal { signal: i32 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Variable {
    #[serde(with = "wire::bytes")]
    pub name: Vec<u8>,
    #[serde(with = "wire::bytes")]
    pub value: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    #[serde(with = "wire::bytes")]
    pub program: Vec<u8>,
    #[serde(with = "wire::bytes::list")]
    pub arguments: Vec<Vec<u8>>,
    /// The caller's variables its consumer configuration forwards.
    pub environment: Vec<Variable>,
    /// The caller's working directory inside the workspace.
    pub directory: Option<WorkspacePath>,
    /// Whether the caller's stdout and stderr are one destination, so the command
    /// gets one pipe for both and their order survives.
    pub merged: bool,
    /// The caller has standard input to forward.
    pub input: bool,
    /// Paths to bring back once the command exits.
    pub outputs: Vec<WorkspacePath>,
    /// Toolchains and configuration locations to fingerprint, under a salt.
    pub consistency: Option<consistency::Request>,
}

impl Start {
    /// Checks an untrusted start request.
    pub fn validate(&self) -> Result<(), &'static str> {
        let nul = |bytes: &[u8]| bytes.contains(&0);
        let total = self.program.len()
            + self.arguments.iter().map(Vec::len).sum::<usize>()
            + self
                .environment
                .iter()
                .map(|v| v.name.len() + v.value.len() + 2)
                .sum::<usize>();
        if self.program.is_empty() || nul(&self.program) {
            return Err("The command is empty or contains NUL.");
        }
        if self.arguments.len() > MAXIMUM_ARGUMENTS || self.arguments.iter().any(|a| nul(a)) {
            return Err("The command's arguments are invalid.");
        }
        if total > MAXIMUM_COMMAND_BYTES {
            return Err("The command line and environment are too large.");
        }
        let valid_name = |name: &[u8]| !name.is_empty() && !name.contains(&b'=') && !nul(name);
        if self
            .environment
            .iter()
            .any(|v| !valid_name(&v.name) || nul(&v.value))
        {
            return Err("A forwarded variable is invalid.");
        }
        if self.outputs.len() > MAXIMUM_DECLARED {
            return Err("Too many outputs.");
        }
        Ok(())
    }
}

/// Liveness failure observed by the protocol consumer, including unexpected EOF.
/// The serving scheduler supplies member identity; callers cannot name a peer.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamFailure {
    pub phase: crate::peer_diagnostics::Phase,
    pub kind: crate::peer_diagnostics::Kind,
    pub duration_ms: u64,
}

/// Metadata is less authoritative than a confirmed process exit. Reject a bad
/// assertion independently, so it cannot erase valid exit/CPU/phase facts.
pub fn received_observation(
    value: Option<Box<serde_json::value::RawValue>>,
    workload: Option<&crate::workload_metadata::Workload>,
) -> (Option<crate::workload_metadata::Observation>, bool) {
    use crate::workload_metadata::{MAXIMUM_JSON_BYTES, Observation};
    let Some(value) = value else {
        return (None, false);
    };
    let accepted = workload.and_then(|workload| {
        if value.get().len() > MAXIMUM_JSON_BYTES {
            return None;
        }
        let observation: Observation = serde_json::from_str(value.get()).ok()?;
        observation.validate(Some(workload)).ok()?;
        Some(observation)
    });
    match accepted {
        Some(observation) => (Some(observation), false),
        None => (workload.map(|_| Observation::Unknown {}), true),
    }
}

/// The canonical JSON run-message boundary. A direct terminal decode retains
/// raw optional metadata without the internally tagged enum's Content buffer.
pub(super) fn decode(bytes: &[u8]) -> serde_json::Result<RunMessage> {
    #[derive(Deserialize)]
    struct Header {
        #[serde(rename = "type")]
        kind: String,
    }
    #[derive(Deserialize)]
    struct Terminal {
        exit: Exited,
        #[serde(default)]
        cpu_ms: Option<u64>,
        #[serde(default)]
        plan_observation: Option<Box<serde_json::value::RawValue>>,
    }
    let header: Header = serde_json::from_slice(bytes)?;
    if header.kind == "exited" {
        let terminal: Terminal = serde_json::from_slice(bytes)?;
        Ok(RunMessage::Exited {
            exit: terminal.exit,
            cpu_ms: terminal.cpu_ms,
            plan_observation: terminal.plan_observation,
        })
    } else {
        serde_json::from_slice(bytes)
    }
}

#[cfg(test)]
mod terminal_boundary_tests {
    #[test]
    fn duplicate_terminal_fields_are_rejected_by_the_canonical_decoder() {
        for bytes in [
            r#"{"type":"exited","type":"started","exit":{"kind":"code","code":0}}"#,
            r#"{"type":"exited","exit":{"kind":"code","code":0},"exit":{"kind":"code","code":1}}"#,
            r#"{"type":"exited","exit":{"kind":"code","code":0},"plan_observation":null,"plan_observation":{"status":"unknown"}}"#,
        ] {
            assert!(crate::remote_run::link::decode(bytes.as_bytes()).is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_including_nested_sync() {
        for message in [
            RunMessage::Sync(SyncMessage::Abort {
                message: "x".into(),
            }),
            RunMessage::Signal { signal: 2 },
            RunMessage::Exited {
                plan_observation: None,
                cpu_ms: Some(12),
                exit: Exited::Code { code: 125 },
            },
            RunMessage::Ack {
                stream: Stream::Stdout,
                bytes: 10,
            },
        ] {
            let json = serde_json::to_vec(&message).unwrap();
            let back: RunMessage = serde_json::from_slice(&json).unwrap();
            assert_eq!(format!("{back:?}"), format!("{message:?}"));
        }
    }

    #[test]
    fn validates_start_requests() {
        let start = Start {
            program: b"/bin/echo".to_vec(),
            arguments: vec![b"\xff".to_vec()],
            environment: vec![Variable {
                name: b"CI".to_vec(),
                value: b"1".to_vec(),
            }],
            directory: None,
            merged: false,
            input: false,
            outputs: Vec::new(),
            consistency: None,
        };
        assert!(start.validate().is_ok());
        let mut bad = start.clone();
        bad.environment[0].name = b"A=B".to_vec();
        assert!(bad.validate().is_err());
        let mut bad = start.clone();
        bad.arguments.push(b"a\0b".to_vec());
        assert!(bad.validate().is_err());
        let mut bad = start;
        bad.program.clear();
        assert!(bad.validate().is_err());
    }
}
