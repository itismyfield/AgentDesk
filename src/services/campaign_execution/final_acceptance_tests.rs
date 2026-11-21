use chrono::TimeDelta;

use super::*;
use crate::services::campaign_execution::control::{self, tests as domain};
use crate::services::campaign_execution::deploy_policy::DeployFact;
use crate::services::campaign_execution::deploy_policy::tests::{
    deployed, landed, record, request, sha, witnesses,
};

fn accept(state: &CampaignState, fact: AcceptanceFact) -> Result<CampaignState, Rejection> {
    let mut envelope = domain::envelope(Command::RecordAcceptance {
        node_id: "a".into(),
        fact: Box::new(fact),
    });
    envelope.idempotency_key = format!("key-{}", state.event_sequence);
    control::decide(state, &envelope).map(|decision| decision.state)
}

fn check(condition: &str, target: &str) -> AcceptanceCheck {
    AcceptanceCheck {
        condition_id: condition.into(),
        reference: "https://example.com/live-check".into(),
        deployed_sha: target.into(),
        observed_at: domain::now(),
    }
}

fn missing(state: &CampaignState) -> Option<String> {
    let rejection = accept(state, AcceptanceFact::Judge).unwrap_err();
    assert_eq!(rejection.code, RejectionCode::GateNotPassed);
    rejection.target
}

fn complete(state: &CampaignState) -> Result<CampaignState, Rejection> {
    let command = Command::CompleteNode {
        expected_run: "run".into(),
        node_id: "a".into(),
        expected_epoch: state.run.as_ref().unwrap().fences["a"].epoch,
    };
    control::decide(state, &domain::envelope(command)).map(|decision| decision.state)
}

#[test]
fn m8_a_node_completes_only_after_every_acceptance_condition_is_proven() {
    let main = sha('c');
    let landed = landed(false);
    assert_eq!(
        complete(&landed).unwrap_err().code,
        RejectionCode::GateNotPassed
    );
    assert_eq!(missing(&landed).as_deref(), Some("acceptance:ac-deploy"));
    // Nothing can be checked against a deployment that was not witnessed.
    let early = AcceptanceFact::Checked(check("ac-live", &main));
    assert!(accept(&landed, early.clone()).is_err());

    let deployed = deployed();
    assert_eq!(missing(&deployed).as_deref(), Some("acceptance:ac-live"));
    assert_eq!(
        complete(&deployed).unwrap_err().code,
        RejectionCode::GateNotPassed
    );
    let mut stale = check("ac-live", &main);
    stale.observed_at -= TimeDelta::seconds(1);
    let mut unreferenced = check("ac-live", &main);
    unreferenced.reference = " ".into();
    for refused in [
        check("ac-live", &sha('d')),
        check("ac-deploy", &main),
        check("ac-absent", &main),
        stale,
        unreferenced,
    ] {
        assert!(accept(&deployed, AcceptanceFact::Checked(refused)).is_err());
    }
    let checked = accept(&deployed, early.clone()).unwrap();
    assert_eq!(accept(&checked, early).unwrap(), checked);
    let mut other = check("ac-live", &main);
    other.reference = "https://example.com/another".into();
    assert_eq!(
        accept(&checked, AcceptanceFact::Checked(other))
            .unwrap_err()
            .code,
        RejectionCode::IdempotencyConflict
    );
    assert!(!checked.nodes["a"].acceptance_complete);

    let judged = accept(&checked, AcceptanceFact::Judge).unwrap();
    assert!(judged.nodes["a"].acceptance_complete && judged.nodes["a"].evidence_current);
    assert_eq!(accept(&judged, AcceptanceFact::Judge).unwrap(), judged);
    let done = complete(&judged).unwrap();
    assert_eq!(done.nodes["a"].stage, Stage::Complete);
}

#[test]
fn m8_unknown_deployment_evidence_and_a_lost_merge_receipt_prove_nothing() {
    let main = sha('c');
    let waiting = record(&landed(false), request(&main)).unwrap();
    let lost = DeployFact::Indeterminate {
        evidence: "lost start reply".into(),
    };
    let unknown = record(&waiting, lost).unwrap();
    assert_eq!(missing(&unknown).as_deref(), Some("acceptance:ac-deploy"));
    let failed = DeployFact::Failed {
        reason: "no witness before the deadline".into(),
        uncertain: false,
    };
    let failed = record(&unknown, failed).unwrap();
    assert_eq!(missing(&failed).as_deref(), Some("acceptance:ac-deploy"));
    assert!(accept(&failed, AcceptanceFact::Checked(check("ac-live", &main))).is_err());
    assert_eq!(witnesses(&main).len(), 2);

    let mut untested = deployed();
    let run = untested.run.as_mut().unwrap();
    run.actions
        .get_mut("merge:opening")
        .unwrap()
        .initiation_receipt = None;
    assert_eq!(missing(&untested).as_deref(), Some("acceptance:ac-test"));

    // An edited definition no longer names the landed candidate.
    let mut edited = deployed();
    let run = edited.run.as_mut().unwrap();
    run.attempts
        .get_mut("attempt")
        .unwrap()
        .binding
        .hashes
        .acceptance = "other".into();
    assert_eq!(
        accept(&edited, AcceptanceFact::Judge).unwrap_err().code,
        RejectionCode::StaleBinding
    );
}
