//! Agent-ready lines on stderr: one plain line that says what happened and
//! exactly what to do, so consumers never format their own. Every line has the
//! form `elsecall: <code>: <what happened> <what to do>` and ends with a newline.
//!
//! - Elsecall's own failures of `run` and `lease acquire` write one, with the
//!   failure's code, before the JSON error envelope.
//! - Work that proceeds without admission because the service is unavailable
//!   writes one, with the code `unmanaged`, before it proceeds.
use crate::failure::Failure;
use std::io::Write;

/// The retry delay `fleet_busy` suggests when its details lack one.
const DEFAULT_RETRY_SECONDS: u64 = 60;

/// What proceeds without admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unmanaged {
    /// `run`: the command runs directly.
    Run,
    /// `lease acquire`: no lease is held.
    Lease,
}

/// One line: no control characters, so it never splits or spoofs another line.
fn plain(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn explain(failure: &Failure) -> String {
    match &failure.run_id {
        Some(id) => format!(" `elsecall explain {id}` shows the record."),
        None => String::new(),
    }
}

/// The line for one of Elsecall's own failures.
pub fn failure_line(failure: &Failure) -> String {
    let next = match failure.code.as_str() {
        "fleet_busy" => {
            let retry = failure
                .details
                .as_ref()
                .and_then(|details| details["retry_after_seconds"].as_u64())
                .unwrap_or(DEFAULT_RETRY_SECONDS);
            let id = failure.run_id.as_deref().unwrap_or("<run_id>");
            format!(
                "Wait about {retry} s, then rerun the same command once. `elsecall runs` shows what is running and queued; `elsecall explain {id}` shows why."
            )
        }
        "service_unavailable" => format!(
            "{} Then rerun the same command. To run without admission while the service is down, set ELSECALL_WHEN_UNAVAILABLE=run.",
            failure.hint
        ),
        "request_too_large" => format!(
            "Lower --size, or the size of the label's rule in .elsecall.toml, to at most this member's capacity, then rerun the same command.{}",
            explain(failure)
        ),
        "deadline_skipped" => format!(
            "Nothing ran, and nothing needs doing now: request it again when it is next due, or use --at-deadline start.{}",
            explain(failure)
        ),
        "connection_lost" => format!(
            "{} Check the command's effects before you rerun it.{}",
            failure.hint,
            explain(failure)
        ),
        _ => format!("{}{}", failure.hint, explain(failure)),
    };
    plain(&format!(
        "elsecall: {}: {} {next}",
        failure.code, failure.message
    ))
}

/// The notice for work that proceeds without admission because of `reason`.
pub fn unmanaged_line(reason: &Failure, unmanaged: Unmanaged) -> String {
    let what = match unmanaged {
        Unmanaged::Run => "The command runs without admission.",
        Unmanaged::Lease => "No lease is held, and the work runs without admission.",
    };
    plain(&format!(
        "elsecall: unmanaged: {} {what} Run `elsecall doctor` to restore admission.",
        reason.message
    ))
}

/// Writes a line to stderr. A closed stderr is no reason to fail the work.
pub fn write(line: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{line}");
    let _ = stderr.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fleet_busy_says_when_to_retry_and_where_to_look() {
        let failure = Failure::new(
            "fleet_busy",
            "No capacity within the 90 s hold limit: 0 of 12 cores are free and 2 other requests wait.",
            "Retry in about 40 s, request a smaller --size, or run it as --deferrable with a --deadline.",
        )
        .for_run("r-3f9a0c41d2b7")
        .with_details(serde_json::json!({"retry_after_seconds": 40, "running": []}));
        assert_eq!(
            failure_line(&failure),
            "elsecall: fleet_busy: No capacity within the 90 s hold limit: 0 of 12 cores are free and 2 other requests wait. \
             Wait about 40 s, then rerun the same command once. `elsecall runs` shows what is running and queued; \
             `elsecall explain r-3f9a0c41d2b7` shows why."
        );
    }

    #[test]
    fn other_failures_end_with_their_next_step() {
        let unavailable = failure_line(&Failure::unavailable());
        assert_eq!(
            unavailable,
            "elsecall: service_unavailable: The local service could not be reached. Run elsecall doctor; use elsecall install if absent, or elsecall service restart if installed. \
             Then rerun the same command. To run without admission while the service is down, set ELSECALL_WHEN_UNAVAILABLE=run."
        );
        let large = failure_line(
            &Failure::new(
                "request_too_large",
                "Size 16 is larger than this member's capacity of 12 cores.",
                "Request a smaller --size.",
            )
            .for_run("r-000000000001"),
        );
        assert!(
            large.starts_with("elsecall: request_too_large: Size 16 is larger")
                && large.ends_with("`elsecall explain r-000000000001` shows the record."),
            "{large}"
        );
        let invalid = failure_line(&Failure::new(
            "invalid_request",
            "/repo/.elsecall.toml is invalid: line 3: bad\nthing",
            "Fix it, then rerun the same command.",
        ));
        assert_eq!(
            invalid,
            "elsecall: invalid_request: /repo/.elsecall.toml is invalid: line 3: bad thing Fix it, then rerun the same command."
        );
    }

    #[test]
    fn unmanaged_work_gets_one_notice() {
        assert_eq!(
            unmanaged_line(&Failure::unavailable(), Unmanaged::Run),
            "elsecall: unmanaged: The local service could not be reached. The command runs without admission. Run `elsecall doctor` to restore admission."
        );
        assert!(
            unmanaged_line(&Failure::unavailable(), Unmanaged::Lease).contains("No lease is held")
        );
    }
}
