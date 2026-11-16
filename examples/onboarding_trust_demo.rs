//! Isolated demonstration: real signatures and SQLite, no networking or installation.
use elsecall::{
    member_key::MemberKey, onboarding_claim::ClaimRequest, onboarding_id::OperationId,
    onboarding_invitation::Invitation, onboarding_operation::Operations,
    sqlite_onboarding_claims::ClaimLedger,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state = tempfile::tempdir()?;
    let master = MemberKey::generate()?;
    let candidate = MemberKey::generate()?;
    let wrong_fleet = MemberKey::generate()?;
    let now = elsecall::onboarding_operation::now();
    let operations = Operations::new(state.path().join("operations"));
    let operation = operations.prepare(now)?;
    println!(
        "prepare: {:?}; artifact absent; onboarding_installer_unavailable",
        operation.stage
    );
    let invitation = Invitation::issue(&master, master.public(), OperationId::generate()?, now)?;
    let accepted = invitation.accept(master.public(), now)?;
    let request = ClaimRequest::new(&candidate, &accepted, OperationId::generate()?);
    let ledger = ClaimLedger::open(&state.path().join("claims"))?;
    let receipt = request.claim(&invitation, master.public(), &master, &ledger, now)?;
    request.verify_receipt(&receipt, &invitation, master.public(), now)?;
    println!("claim: candidate possession and master receipt verified");
    drop(ledger);
    let ledger = ClaimLedger::open(&state.path().join("claims"))?;
    let replay = request
        .claim(&invitation, master.public(), &master, &ledger, now)
        .err()
        .ok_or("replay accepted")?;
    println!("after restart: {}", replay.code);
    let wrong = invitation
        .accept(wrong_fleet.public(), now)
        .err()
        .ok_or("wrong fleet accepted")?;
    println!("wrong fleet: {}", wrong.code);
    let expired = invitation
        .accept(master.public(), accepted.expires_at())
        .err()
        .ok_or("expired invitation accepted")?;
    println!("expired: {}", expired.code);
    println!(
        "cancel: {:?}",
        operations.status(&operation.id, now, true)?.stage
    );
    println!("Demonstrated trust primitives only. No Mac installed, enrolled, or controlled.");
    Ok(())
}
