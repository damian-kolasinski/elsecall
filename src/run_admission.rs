//! How a command that `elsecall run` started came to run. The status file reports
//! it as `admission`, and the command itself sees it as `ELSECALL_ADMISSION`, set in
//! every mode. A script that re-execs itself through Elsecall reads the variable to
//! do so exactly once, whether or not Elsecall manages the run. Admission never reads
//! it: nesting follows processes, not variables. The workload observation CLI
//! uses it only to refuse reports in bypass/unmanaged contexts; the service
//! always authorizes reports through the process tree and active grant.
use serde::Serialize;

/// The variable that carries the admission into the command's environment.
pub const VARIABLE: &str = "ELSECALL_ADMISSION";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Admission {
    /// Admitted with capacity of its own, here or on the member it was placed on.
    Granted,
    /// Requested from inside other work, so it shares that work's grant.
    Nested,
    /// The service was unavailable and the caller chose to run anyway.
    Unmanaged,
    /// `ELSECALL=off`: run directly, with Elsecall stepping aside.
    Off,
}

impl Admission {
    pub fn as_str(self) -> &'static str {
        match self {
            Admission::Granted => "granted",
            Admission::Nested => "nested",
            Admission::Unmanaged => "unmanaged",
            Admission::Off => "off",
        }
    }

    /// The `ELSECALL_ADMISSION` assignment the command runs with.
    pub fn variable(self) -> (&'static str, String) {
        (VARIABLE, self.as_str().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::Admission;

    #[test]
    fn the_variable_says_what_the_status_file_says() {
        for admission in [
            Admission::Granted,
            Admission::Nested,
            Admission::Unmanaged,
            Admission::Off,
        ] {
            assert_eq!(
                serde_json::to_value(admission).unwrap(),
                admission.as_str(),
                "{admission:?}"
            );
            assert_eq!(
                admission.variable(),
                ("ELSECALL_ADMISSION", admission.as_str().to_owned())
            );
        }
    }
}
