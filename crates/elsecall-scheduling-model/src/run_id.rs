//! Identifiers of work requests, shared by runs and leases.
use serde::{Deserialize, Deserializer, Serialize};

/// Source of request identities at the admission boundary.
pub trait RunIds: Send + Sync {
    fn next(&self) -> RunId;
}

pub struct RandomRunIds;
impl RunIds for RandomRunIds {
    fn next(&self) -> RunId {
        RunId::generate()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    /// A random identifier such as `r-3f9a0c41d2b7`.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 6];
        // SAFETY: the buffer is valid and writable for its full length.
        unsafe { libc::arc4random_buf(bytes.as_mut_ptr().cast(), bytes.len()) };
        Self(format!(
            "r-{}",
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ))
    }
    pub fn parse(text: &str) -> Option<Self> {
        let hex = text.strip_prefix("r-")?;
        (hex.len() == 12 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
            .then(|| Self(text.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for RunId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| serde::de::Error::custom("invalid run ID"))
    }
}

#[cfg(test)]
mod tests {
    use super::RunId;

    #[test]
    fn generated_ids_parse_and_others_do_not() {
        let id = RunId::generate();
        assert_eq!(RunId::parse(id.as_str()), Some(id.clone()));
        assert_ne!(RunId::generate(), id);
        for invalid in [
            "",
            "r-",
            "r-3F9A0C41D2B7",
            "x-3f9a0c41d2b7",
            "r-3f9a0c41d2b",
            "r-../../etc",
        ] {
            assert_eq!(RunId::parse(invalid), None, "{invalid}");
        }
    }
}
