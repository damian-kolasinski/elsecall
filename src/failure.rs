use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, Failure>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Failure {
    pub code: String,
    pub message: String,
    pub hint: String,
    /// The run or lease the failure belongs to, when a request record exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Structured context for the code, such as a suggested retry delay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Box<serde_json::Value>>,
}

impl Failure {
    pub fn new(code: &str, message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            hint: hint.into(),
            run_id: None,
            details: None,
        }
    }

    pub fn for_run(mut self, run_id: impl ToString) -> Self {
        self.run_id = Some(run_id.to_string());
        self
    }

    pub fn with_details(mut self, details: impl serde::Serialize) -> Self {
        self.details = Some(Box::new(
            serde_json::to_value(details).expect("details serialize"),
        ));
        self
    }

    pub fn io(action: &str, error: std::io::Error) -> Self {
        Self::new(
            "io_error",
            format!("{action}: {error}"),
            "Run elsecall doctor and check directory ownership and permissions.",
        )
    }

    pub fn unavailable() -> Self {
        Self::new(
            "service_unavailable",
            "The local service could not be reached.",
            "Run elsecall doctor; use elsecall install if absent, or elsecall service restart if installed.",
        )
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for Failure {}
