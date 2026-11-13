//! What a member's record keeps of the processes a placed command left running
//! after it exited ("leftovers"). They are not admitted work: the run's capacity
//! is released at its end, and they run on as outside load until they end on their
//! own or are stopped.
use crate::scheduling::{processes::ProcessIdentity, timestamp::Timestamp};
use serde::{Deserialize, Serialize};

/// The leftovers of one placed run, while they run and once they ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Leftovers {
    Running {
        /// When the command exited, which is when they became leftovers.
        since: Timestamp,
        /// How long they may run after the exit before they are stopped.
        cap_ms: u64,
        /// The command's process group, which they are in.
        group: u32,
        /// Some of them, so a restarted service can find the group and stop it:
        /// a PID is never reused while its process group exists.
        processes: Vec<ProcessIdentity>,
    },
    Ended {
        since: Timestamp,
        cap_ms: u64,
        /// From the command's exit until the last of them ended; absent when a
        /// restart found none of them left, so when they ended is unknown.
        duration_ms: Option<u64>,
        /// CPU time they used after the exit, sampled every half second, with
        /// that of the children they waited for. An approximation: what a
        /// process used since the last sample before it ended is missed unless
        /// another of them waited for it.
        cpu_ms: Option<u64>,
        ending: Ending,
    },
}

/// Why leftovers ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum Ending {
    /// The last of them exited on its own.
    Finished,
    /// They were stopped when they reached the cap.
    Capped,
    /// They were stopped so the next run of the same copy could sync it.
    NextRun { run_id: String },
    /// The member's service restarted while they ran and stopped them.
    ServiceRestarted,
}

impl Leftovers {
    pub fn is_running(&self) -> bool {
        matches!(self, Leftovers::Running { .. })
    }

    /// The cap stopped them.
    pub fn capped(&self) -> bool {
        matches!(
            self,
            Leftovers::Ended {
                ending: Ending::Capped,
                ..
            }
        )
    }

    /// The record of leftovers a restarted service found still running, or
    /// gone, at `now`. `stopped` says whether any of them was still alive.
    pub fn restarted(&self, now: Timestamp, stopped: bool) -> Self {
        match self {
            Leftovers::Running { since, cap_ms, .. } => Leftovers::Ended {
                since: *since,
                cap_ms: *cap_ms,
                duration_ms: stopped.then(|| now.since(*since).as_millis() as u64),
                cpu_ms: None,
                ending: Ending::ServiceRestarted,
            },
            ended => ended.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_say_how_leftovers_ran_and_ended() {
        let running = Leftovers::Running {
            since: Timestamp::from_millis(1_000),
            cap_ms: 600_000,
            group: 42,
            processes: vec![ProcessIdentity {
                pid: 43,
                started_us: 7,
            }],
        };
        let json = serde_json::to_value(&running).unwrap();
        assert_eq!(json["state"], "running");
        assert_eq!(json["group"], 42);
        assert!(running.is_running() && !running.capped());

        let stopped = running.restarted(Timestamp::from_millis(4_000), true);
        assert_eq!(
            stopped,
            Leftovers::Ended {
                since: Timestamp::from_millis(1_000),
                cap_ms: 600_000,
                duration_ms: Some(3_000),
                cpu_ms: None,
                ending: Ending::ServiceRestarted,
            }
        );
        let gone = running.restarted(Timestamp::from_millis(4_000), false);
        assert!(matches!(
            gone,
            Leftovers::Ended {
                duration_ms: None,
                ..
            }
        ));
        assert_eq!(gone.restarted(Timestamp::from_millis(9_000), true), gone);

        let next = Leftovers::Ended {
            since: Timestamp::from_millis(1_000),
            cap_ms: 600_000,
            duration_ms: Some(2_000),
            cpu_ms: Some(1_500),
            ending: Ending::NextRun {
                run_id: "r-000000000001".into(),
            },
        };
        let json = serde_json::to_value(&next).unwrap();
        assert_eq!(json["ending"]["by"], "next_run");
        assert_eq!(json["ending"]["run_id"], "r-000000000001");
        assert_eq!(serde_json::from_value::<Leftovers>(json).unwrap(), next);
        let capped = Leftovers::Ended {
            since: Timestamp::from_millis(1_000),
            cap_ms: 600_000,
            duration_ms: Some(600_000),
            cpu_ms: Some(1),
            ending: Ending::Capped,
        };
        assert!(capped.capped() && !next.capped());
    }
}
