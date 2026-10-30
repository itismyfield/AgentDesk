use super::*;
use crate::services::campaign_execution::{
    control::tests as domain,
    evidence_gate::tests::{OUTPUT, collected, seal},
    model::{AttemptOutcome, NodeRuntime, Stage},
    profile::TestOutcome,
    review_gate::StagePrerequisite,
};
use serde_json::json;

// Work A sealed and verified by V; Review R is now current and holds V as its prerequisite.
fn handed_over() -> (CampaignState, VerificationRun, StagePrerequisite) {
    let mut state = domain::running();
    let work = domain::binding(&state);
    let v = collected(&work, &seal(&work), &[("t1", TestOutcome::Passed)], Some(0));
    let mut review = work.clone();
    review.attempt_id = "review".into();
    review.role = Role::Review;
    review.stage = Stage::Review;
    let prerequisite = StagePrerequisite::bind(&v, &seal(&work), &review, 2, 1).unwrap();
    let run = state.run.as_mut().unwrap();
    run.attempts.get_mut("attempt").unwrap().outcome = Some(AttemptOutcome::Passed);
    let mut attempt = run.attempts["attempt"].clone();
    attempt.binding = review;
    attempt.outcome = None;
    run.attempts.insert("review".into(), attempt);
    run.verifications
        .insert(v.origin.execution_id.clone(), v.clone());
    state.nodes.insert(
        "a".into(),
        NodeRuntime {
            stage: Stage::Review,
            current_attempt: Some("review".into()),
            ..state.nodes["a"].clone()
        },
    );
    (state, v, prerequisite)
}

#[test]
fn tui_body_cannot_choose_source_owner_status_consumer_or_runner_payload() {
    let output = json!({"kind": "output_claim", "output_sha": OUTPUT});
    for (field, value) in [
        ("source", json!("server_runner")),
        ("status", json!("accepted")),
        ("binding", json!({"attempt_id": "review"})),
        ("authority", json!({"source": "current_work"})),
    ] {
        let mut body = json!({"evidence_id": "e", "payload": output.clone()});
        body[field] = value;
        assert!(
            serde_json::from_value::<TuiSubmission>(body).is_err(),
            "{field}"
        );
    }
    let mut claimed = output.clone();
    claimed["source"] = json!("server_runner");
    let body = json!({"evidence_id": "e", "payload": claimed});
    assert!(serde_json::from_value::<TuiSubmission>(body).is_err());
    let verification =
        json!({"evidence_id": "e", "payload": {"kind": "verification", "execution_id": "v"}});
    assert!(serde_json::from_value::<TuiSubmission>(verification).is_err());
    let state = domain::running();
    let work = domain::binding(&state);
    let body = json!({"evidence_id": "run:v", "payload": output});
    let submission = serde_json::from_value(body).unwrap();
    let envelope = EvidenceEnvelope::from_tui(submission, &work, "pin", domain::now()).unwrap();
    assert_eq!(envelope.source, EvidenceSource::BoundTui);
    assert_eq!(envelope.evidence_id, "tui:attempt:run:v");
    let decision = admit(&envelope, &state, "pin", &[]);
    assert_eq!(decision.status, EvidenceStatus::Accepted);
}

#[test]
fn forged_runner_source_payload_or_result_is_rejected() {
    let (state, v, prerequisite) = handed_over();
    let runner = EvidenceEnvelope::from_runner(&v, domain::now()).unwrap();
    let prerequisites = [prerequisite];
    let rejected = |envelope: &EvidenceEnvelope| {
        let decision = admit(envelope, &state, "pin", &prerequisites);
        assert_eq!(decision.status, EvidenceStatus::Rejected);
        decision.reasons
    };
    let mut relabelled = runner.clone();
    relabelled.source = EvidenceSource::BoundTui;
    assert_eq!(rejected(&relabelled), vec![GateReason::SourceMismatch]);
    let mut claimed = claim_from(&state, "review");
    claimed.source = EvidenceSource::ServerRunner;
    assert_eq!(rejected(&claimed), vec![GateReason::SourceMismatch]);
    let mut forged = v.clone();
    if let Some(VerificationCollection::Executed { audit, .. }) = &mut forged.collection {
        audit.exit_code = Some(0);
        audit.executed = 99;
    }
    let forged = EvidenceEnvelope::from_runner(&forged, domain::now()).unwrap();
    assert!(matches!(
        rejected(&forged)[..],
        [GateReason::ResultMismatch(_)]
    ));
    let mut tampered = runner.clone();
    tampered.payload_digest = "other".into();
    assert_eq!(rejected(&tampered), vec![GateReason::DigestMismatch]);
    assert_eq!(
        admit(&runner, &state, "new-pin", &prerequisites).reasons,
        vec![GateReason::StalePin]
    );
}

fn claim_from(state: &CampaignState, attempt: &str) -> EvidenceEnvelope {
    let binding = &state.run.as_ref().unwrap().attempts[attempt].binding;
    let submission = TuiSubmission {
        evidence_id: "claim".into(),
        payload: TuiPayload::OutputClaim {
            output_sha: OUTPUT.into(),
        },
    };
    EvidenceEnvelope::from_tui(submission, binding, "pin", domain::now()).unwrap()
}

#[test]
fn late_result_keeps_original_owner_and_feeds_only_its_exact_prerequisite() {
    let (state, v, prerequisite) = handed_over();
    let runner = EvidenceEnvelope::from_runner(&v, domain::now()).unwrap();
    assert_eq!(runner.binding, v.origin.owner_binding);
    assert_eq!(runner.binding.attempt_id, "attempt");
    let mut unrelated = prerequisite.clone();
    unrelated.id = "other".into();
    unrelated.target.execution_id = "v-other".into();
    let decision = admit(&runner, &state, "pin", &[prerequisite.clone(), unrelated]);
    assert_eq!(decision.status, EvidenceStatus::Accepted);
    assert_eq!(decision.prerequisites, vec![prerequisite.id.clone()]);
    // The finished Work attempt can no longer submit; its late claim is not relabelled onto R.
    let late = claim_from(&state, "attempt");
    let decision = admit(&late, &state, "pin", &[]);
    assert_eq!(decision.status, EvidenceStatus::Rejected);
    assert_eq!(decision.reasons, vec![GateReason::StaleAttempt]);
    assert_eq!(
        admit(&claim_from(&state, "review"), &state, "pin", &[]).reasons,
        vec![GateReason::RoleMismatch]
    );
    let review = &state.run.as_ref().unwrap().attempts["review"].binding;
    let request = TuiSubmission {
        evidence_id: "log".into(),
        payload: TuiPayload::ObservationRequest {
            execution_id: Some(v.origin.execution_id.clone()),
            links: vec!["https://ci.example/run/1".into()],
        },
    };
    let request = EvidenceEnvelope::from_tui(request, review, "pin", domain::now()).unwrap();
    let decision = admit(&request, &state, "pin", &[]);
    assert_eq!(decision.status, EvidenceStatus::Recorded);
    assert_eq!(decision.reasons, vec![GateReason::NotEvidence]);
}
