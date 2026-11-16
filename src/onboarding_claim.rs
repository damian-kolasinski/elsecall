//! One invitation authorizes selection of one bootstrap identity, never a shell.
use crate::{
    failure::{Failure, Result},
    member_key::{MemberKey, PublicKey},
    onboarding_id::OperationId,
    onboarding_invitation::Invitation,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

const DOMAIN: &[u8] = b"elsecall-onboarding-claim-v1\0";
const RECEIPT: &[u8] = b"elsecall-onboarding-claim-receipt-v1\0";

/// The durable boundary must atomically consume the invitation before returning.
/// Even an identical replay is refused; a lost response requires a fresh invitation.
pub trait InvitationUses {
    fn consume(&self, invitation: &[u8; 32], candidate: PublicKey) -> Result<()>;
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimRequest {
    candidate: PublicKey,
    challenge: OperationId,
    signature: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimReceipt {
    signature: String,
}
impl ClaimRequest {
    pub fn new(
        candidate: &MemberKey,
        invitation: &crate::onboarding_invitation::AcceptedInvitation,
        challenge: OperationId,
    ) -> Self {
        let public = candidate.public();
        let message = context(invitation.digest(), public, &challenge);
        Self {
            candidate: public,
            challenge,
            signature: URL_SAFE_NO_PAD.encode(candidate.sign(&message)),
        }
    }
    /// Invoked by the explicitly authorized existing member, after discovery.
    /// Issuer proof and replay protection are separate from candidate possession.
    pub fn claim(
        &self,
        invitation: &Invitation,
        expected_master: PublicKey,
        issuer: &MemberKey,
        uses: &dyn InvitationUses,
        now: u64,
    ) -> Result<ClaimReceipt> {
        let accepted = invitation.accept(expected_master, now)?;
        if self.signature.len() != 86 {
            return Err(rejected());
        }
        let message = context(accepted.digest(), self.candidate, &self.challenge);
        let signature = URL_SAFE_NO_PAD
            .decode(&self.signature)
            .map_err(|_| rejected())?;
        if issuer.public() != accepted.issuer() || !self.candidate.verify(&message, &signature) {
            return Err(rejected());
        }
        uses.consume(accepted.digest(), self.candidate)?;
        Ok(ClaimReceipt {
            signature: URL_SAFE_NO_PAD.encode(issuer.sign(&[RECEIPT, &message].concat())),
        })
    }
    /// Bootstrap must retain its live challenge and verify against that exact request.
    /// This receipt is only an identity binding, not installation/control authority.
    pub fn verify_receipt(
        &self,
        receipt: &ClaimReceipt,
        invitation: &Invitation,
        expected_master: PublicKey,
        now: u64,
    ) -> Result<()> {
        let accepted = invitation.accept(expected_master, now)?;
        if receipt.signature.len() != 86 {
            return Err(rejected());
        }
        let signature = URL_SAFE_NO_PAD
            .decode(&receipt.signature)
            .map_err(|_| rejected())?;
        let message = context(accepted.digest(), self.candidate, &self.challenge);
        if !accepted
            .issuer()
            .verify(&[RECEIPT, &message].concat(), &signature)
        {
            return Err(rejected());
        }
        Ok(())
    }
}

fn context(invitation: &[u8; 32], candidate: PublicKey, challenge: &OperationId) -> Vec<u8> {
    [
        DOMAIN,
        invitation.as_slice(),
        candidate.as_bytes(),
        challenge.as_str().as_bytes(),
    ]
    .concat()
}
fn rejected() -> Failure {
    Failure::new(
        "onboarding_claim_rejected",
        "The bootstrap claim could not be authenticated.",
        "Request a new invitation and retry from the intended fleet.",
    )
}
