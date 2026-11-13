//! Member codes: a member's name, public key and endpoints as one copyable string.
//!
//! Codes hold no secrets. The operator carries them between machines, and that
//! channel is what vouches for the key inside; codes are validated as untrusted input.
use crate::{
    failure::{Failure, Result},
    roster::Member,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

const PREFIX: &str = "elsecall-member-v1.";
const LIMIT: usize = 4096;

pub fn encode(member: &Member) -> String {
    let json = serde_json::to_vec(member).expect("serializable member");
    format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(json))
}

pub fn decode(code: &str) -> Result<Member> {
    let invalid = || {
        Failure::new(
            "invalid_member_code",
            "The member code is malformed or from an incompatible version.",
            "Copy the code again from elsecall fleet identity on that machine.",
        )
    };
    if code.len() > LIMIT {
        return Err(invalid());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(code.trim().strip_prefix(PREFIX).ok_or_else(invalid)?)
        .map_err(|_| invalid())?;
    serde_json::from_slice(&bytes).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roster::tests::member;

    #[test]
    fn codes_round_trip_and_reject_tampering() {
        let a = member("a", 1);
        let code = encode(&a);
        assert_eq!(decode(&code).unwrap(), a);
        assert_eq!(decode(&format!("  {code}\n")).unwrap(), a);
        assert!(decode(&code[1..]).is_err());
        assert!(decode(&format!("{code}x")).is_err());
        let bad = URL_SAFE_NO_PAD.encode(br#"{"name":"a","key":"x","endpoints":[]}"#);
        assert!(decode(&format!("{PREFIX}{bad}")).is_err());
        assert!(decode(&"x".repeat(LIMIT + 1)).is_err());
    }
}
