//! The accepted invitation pins a fleet and the member allowed to claim a bootstrap.
//! Signatures authenticate subsequent claims to that pin, not to a global fleet CA.
use crate::onboarding_id::OperationId;
use crate::{
    failure::{Failure, Result},
    member_key::{MemberKey, PublicKey},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const LIFETIME: u64 = 900;
const DOMAIN: &[u8] = b"elsecall-onboarding-invitation-v1\0";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    version: u32,
    operation: OperationId,
    issuer: PublicKey,
    fleet_master: PublicKey,
    issued_at: u64,
    expires_at: u64,
}

/// Wire artifact; carries public pins and a signature, never bearer credentials.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invitation {
    payload: String,
    signature: String,
}

/// Validated invitation. Constructed only by strict signature/time/pin validation.
pub struct AcceptedInvitation {
    payload: Payload,
    digest: [u8; 32],
}
impl Invitation {
    pub fn issue(
        issuer: &MemberKey,
        fleet_master: PublicKey,
        operation: OperationId,
        now: u64,
    ) -> Result<Self> {
        // V1 has no delegation chain. A member cannot self-assert master authority.
        if issuer.public() != fleet_master {
            return Err(invalid());
        }
        let expires_at = now.checked_add(LIFETIME).ok_or_else(invalid)?;
        let payload = serde_json::to_vec(&Payload {
            version: 1,
            operation,
            issuer: issuer.public(),
            fleet_master,
            issued_at: now,
            expires_at,
        })
        .expect("invitation serializes");
        let message = [DOMAIN, &payload].concat();
        Ok(Self {
            signature: URL_SAFE_NO_PAD.encode(issuer.sign(&message)),
            payload: URL_SAFE_NO_PAD.encode(payload),
        })
    }

    /// `expected_master` comes from the accepted transfer's pin, never discovery.
    /// Call again at the claim boundary: validation is not a permanent authorization.
    pub fn accept(&self, expected_master: PublicKey, now: u64) -> Result<AcceptedInvitation> {
        if self.payload.len() > 2048 || self.signature.len() != 86 {
            return Err(invalid());
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(&self.payload)
            .map_err(|_| invalid())?;
        let payload: Payload = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        let signature = URL_SAFE_NO_PAD
            .decode(&self.signature)
            .map_err(|_| invalid())?;
        if payload.version != 1
            || payload.fleet_master != expected_master
            || payload.issuer != expected_master
            || payload.expires_at.checked_sub(payload.issued_at) != Some(LIFETIME)
            || now < payload.issued_at
            || now >= payload.expires_at
            || !payload
                .issuer
                .verify(&[DOMAIN, &bytes].concat(), &signature)
        {
            return Err(invalid());
        }
        Ok(AcceptedInvitation {
            payload,
            digest: Sha256::digest([DOMAIN, &bytes].concat()).into(),
        })
    }
}
impl AcceptedInvitation {
    pub fn operation(&self) -> &OperationId {
        &self.payload.operation
    }
    pub fn issuer(&self) -> PublicKey {
        self.payload.issuer
    }
    pub fn expires_at(&self) -> u64 {
        self.payload.expires_at
    }
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

fn invalid() -> Failure {
    Failure::new(
        "onboarding_invitation_rejected",
        "The invitation is invalid, expired, or belongs to another fleet.",
        "Request a new invitation from the intended fleet.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invitation_round_trip_pins_master_and_enforces_time_boundaries() {
        let issuer = MemberKey::generate().unwrap();
        let other = MemberKey::generate().unwrap();
        let id = OperationId::generate().unwrap();
        let invitation = Invitation::issue(&issuer, issuer.public(), id.clone(), 100).unwrap();
        let json = serde_json::to_vec(&invitation).unwrap();
        let invitation: Invitation = serde_json::from_slice(&json).unwrap();
        assert_eq!(
            invitation.accept(issuer.public(), 100).unwrap().operation(),
            &id
        );
        assert!(invitation.accept(issuer.public(), 99).is_err());
        assert!(invitation.accept(issuer.public(), 999).is_ok());
        assert!(invitation.accept(issuer.public(), 1000).is_err());
        assert!(invitation.accept(other.public(), 100).is_err());
        assert!(Invitation::issue(&other, issuer.public(), id.clone(), 100).is_err());
        assert!(Invitation::issue(&issuer, issuer.public(), id, u64::MAX).is_err());
    }

    #[test]
    fn all_signed_fields_and_signature_are_tamper_evident() {
        let issuer = MemberKey::generate().unwrap();
        let invitation = Invitation::issue(
            &issuer,
            issuer.public(),
            OperationId::generate().unwrap(),
            100,
        )
        .unwrap();
        let original = URL_SAFE_NO_PAD.decode(&invitation.payload).unwrap();
        for (field, replacement) in [
            ("version", serde_json::json!(2)),
            (
                "operation",
                serde_json::json!(OperationId::generate().unwrap()),
            ),
            (
                "issuer",
                serde_json::json!(MemberKey::generate().unwrap().public()),
            ),
            (
                "fleet_master",
                serde_json::json!(MemberKey::generate().unwrap().public()),
            ),
            ("issued_at", serde_json::json!(99)),
            ("expires_at", serde_json::json!(99999)),
        ] {
            let mut payload: serde_json::Value = serde_json::from_slice(&original).unwrap();
            payload[field] = replacement;
            let tampered = Invitation {
                payload: URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap()),
                signature: invitation.signature.clone(),
            };
            assert!(tampered.accept(issuer.public(), 100).is_err(), "{field}");
        }
        let bad_signature = Invitation {
            payload: invitation.payload,
            signature: URL_SAFE_NO_PAD.encode([0u8; 64]),
        };
        assert!(bad_signature.accept(issuer.public(), 100).is_err());
    }

    #[test]
    fn even_master_signed_unsupported_or_overlong_payload_is_refused() {
        let issuer = MemberKey::generate().unwrap();
        for (version, expires_at, signer_matches) in
            [(2, 1000, true), (1, 1001, true), (1, 1000, false)]
        {
            let signer = MemberKey::generate().unwrap();
            let signer = if signer_matches { &issuer } else { &signer };
            let payload = serde_json::to_vec(&Payload {
                version,
                operation: OperationId::generate().unwrap(),
                issuer: signer.public(),
                fleet_master: issuer.public(),
                issued_at: 100,
                expires_at,
            })
            .unwrap();
            let invitation = Invitation {
                payload: URL_SAFE_NO_PAD.encode(&payload),
                signature: URL_SAFE_NO_PAD.encode(signer.sign(&[DOMAIN, &payload].concat())),
            };
            assert!(invitation.accept(issuer.public(), 100).is_err());
        }
        let oversized = Invitation {
            payload: "x".repeat(2049),
            signature: "x".repeat(86),
        };
        assert!(oversized.accept(issuer.public(), 100).is_err());
    }
}
