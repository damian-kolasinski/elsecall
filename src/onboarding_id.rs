//! Opaque correlation IDs for transfers, operations and bootstrap challenges.
use crate::failure::{Failure, Result};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct OperationId(String);
impl OperationId {
    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|_| {
            Failure::new(
                "random_unavailable",
                "Secure randomness is unavailable.",
                "Retry the operation.",
            )
        })?;
        Ok(Self(bytes.iter().map(|b| format!("{b:02x}")).collect()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for OperationId {
    type Error = Failure;
    fn try_from(value: String) -> Result<Self> {
        if value.len() != 32
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Failure::new(
                "invalid_onboarding_id",
                "The onboarding operation ID is invalid.",
                "Use the ID returned by prepare.",
            ));
        }
        Ok(Self(value))
    }
}
impl From<OperationId> for String {
    fn from(id: OperationId) -> String {
        id.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_are_random_canonical_and_cannot_address_paths() {
        let first = OperationId::generate().unwrap();
        assert_ne!(first, OperationId::generate().unwrap());
        assert_eq!(
            OperationId::try_from(first.as_str().to_owned()).unwrap(),
            first
        );
        for text in [
            "",
            "../state",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "0123456789012345678901234567890g",
        ] {
            assert!(OperationId::try_from(text.to_owned()).is_err());
        }
    }
}
