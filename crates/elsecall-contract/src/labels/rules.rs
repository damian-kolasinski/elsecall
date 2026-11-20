//! What a label may be: how long, and the prefix that marks rehearsal's
//! synthetic work. Admission, reporting, replay and rehearsal all read labels by
//! these rules, so they live below all of them.

/// The longest label a request may carry, in characters.
pub const MAXIMUM_LABEL: usize = 200;
/// What a synthetic rehearsal session adds to a label beyond its prefix.
pub const SESSION_LABEL_EXTRA: usize = 25;
/// The prefix of every label rehearsal gives its synthetic work.
pub const PREFIX: &str = "rehearsal:";

/// Whether a label marks rehearsal's synthetic work.
pub fn synthetic(label: Option<&str>) -> bool {
    label.is_some_and(|label| label.starts_with(PREFIX))
}

/// The longest `label` may be: synthetic labels carry their prefix and session
/// on top of the ordinary maximum.
pub fn label_limit(label: &str) -> usize {
    MAXIMUM_LABEL
        + if synthetic(Some(label)) {
            PREFIX.len() + SESSION_LABEL_EXTRA
        } else {
            0
        }
}

/// Whether a label belongs to the rehearsal session `session`.
pub fn belongs(session: &str, label: Option<&str>) -> bool {
    label.is_some_and(|l| l.starts_with(&format!("{PREFIX}{session}:")))
}
