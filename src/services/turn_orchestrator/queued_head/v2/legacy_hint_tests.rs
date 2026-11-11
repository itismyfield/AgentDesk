use super::super::super::v2_types::marker_control::{
    MarkerEntry, MarkerNativeSource, MarkerOwner, MarkerPathSnapshot, MarkerRowExpectation,
    MarkerWitness,
};
use super::*;
use serde_json::json;

fn item(id: u64) -> PendingQueueItem {
    serde_json::from_value(json!({"message_id":id,"author_id":7,"channel_id":8057201,
        "source_message_ids":[id],"queued_generation":3,"text":"kept original"}))
    .unwrap()
}
fn owner() -> Owner {
    let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join(format!("marker-{}.json", uuid::Uuid::new_v4()));
    let mut owner = Owner::adopt(path, "marker-scope".into()).unwrap();
    owner.enqueue_fixture(&item(80572011)).unwrap();
    owner
}
fn control(owner: &Owner, front: &Front) -> MarkerControl {
    MarkerControl {
        cleanup_id: "cleanup-1".into(),
        identity: sources::identity(&front.1),
        front_receipt: owner.a_front_receipt(front).unwrap(),
        epoch: "epoch-1".into(),
        expected: MarkerRowExpectation::Missing {
            owner: MarkerOwner {
                provider: "codex".into(),
                discord_token_hash: "marker-token".into(),
                channel_id: "8057201".into(),
                logical_key: "AgentDesk-codex-marker".into(),
                owner_node: "node".into(),
                runtime_root: "/fixture".into(),
            },
            reserved_row_id: Some(17),
        },
        paths: vec![MarkerPathSnapshot {
            path: std::env::temp_dir().join("marker-fixture.kind"),
            parent: [1, 2],
            entry: MarkerEntry::Absent,
        }],
        witness: MarkerWitness {
            nonce: None,
            source: None,
        },
        stage: MarkerStage::Intent,
        attempt: None,
    }
}
fn outbox(owner: &mut Owner, control: MarkerControl) {
    let front = owner.front().unwrap();
    owner.a_intent(&front, control.clone(), "epoch-1").unwrap();
    owner
        .a_stage(
            &control.cleanup_id,
            &MarkerStage::Intent,
            MarkerStage::Receipt,
        )
        .unwrap();
    owner
        .a_stage(
            &control.cleanup_id,
            &MarkerStage::Receipt,
            MarkerStage::Outbox {
                event_id: "marker-scope/cleanup-1/front/epoch-1".into(),
            },
        )
        .unwrap();
}

#[test]
fn marker_intent_keeps_initial_native_witness_across_restart_and_publication_recovery() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut owner = owner();
    let front = owner.front().unwrap();
    let mut debt = control(&owner, &front);
    debt.witness = MarkerWitness {
        nonce: Some("initial-nonce".into()),
        source: Some(MarkerNativeSource {
            path: std::env::temp_dir().join("source.jsonl"),
            session: "source-session".into(),
            nonce: "source-nonce".into(),
            proof_seq: 8,
            declared_file: [1, 3],
            file: [1, 3],
        }),
    };
    owner.fault = Fault::Before;
    assert!(owner.a_intent(&front, debt.clone(), "epoch-1").is_err());
    assert!(owner.a_controls().is_empty());
    owner.fault = Fault::AfterRename;
    owner.a_intent(&front, debt.clone(), "epoch-1").unwrap();
    let path = owner.path.clone();
    drop(owner);
    let raw = std::fs::read_to_string(path).unwrap();
    let super::super::super::compat::SnapshotRead::ReadOnlyV2(restored) =
        super::super::super::compat::inspect(&raw).unwrap()
    else {
        panic!("owned intent")
    };
    assert_eq!(restored.version, 3);
    assert_eq!(restored.a_control, vec![debt]);
}

#[test]
fn marker_stages_never_ack_a_claim_and_unknown_candidate_never_advances() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut owner = owner();
    let front = owner.front().unwrap();
    let debt = control(&owner, &front);
    outbox(&mut owner, debt.clone());
    assert!(!owner.a_ack_if_settled(&debt.cleanup_id).unwrap());
    owner.grant_front(&front).unwrap();
    owner.claim_turn(front.1.message_id, &front).unwrap();
    assert!(!owner.a_ack_if_settled(&debt.cleanup_id).unwrap());
    assert!(
        owner
            .a_stage(&debt.cleanup_id, &MarkerStage::Intent, MarkerStage::Receipt)
            .is_err()
    );
    assert_eq!(owner.a_controls().len(), 1);
    owner.fault = Fault::Third;
    let attempt = serde_json::from_value(json!({"nonce":"attempt","provider":"codex","target":{"host":"herdr","owner":"logical","root":"socket","execution_nonce":"execution"},"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}})).unwrap();
    assert!(owner.a_attempt(&debt.cleanup_id, attempt).is_err());
    assert!(owner.a_ack_if_settled(&debt.cleanup_id).is_err());
    assert_eq!(std::fs::read(&owner.path).unwrap(), b"third state");
    assert!(owner.snapshot.a_control[0].attempt.is_none());
}

#[test]
fn marker_refusal_retains_only_its_original_and_owes_one_notice_without_opaque_retry() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut owner = owner();
    owner.enqueue_fixture(&item(80572012)).unwrap();
    let front = owner.front().unwrap();
    let debt = control(&owner, &front);
    outbox(&mut owner, debt.clone());
    assert!(
        owner
            .a_refuse(&front, &debt.cleanup_id, "evidence_changed")
            .unwrap()
    );
    assert!(owner.a_controls().is_empty());
    assert!(!owner.front_outstanding());
    assert_eq!(owner.front().unwrap().1.message_id, 80572012);
    let retained = &owner.snapshot.retained_inputs[0];
    assert_eq!(retained.input.original.text, "kept original");
    assert!(retained.input.attempt.is_none());
    assert!(matches!(
        retained.disposition,
        Some(RetainedDisposition::KnownNotSent)
    ));
    assert!(!retained.notice.enqueued);
    assert!(owner.requeue_known_not_sent().unwrap().is_empty());
}

#[test]
fn marker_cancel_ack_uses_the_exact_generation_and_preserves_unrelated_debt() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut owner = owner();
    let front = owner.front().unwrap();
    let debt = control(&owner, &front);
    outbox(&mut owner, debt.clone());
    let mut other = debt;
    other.cleanup_id = "other-generation".into();
    other.identity.queued_generation += 1;
    owner.snapshot.a_control.push(other.clone());
    owner.clear_waiting().unwrap();
    assert_eq!(owner.a_controls(), vec![other]);
}

#[test]
fn marker_claim_witness_revokes_on_new_claim_hold_and_owner_drop() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut owner = owner();
    let front = owner.front().unwrap();
    owner.grant_front(&front).unwrap();
    owner.claim_turn(front.1.message_id, &front).unwrap();
    let first = owner
        .a_claim_liveness(&front, owner.claim_number())
        .unwrap();
    assert!(first.load(Ordering::SeqCst));
    owner.grant_front(&front).unwrap();
    owner.claim_turn(front.1.message_id, &front).unwrap();
    assert!(!first.load(Ordering::SeqCst));
    let second = owner
        .a_claim_liveness(&front, owner.claim_number())
        .unwrap();
    owner.hold_front(&front).unwrap();
    assert!(!second.load(Ordering::SeqCst));
    owner.grant_front(&front).unwrap();
    owner.claim_turn(front.1.message_id, &front).unwrap();
    let third = owner
        .a_claim_liveness(&front, owner.claim_number())
        .unwrap();
    drop(owner);
    assert!(!third.load(Ordering::SeqCst));
}

#[test]
fn marker_missing_row_binds_only_the_durably_reserved_id_before_cleanup() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut owner = owner();
    let front = owner.front().unwrap();
    let debt = control(&owner, &front);
    owner.a_intent(&front, debt, "epoch-1").unwrap();
    assert!(owner.a_bind_created_row("cleanup-1", 18, "key").is_err());
    let created = owner
        .a_bind_created_row("cleanup-1", 17, "exact-key")
        .unwrap();
    assert!(matches!(
        created.expected,
        MarkerRowExpectation::Legacy { row_id: 17, .. }
    ));
    assert_eq!(
        owner
            .a_bind_created_row("cleanup-1", 17, "exact-key")
            .unwrap(),
        created
    );
    owner
        .a_stage("cleanup-1", &MarkerStage::Intent, MarkerStage::Receipt)
        .unwrap();
    assert!(
        owner
            .a_bind_created_row("cleanup-1", 17, "exact-key")
            .is_err()
    );
}

#[test]
fn marker_codec_requires_explicit_initial_absence_and_keeps_unknown_native_fields() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = owner();
    let front = owner.front().unwrap();
    let debt = control(&owner, &front);
    let mut raw = serde_json::to_value(debt).unwrap();
    raw["witness"].as_object_mut().unwrap().remove("nonce");
    assert!(serde_json::from_value::<MarkerControl>(raw.clone()).is_err());
    raw["witness"]["nonce"] = json!(null);
    raw["witness"]["future"] = json!(true);
    assert!(serde_json::from_value::<MarkerControl>(raw).is_err());
}

#[test]
fn marker_unproved_result_keeps_ambiguity_in_one_commit_without_failed_terminal_or_retry() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut owner = owner();
    owner.enqueue_fixture(&item(80572012)).unwrap();
    let digest = owner.snapshot.queued_digest(0).unwrap();
    owner.snapshot.admission_binds.push(AdmissionBind {
        receipt: "tk:unproved-origin:fixture".into(),
        at: BindAt::Queued {
            index: 0,
            item: digest,
        },
        context: Some(AdmissionContext {
            original: None,
            execution: Some(json!({
                "metadata":{"human_input":{"origin_id":"unproved-origin"}}
            })),
        }),
    });
    let front = owner.front().unwrap();
    let debt = control(&owner, &front);
    outbox(&mut owner, debt.clone());
    owner.grant_front(&front).unwrap();
    owner.claim_turn(front.1.message_id, &front).unwrap();
    let life = owner
        .a_claim_liveness(&front, owner.claim_number())
        .unwrap();
    owner.fault = Fault::Before;
    assert!(
        owner
            .a_unproved(&front, &debt.cleanup_id, "result_missing")
            .is_err()
    );
    assert!(!life.load(Ordering::SeqCst));
    assert!(owner.snapshot.retained_inputs.is_empty());
    assert_eq!(owner.a_controls().len(), 1);
    owner.fault = Fault::AfterRename;
    assert!(
        owner
            .a_unproved(&front, &debt.cleanup_id, "result_missing")
            .unwrap()
    );
    assert!(!life.load(Ordering::SeqCst));
    assert!(!owner.front_outstanding());
    assert!(owner.a_controls().is_empty());
    assert!(owner.snapshot.owed_drop_receipts.is_empty());
    let retained = &owner.snapshot.retained_inputs[0];
    assert_eq!(retained.input.original.message_id, front.1.message_id);
    assert!(matches!(retained.reason, RetainedReason::Unconfirmed));
    assert!(retained.disposition.is_none());
    assert_eq!(retained.notice.kind, "unconfirmed");
    assert!(!retained.notice.enqueued);
    assert!(owner.requeue_known_not_sent().unwrap().is_empty());
    assert_eq!(owner.front().unwrap().1.message_id, 80572012);
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&owner.path).unwrap()).unwrap();
    assert_eq!(raw["retained_inputs"][0]["reason"]["kind"], "unconfirmed");
    assert!(raw["retained_inputs"][0]["disposition"].is_null());
}

#[test]
fn marker_unproved_result_does_not_reclassify_an_existing_attempt_or_other_front() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut owner = owner();
    let front = owner.front().unwrap();
    let debt = control(&owner, &front);
    outbox(&mut owner, debt.clone());
    let before = std::fs::read(&owner.path).unwrap();
    let mut other = front.clone();
    other.1.queued_generation += 1;
    assert!(
        !owner
            .a_unproved(&other, &debt.cleanup_id, "other_front")
            .unwrap()
    );
    assert_eq!(std::fs::read(&owner.path).unwrap(), before);
    owner.grant_front(&front).unwrap();
    owner.claim_turn(front.1.message_id, &front).unwrap();
    let attempt = serde_json::from_value(json!({"nonce":"existing-attempt","provider":"codex","target":{"host":"herdr","owner":"logical","root":"socket","execution_nonce":"execution"},"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}})).unwrap();
    let key = owner
        .reserve_claimed(
            (&front, owner.claim_number()),
            attempt,
            PreparedPayload {
                frame: "raw kept".into(),
                digest: "digest".into(),
                attribution: json!({}),
                followers: vec![],
            },
        )
        .unwrap();
    let before = std::fs::read(&owner.path).unwrap();
    assert!(
        !owner
            .a_unproved(&front, &debt.cleanup_id, "result_missing")
            .unwrap()
    );
    assert_eq!(std::fs::read(&owner.path).unwrap(), before);
    assert_eq!(owner.active_attempt().unwrap(), key);
    assert!(owner.snapshot.retained_inputs.is_empty());
    assert_eq!(owner.a_controls().len(), 1);
}
