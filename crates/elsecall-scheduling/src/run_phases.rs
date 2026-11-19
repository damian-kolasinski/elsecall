//! Caller-observed run boundaries, separate from the time a grant was held.
use serde::{Deserialize, Serialize};

/// Discard implausible caller-supplied samples without changing the outcome.
const MAXIMUM_ELAPSED_MS: u64 = 365 * 24 * 60 * 60 * 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteFailurePhase {
    WorkspaceSync,
    Execution,
    ResultReturn,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunPhases {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_return_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_to_result_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_failure_phase: Option<RemoteFailurePhase>,
}

impl RunPhases {
    pub fn failed(phase: RemoteFailurePhase) -> Self {
        Self {
            remote_failure_phase: Some(phase),
            ..Self::default()
        }
    }

    /// Accept only combinations a caller can observe when reporting a command exit.
    pub fn valid_completion(self, remote: bool) -> bool {
        let Some(execution) = self.execution_ms.filter(|ms| *ms <= MAXIMUM_ELAPSED_MS) else {
            return false;
        };
        if self
            .request_to_result_ms
            .is_some_and(|ms| ms > MAXIMUM_ELAPSED_MS)
        {
            return false;
        }
        match (
            remote,
            self.result_return_ms,
            self.request_to_result_ms,
            self.remote_failure_phase,
        ) {
            (_, None, Some(total), None)
            | (true, None, Some(total), Some(RemoteFailurePhase::ResultReturn)) => {
                execution <= total
            }
            (true, Some(returned), Some(total), None) => total
                .checked_sub(execution)
                .is_some_and(|left| returned <= left),
            (true, None, None, Some(RemoteFailurePhase::ResultReturn)) => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_validates_ordering_and_rejects_implausible_measurements() {
        let returned = RunPhases {
            execution_ms: Some(20),
            result_return_ms: Some(5),
            request_to_result_ms: Some(30),
            remote_failure_phase: None,
        };
        assert!(returned.valid_completion(true));
        assert!(!returned.valid_completion(false));
        assert!(
            !RunPhases {
                request_to_result_ms: Some(24),
                ..returned
            }
            .valid_completion(true)
        );
        assert!(
            !RunPhases {
                request_to_result_ms: Some(u64::MAX),
                ..returned
            }
            .valid_completion(true)
        );

        let failed_return = RunPhases {
            execution_ms: Some(20),
            ..RunPhases::failed(RemoteFailurePhase::ResultReturn)
        };
        assert!(failed_return.valid_completion(true));
        assert!(
            !RunPhases {
                execution_ms: Some(u64::MAX),
                ..failed_return
            }
            .valid_completion(true)
        );
    }
}
