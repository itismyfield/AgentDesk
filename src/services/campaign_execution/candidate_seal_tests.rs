use super::*;
use crate::services::campaign_execution::{
    control::tests as domain,
    evidence_gate::tests::{OUTPUT, REPO, claim, collected, plan, seal, workspace},
    profile::{ContractError, TestOutcome},
    verification_run::VerificationCollection,
};

#[test]
fn seal_verifies_the_observed_output_without_rewriting_the_input_binding() {
    let binding = domain::binding(&domain::running());
    let sealed = seal(&binding);
    assert_eq!(
        (sealed.base_sha.as_str(), sealed.output_sha.as_str()),
        ("base", OUTPUT)
    );
    assert_eq!(binding.candidate_sha, "candidate");
    let run = collected(&binding, &sealed, &[("t1", TestOutcome::Passed)], Some(0));
    let Some(VerificationCollection::Executed { evidence, .. }) = &run.collection else {
        unreachable!()
    };
    let subject = resolved_subject(&run).unwrap();
    evidence
        .validate_execution_subject(&plan(&binding), &binding, &subject)
        .unwrap();
    // The bound input H0 still cannot vouch for an H1 run, so the binding was never widened.
    assert_eq!(
        evidence.validate_execution(&plan(&binding), &binding),
        Err(ContractError::CandidateMismatch)
    );
    let mut foreign = run.clone();
    foreign.origin.owner_attempt = "other-work".into();
    assert!(resolved_subject(&foreign).is_none());
}

#[test]
fn seal_rejects_moved_dirty_foreign_or_unbound_claims_and_any_replacement() {
    let binding = domain::binding(&domain::running());
    let claim_h1 = claim(&binding, OUTPUT);
    let attempt = |workspace: &WorkspaceObservation, identity: &str| {
        CandidateSeal::seal(
            &binding,
            &claim_h1,
            identity,
            workspace,
            "pin",
            domain::now(),
        )
    };
    assert_eq!(
        attempt(&workspace("editing-h2"), REPO),
        Err(SealError::WrongHead)
    );
    let mut dirty = workspace(OUTPUT);
    dirty.untracked_dirty = true;
    assert_eq!(attempt(&dirty, REPO), Err(SealError::Dirty));
    assert_eq!(
        attempt(&workspace(OUTPUT), "github.com/fork/AgentDesk"),
        Err(SealError::RepositoryMismatch)
    );
    let mut other = binding.clone();
    other.attempt_id = "other".into();
    let foreign_claim = claim(&other, OUTPUT);
    assert_eq!(
        CandidateSeal::seal(
            &binding,
            &foreign_claim,
            REPO,
            &workspace(OUTPUT),
            "pin",
            domain::now()
        ),
        Err(SealError::ClaimMismatch)
    );
    let first = seal(&binding);
    let mut later = workspace(OUTPUT);
    later.observed_at = domain::now() + chrono::Duration::minutes(5);
    let retry = CandidateSeal::seal(&binding, &claim_h1, REPO, &later, "pin", later.observed_at);
    retry.unwrap().confirm_retry(&first).unwrap();
    let replacement = CandidateSeal::seal(
        &binding,
        &claim(&binding, "output-h2"),
        REPO,
        &workspace("output-h2"),
        "pin",
        domain::now(),
    )
    .unwrap();
    assert_eq!(replacement.seal_id, first.seal_id);
    assert_eq!(
        replacement.confirm_retry(&first),
        Err(SealError::AlreadySealed)
    );
}
