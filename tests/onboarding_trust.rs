//! Real signatures and durable storage; no internal-module mocks.
use elsecall::{
    member_key::MemberKey,
    onboarding_claim::ClaimRequest,
    onboarding_id::OperationId,
    onboarding_invitation::{Invitation, LIFETIME},
    sqlite_onboarding_claims::ClaimLedger,
};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    sync::{Arc, Barrier},
};

fn invitation(issuer: &MemberKey) -> Invitation {
    Invitation::issue(
        issuer,
        issuer.public(),
        OperationId::generate().unwrap(),
        100,
    )
    .unwrap()
}

#[test]
fn claim_authenticates_issuer_candidate_and_live_challenge() {
    let issuer = MemberKey::generate().unwrap();
    let candidate = MemberKey::generate().unwrap();
    let invitation = invitation(&issuer);
    let accepted = invitation.accept(issuer.public(), 100).unwrap();
    let request = ClaimRequest::new(&candidate, &accepted, OperationId::generate().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let ledger = ClaimLedger::open(&dir.path().join("claims")).unwrap();
    let receipt = request
        .claim(&invitation, issuer.public(), &issuer, &ledger, 100)
        .unwrap();
    request
        .verify_receipt(&receipt, &invitation, issuer.public(), 100)
        .unwrap();
    let different_challenge =
        ClaimRequest::new(&candidate, &accepted, OperationId::generate().unwrap());
    assert!(
        different_challenge
            .verify_receipt(&receipt, &invitation, issuer.public(), 100)
            .is_err()
    );
    let different_candidate = ClaimRequest::new(
        &MemberKey::generate().unwrap(),
        &accepted,
        OperationId::generate().unwrap(),
    );
    assert!(
        different_candidate
            .verify_receipt(&receipt, &invitation, issuer.public(), 100)
            .is_err()
    );
    assert!(
        request
            .verify_receipt(&receipt, &invitation, issuer.public(), 100 + LIFETIME)
            .is_err()
    );
}

#[test]
fn interrupted_claim_cannot_be_replayed_or_select_another_member_after_restart() {
    let issuer = MemberKey::generate().unwrap();
    let invitation = invitation(&issuer);
    let accepted = invitation.accept(issuer.public(), 100).unwrap();
    let request = ClaimRequest::new(
        &MemberKey::generate().unwrap(),
        &accepted,
        OperationId::generate().unwrap(),
    );
    let dir = tempfile::tempdir().unwrap();
    {
        let ledger = ClaimLedger::open(&dir.path().join("claims")).unwrap();
        // Simulate losing the response after the durable consume completed.
        request
            .claim(&invitation, issuer.public(), &issuer, &ledger, 100)
            .unwrap();
    }
    let ledger = ClaimLedger::open(&dir.path().join("claims")).unwrap();
    let replay = request
        .claim(&invitation, issuer.public(), &issuer, &ledger, 101)
        .err()
        .unwrap();
    assert_eq!(replay.code, "onboarding_invitation_used");
    let other = ClaimRequest::new(
        &MemberKey::generate().unwrap(),
        &accepted,
        OperationId::generate().unwrap(),
    );
    assert_eq!(
        other
            .claim(&invitation, issuer.public(), &issuer, &ledger, 101)
            .err()
            .unwrap()
            .code,
        "onboarding_invitation_used"
    );
    // A fresh invitation is the explicit retry path, with a new correlation ID.
    let fresh = self::invitation(&issuer);
    let accepted = fresh.accept(issuer.public(), 101).unwrap();
    let request = ClaimRequest::new(
        &MemberKey::generate().unwrap(),
        &accepted,
        OperationId::generate().unwrap(),
    );
    request
        .claim(&fresh, issuer.public(), &issuer, &ledger, 101)
        .unwrap();
    assert_eq!(
        fs::metadata(dir.path().join("claims/claims.sqlite"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn wrong_fleet_issuer_expiry_and_tampered_candidate_do_not_consume() {
    let issuer = MemberKey::generate().unwrap();
    let wrong = MemberKey::generate().unwrap();
    let invitation = invitation(&issuer);
    let accepted = invitation.accept(issuer.public(), 100).unwrap();
    let request = ClaimRequest::new(
        &MemberKey::generate().unwrap(),
        &accepted,
        OperationId::generate().unwrap(),
    );
    let dir = tempfile::tempdir().unwrap();
    let ledger = ClaimLedger::open(&dir.path().join("claims")).unwrap();
    assert!(
        request
            .claim(&invitation, wrong.public(), &issuer, &ledger, 100)
            .is_err()
    );
    assert!(
        request
            .claim(&invitation, issuer.public(), &wrong, &ledger, 100)
            .is_err()
    );
    assert!(
        request
            .claim(
                &invitation,
                issuer.public(),
                &issuer,
                &ledger,
                100 + LIFETIME
            )
            .is_err()
    );
    let mut tampered = serde_json::to_value(&request).unwrap();
    tampered["candidate"] = serde_json::json!(wrong.public());
    let tampered: ClaimRequest = serde_json::from_value(tampered).unwrap();
    assert!(
        tampered
            .claim(&invitation, issuer.public(), &issuer, &ledger, 100)
            .is_err()
    );
    request
        .claim(&invitation, issuer.public(), &issuer, &ledger, 100)
        .unwrap();
}

#[test]
fn concurrent_claims_through_separate_connections_have_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let issuer = Arc::new(MemberKey::generate().unwrap());
    let invitation = Arc::new(invitation(&issuer));
    let first = ClaimLedger::open(&dir.path().join("claims")).unwrap();
    let second = ClaimLedger::open(&dir.path().join("claims")).unwrap();
    let handles: Vec<_> = [first, second]
        .into_iter()
        .map(|ledger| {
            let (barrier, issuer, invitation) =
                (barrier.clone(), issuer.clone(), invitation.clone());
            std::thread::spawn(move || {
                let accepted = invitation.accept(issuer.public(), 100).unwrap();
                let request = ClaimRequest::new(
                    &MemberKey::generate().unwrap(),
                    &accepted,
                    OperationId::generate().unwrap(),
                );
                barrier.wait();
                request
                    .claim(&invitation, issuer.public(), &issuer, &ledger, 100)
                    .is_ok()
            })
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .filter_map(|h| h.join().unwrap().then_some(()))
            .count(),
        1
    );
}

#[test]
fn ledger_refuses_unowned_storage_and_symlinked_database() {
    let dir = tempfile::tempdir().unwrap();
    elsecall::private_files::directory(&dir.path().join("claims")).unwrap();
    let unrelated = dir.path().join("unrelated");
    fs::write(&unrelated, b"keep").unwrap();
    symlink(&unrelated, dir.path().join("claims/claims.sqlite")).unwrap();
    assert!(ClaimLedger::open(&dir.path().join("claims")).is_err());
    assert_eq!(fs::read(&unrelated).unwrap(), b"keep");
    fs::remove_file(dir.path().join("claims/claims.sqlite")).unwrap();
    fs::set_permissions(dir.path().join("claims"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(ClaimLedger::open(&dir.path().join("claims")).is_err());
}
