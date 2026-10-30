use super::tests::{Harness, applied, execute, rejected, send};
use super::*;
use crate::services::campaign_execution::{
    candidate_seal::CandidateSeal, control::tests as domain, stage_fsm::tests as fsm,
    verification_run::Slot,
};
use evidence_store::StoredEvidence;

async fn store_seal(h: &Harness, seal: &CandidateSeal) {
    let mut tx = h.pool.begin().await.unwrap();
    let record = StoredEvidence::Seal {
        seal: Box::new(seal.clone()),
    };
    evidence_store::record(&mut tx, "campaign", "run", &record)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn begin_review_commits_once_survives_restart_and_cites_only_the_stored_seal_pg() {
    let mut h = Harness::new().await;
    let (mut state, work) = fsm::assigned();
    let seal = fsm::seal_of(&state, &work);
    let start = fsm::execution(&state, "work-a").start_executions[0].clone();
    fsm::run_v(&mut state, &start, &seal, 0);
    h.seed(state.clone()).await;
    let review = fsm::review_of(&work);
    let begin = || fsm::begin(&state, &seal, &review);
    rejected(
        send(&h.pool, "begin-unsealed", begin()).await,
        RejectionCode::StaleBinding,
    );
    store_seal(&h, &seal).await;
    let first = applied(send(&h.pool, "begin", begin()).await);
    h.reconnect().await;
    let restored = get(&h.pool, "campaign").await.unwrap().unwrap();
    let node = &restored.nodes["a"];
    assert_eq!(node.stage, Stage::Review);
    assert_eq!(node.current_attempt.as_deref(), Some("review-r"));
    let run = restored.run.as_ref().unwrap();
    assert_eq!(run.fences["a"].epoch, 2);
    assert_eq!(run.prerequisites.len(), 1);
    assert_eq!(run.attempts["work-a"].outcome, Some(AttemptOutcome::Passed));
    // The lost response replays from the receipt after a restart; a fresh key adds no epoch.
    let replay = execute(
        &h.pool,
        "campaign",
        &super::tests::envelope("begin", begin()),
    )
    .await
    .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.outcome, CommandOutcome::Decided(Ok(first.clone())));
    let retry = applied(send(&h.pool, "begin-retry", begin()).await);
    assert_eq!(retry.event_sequence, first.event_sequence);
    let after = get(&h.pool, "campaign").await.unwrap().unwrap();
    assert_eq!(after, restored);
    h.close().await;
}

type Rewrite = fn(&mut Run);

#[tokio::test]
async fn stored_v_origin_filled_slots_prerequisites_and_pins_cannot_be_rewritten_pg() {
    let h = Harness::new().await;
    let (state, ..) = fsm::handed_over();
    h.seed(state.clone()).await;
    let rewrites: [Rewrite; 4] = [
        |run| {
            let v = run.verifications.values_mut().next().unwrap();
            v.origin.registered_node_epoch += 1;
        },
        |run| {
            let v = run
                .verifications
                .values_mut()
                .find(|v| v.subject.bound().is_some())
                .unwrap();
            v.subject = Slot::Unbound;
        },
        |run| {
            run.prerequisites
                .values_mut()
                .next()
                .unwrap()
                .consumer_node_epoch += 1
        },
        |run| {
            let attempt = run.attempts.get_mut("work-a").unwrap();
            attempt.execution.as_mut().unwrap().pin_hash = "another-pin".into();
        },
    ];
    for rewrite in rewrites {
        let mut wrong = state.clone();
        wrong.event_sequence += 1;
        rewrite(wrong.run.as_mut().unwrap());
        let decision = Decision {
            result: CommandResult {
                definition_revision: 1,
                run_id: Some("run".into()),
                control_version: Some(1),
                event_sequence: wrong.event_sequence,
                control: CampaignControl::Running,
            },
            events: vec![Event {
                sequence: wrong.event_sequence,
                run_id: Some("run".into()),
                command_id: "fixture-rewrite".into(),
                observed_at: domain::now(),
                kind: EventKind::DefinitionCreated,
            }],
            state: wrong,
        };
        let mut tx = h.pool.begin().await.unwrap();
        assert!(matches!(
            write::persist(&mut tx, Some(&state), &decision).await,
            Err(StoreError::InconsistentState)
        ));
        tx.rollback().await.unwrap();
        assert_eq!(get(&h.pool, "campaign").await.unwrap(), Some(state.clone()));
    }
    h.close().await;
}
