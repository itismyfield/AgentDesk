use std::collections::BTreeMap;

use super::{tests::*, *};
use crate::services::campaign_execution::{control::tests as domain, view};

fn observe(binding: &AttemptBinding, observation: ActionObservation) -> Command {
    Command::ObserveAction {
        action_id: "action".into(),
        binding: Box::new(binding.clone()),
        observation,
    }
}

async fn ack(pool: &PgPool, key: &str, gate: &HostGate, version: u64) -> CommandOutcome {
    send(
        pool,
        key,
        Command::AcknowledgeFence {
            target: gate.clone(),
            control_version: version,
        },
    )
    .await
}

#[tokio::test]
async fn stop_uses_run_definition_cas_while_events_advance_and_binding_stays_pinned_pg() {
    let mut h = Harness::new().await;
    let state = domain::with_action(ActionState::StartPending, false);
    let binding = domain::binding(&state);
    h.seed(state).await;
    applied(
        send(
            &h.pool,
            "started",
            observe(
                &binding,
                ActionObservation::Started {
                    receipt: "effect".into(),
                },
            ),
        )
        .await,
    );
    let stopped = applied(send(&h.pool, "stop", stop(StopMode::FinishStage)).await);
    assert_eq!(stopped.control_version, Some(2));
    h.reconnect().await;
    let restored = get(&h.pool, "campaign").await.unwrap().unwrap();
    assert!(control::validate_result_binding(&restored, &binding).is_ok());
    assert!(control::validate_new_start(&restored, &binding).is_err());
    assert_eq!(
        restored.run.as_ref().unwrap().attempts["attempt"]
            .binding
            .control_version,
        1
    );
    let mut wrong = binding.clone();
    wrong.candidate_sha = "other".into();
    assert!(control::validate_result_binding(&restored, &wrong).is_err());
    let stale_run = Command::RequestStop {
        expected_revision: 1,
        expected_run: "old-run".into(),
        mode: StopMode::Immediate,
    };
    rejected(
        send(&h.pool, "old-run", stale_run).await,
        RejectionCode::StaleRun,
    );
    let stale_definition = Command::RequestStop {
        expected_revision: 2,
        expected_run: "run".into(),
        mode: StopMode::Immediate,
    };
    rejected(
        send(&h.pool, "old-definition", stale_definition).await,
        RejectionCode::StaleDefinition,
    );
    applied(ack(&h.pool, "ack-v2", &binding.session.gate, 2).await);
    applied(send(&h.pool, "fenced-v2", Command::ConfirmStopFence).await);
    let escalation = applied(send(&h.pool, "escalate", stop(StopMode::Immediate)).await);
    assert_eq!(escalation.control_version, Some(3));
    h.reconnect().await;
    let replay = execute(
        &h.pool,
        "campaign",
        &envelope("stop", stop(StopMode::FinishStage)),
    )
    .await
    .unwrap();
    assert!(replay.replayed);
    assert_eq!(applied(replay.outcome), stopped);
    let restored = get(&h.pool, "campaign").await.unwrap().unwrap();
    let run = restored.run.as_ref().unwrap();
    assert_eq!(run.attempts["attempt"].binding, binding);
    assert_eq!(
        run.attempts["attempt"].outcome,
        Some(AttemptOutcome::UserInterrupted)
    );
    assert_eq!(restored.nodes["a"].quality_failures, 0);
    assert_eq!(run.stop.as_ref().unwrap().confirmed_event, None);
    assert!(run.stop.as_ref().unwrap().acknowledgements.is_empty());
    assert!(control::validate_result_binding(&restored, &binding).is_err());
    rejected(
        ack(&h.pool, "stale-ack", &binding.session.gate, 2).await,
        RejectionCode::StaleFence,
    );
    applied(ack(&h.pool, "ack-v3", &binding.session.gate, 3).await);
    applied(send(&h.pool, "fenced-v3", Command::ConfirmStopFence).await);
    applied(send(&h.pool, "weak-stop", stop(StopMode::FinishStage)).await);
    assert_eq!(
        get(&h.pool, "campaign")
            .await
            .unwrap()
            .unwrap()
            .run
            .unwrap()
            .control_version,
        3
    );
    h.close().await;
}

#[tokio::test]
async fn alive_unknown_unreconciled_action_and_wrong_incarnation_block_edit_and_stopped_pg() {
    for mode in [StopMode::Immediate, StopMode::FinishStage] {
        let mut h = Harness::new().await;
        let mut state = domain::with_action(ActionState::Started, true);
        let binding = domain::binding(&state);
        // FinishStage's unfinished attempt must be settled by F3; this fixture is already settled.
        state
            .run
            .as_mut()
            .unwrap()
            .attempts
            .get_mut("attempt")
            .unwrap()
            .outcome = Some(AttemptOutcome::QualityFailed);
        state.run.as_mut().unwrap().sessions[0].observation = SessionObservation::Alive {
            observed_at: domain::now(),
        };
        h.seed(state).await;
        applied(send(&h.pool, "stop", stop(mode)).await);
        applied(ack(&h.pool, "ack", &binding.session.gate, 2).await);
        applied(send(&h.pool, "fenced", Command::ConfirmStopFence).await);
        h.reconnect().await;
        rejected(
            send(&h.pool, "alive", Command::ConfirmStopped).await,
            RejectionCode::SessionsNotGone,
        );
        rejected(
            send(&h.pool, "edit-alive", edit(1, "Unsafe")).await,
            RejectionCode::InvalidState,
        );
        applied(
            send(
                &h.pool,
                "unknown",
                Command::ObserveSession {
                    identity: binding.session.clone(),
                    observation: SessionObservation::Unknown {
                        reason: "host unreachable".into(),
                        last_observed_at: Some(domain::now()),
                    },
                },
            )
            .await,
        );
        let mut wrong = binding.session.clone();
        wrong.incarnation = "old-process".into();
        rejected(
            send(
                &h.pool,
                "wrong-gone",
                Command::ObserveSession {
                    identity: wrong,
                    observation: SessionObservation::Gone {
                        observed_at: domain::now(),
                        evidence: "different process".into(),
                    },
                },
            )
            .await,
            RejectionCode::StaleBinding,
        );
        rejected(
            send(&h.pool, "unknown-stopped", Command::ConfirmStopped).await,
            RejectionCode::SessionsNotGone,
        );
        applied(
            send(
                &h.pool,
                "gone",
                Command::ObserveSession {
                    identity: binding.session.clone(),
                    observation: SessionObservation::Gone {
                        observed_at: domain::now(),
                        evidence: "reachable host confirms exact process absent".into(),
                    },
                },
            )
            .await,
        );
        rejected(
            send(&h.pool, "unreconciled", Command::ConfirmStopped).await,
            RejectionCode::UnreconciledActions,
        );
        applied(
            send(
                &h.pool,
                "reconciled",
                observe(
                    &binding,
                    ActionObservation::Reconciled {
                        evidence: "external result recovered".into(),
                    },
                ),
            )
            .await,
        );
        applied(send(&h.pool, "stopped", Command::ConfirmStopped).await);
        applied(send(&h.pool, "edit", edit(1, "Safe")).await);
        assert_eq!(
            get(&h.pool, "campaign")
                .await
                .unwrap()
                .unwrap()
                .definition_revision,
            2
        );
        h.close().await;
    }
}

#[tokio::test]
async fn restored_stopped_projection_still_checks_liveness_and_unreconciled_actions_pg() {
    for observation in [
        SessionObservation::Alive {
            observed_at: domain::now(),
        },
        SessionObservation::Unknown {
            reason: "unknown".into(),
            last_observed_at: None,
        },
    ] {
        let h = Harness::new().await;
        let mut state = domain::with_action(ActionState::Started, true);
        state.control = CampaignControl::Stopped;
        state.run.as_mut().unwrap().sessions[0].observation = observation;
        h.seed(state).await;
        rejected(
            send(&h.pool, "edit", edit(1, "Unsafe")).await,
            RejectionCode::SessionsNotGone,
        );
        h.close().await;
    }
    let h = Harness::new().await;
    let mut state = domain::with_action(ActionState::Claimed, false);
    state.control = CampaignControl::Stopped;
    h.seed(state).await;
    rejected(
        send(&h.pool, "edit", edit(1, "Unsafe")).await,
        RejectionCode::UnreconciledActions,
    );
    h.close().await;
}

#[tokio::test]
async fn action_only_host_requires_exact_ack_after_reconnect_and_binding_cannot_be_rewritten_pg() {
    let mut h = Harness::new().await;
    let mut state = domain::with_action(ActionState::Claimed, false);
    let binding = domain::binding(&state);
    let mut effect_gate = binding.session.gate.clone();
    effect_gate.gate = "remote-effect".into();
    state
        .run
        .as_mut()
        .unwrap()
        .actions
        .get_mut("action")
        .unwrap()
        .target = effect_gate.clone();
    h.seed(state).await;
    applied(send(&h.pool, "stop", stop(StopMode::Immediate)).await);
    h.reconnect().await;
    let state = get(&h.pool, "campaign").await.unwrap().unwrap();
    let run = state.run.unwrap();
    assert!(run.sessions.is_empty());
    assert_eq!(run.stop.unwrap().targets.len(), 2);
    rejected(
        send(&h.pool, "missing-ack", Command::ConfirmStopFence).await,
        RejectionCode::FenceIncomplete,
    );
    let mut old_gate = effect_gate.clone();
    old_gate.incarnation = "old-boot".into();
    rejected(
        ack(&h.pool, "old-boot", &old_gate, 2).await,
        RejectionCode::StaleBinding,
    );
    applied(ack(&h.pool, "session-gate", &binding.session.gate, 2).await);
    rejected(
        send(&h.pool, "still-missing", Command::ConfirmStopFence).await,
        RejectionCode::FenceIncomplete,
    );
    applied(ack(&h.pool, "effect-gate", &effect_gate, 2).await);
    applied(send(&h.pool, "fenced", Command::ConfirmStopFence).await);
    applied(send(&h.pool, "stopped", Command::ConfirmStopped).await);
    let mut wrong = binding.clone();
    wrong.control_version = 2;
    rejected(
        send(
            &h.pool,
            "rebound",
            observe(
                &wrong,
                ActionObservation::NotStarted {
                    evidence: "old binding rewritten".into(),
                },
            ),
        )
        .await,
        RejectionCode::StaleBinding,
    );
    assert_eq!(
        attempts(&h.pool, "campaign", "run").await.unwrap()[0]
            .attempt
            .binding,
        binding
    );
    h.close().await;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FakeGate {
    target: HostGate,
    run_id: String,
    version: u64,
    fenced: bool,
    journal: BTreeMap<String, (AttemptBinding, Option<String>)>,
    effects: u32,
}

impl FakeGate {
    async fn install(pool: &PgPool, binding: &AttemptBinding) {
        sqlx::query(
            "CREATE TABLE fake_host_gate (id INTEGER PRIMARY KEY, document JSONB NOT NULL)",
        )
        .execute(pool)
        .await
        .unwrap();
        let gate = Self {
            target: binding.session.gate.clone(),
            run_id: binding.run_id.clone(),
            version: 1,
            fenced: false,
            journal: BTreeMap::new(),
            effects: 0,
        };
        sqlx::query("INSERT INTO fake_host_gate VALUES (1, $1)")
            .bind(Json(gate))
            .execute(pool)
            .await
            .unwrap();
    }

    async fn load(pool: &PgPool) -> Self {
        sqlx::query_scalar::<_, Json<Self>>("SELECT document FROM fake_host_gate WHERE id = 1")
            .fetch_one(pool)
            .await
            .unwrap()
            .0
    }

    async fn save(&self, tx: &mut Transaction<'_, Postgres>) {
        sqlx::query("UPDATE fake_host_gate SET document = $1 WHERE id = 1")
            .bind(Json(self))
            .execute(&mut **tx)
            .await
            .unwrap();
    }

    async fn initiate(
        pool: &PgPool,
        binding: &AttemptBinding,
        crash_before_receipt: bool,
    ) -> Option<String> {
        let mut tx = pool.begin().await.unwrap();
        let mut gate = sqlx::query_scalar::<_, Json<Self>>(
            "SELECT document FROM fake_host_gate WHERE id = 1 FOR UPDATE",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap()
        .0;
        if let Some((recorded, receipt)) = gate.journal.get("action") {
            return (recorded == binding).then(|| receipt.clone()).flatten();
        }
        if gate.fenced
            || gate.version != binding.control_version
            || gate.target != binding.session.gate
            || gate.run_id != binding.run_id
        {
            return None;
        }
        gate.effects += 1;
        let receipt = (!crash_before_receipt).then(|| "fake-effect-receipt".into());
        gate.journal
            .insert("action".into(), (binding.clone(), receipt.clone()));
        gate.save(&mut tx).await;
        tx.commit().await.unwrap();
        receipt
    }

    async fn fence(pool: &PgPool, version: u64) {
        let mut tx = pool.begin().await.unwrap();
        let mut gate = sqlx::query_scalar::<_, Json<Self>>(
            "SELECT document FROM fake_host_gate WHERE id = 1 FOR UPDATE",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap()
        .0;
        gate.version = version;
        gate.fenced = true;
        gate.save(&mut tx).await;
        tx.commit().await.unwrap();
    }
}

#[tokio::test]
async fn fake_gate_fence_first_cancels_intent_claim_and_inflight_without_restart_launch_pg() {
    for phase in [
        ActionState::Planned,
        ActionState::Claimed,
        ActionState::StartPending,
    ] {
        let mut h = Harness::new().await;
        let state = domain::with_action(phase, false);
        let binding = domain::binding(&state);
        h.seed(state).await;
        FakeGate::install(&h.pool, &binding).await;
        applied(send(&h.pool, "stop", stop(StopMode::Immediate)).await);
        FakeGate::fence(&h.pool, 2).await;
        h.reconnect().await;
        assert_eq!(FakeGate::initiate(&h.pool, &binding, false).await, None);
        if phase == ActionState::StartPending {
            rejected(
                ack(&h.pool, "early-ack", &binding.session.gate, 2).await,
                RejectionCode::UnreconciledActions,
            );
            applied(
                send(
                    &h.pool,
                    "not-started",
                    observe(
                        &binding,
                        ActionObservation::NotStarted {
                            evidence: "fake gate journal has no initiation".into(),
                        },
                    ),
                )
                .await,
            );
        }
        applied(ack(&h.pool, "ack", &binding.session.gate, 2).await);
        applied(send(&h.pool, "fenced", Command::ConfirmStopFence).await);
        applied(send(&h.pool, "stopped", Command::ConfirmStopped).await);
        assert_eq!(FakeGate::load(&h.pool).await.effects, 0);
        assert_eq!(FakeGate::initiate(&h.pool, &binding, false).await, None);
        let next = Command::PlanAction {
            action: Box::new(
                domain::with_action(ActionState::Planned, false)
                    .run
                    .unwrap()
                    .actions["action"]
                    .clone(),
            ),
        };
        rejected(
            send(&h.pool, "new-action", next).await,
            RejectionCode::InvalidState,
        );
        h.close().await;
    }
}

#[tokio::test]
async fn fake_gate_start_first_recovers_receipt_and_unknown_session_after_pool_restart_pg() {
    let mut h = Harness::new().await;
    let state = domain::with_action(ActionState::StartPending, false);
    let binding = domain::binding(&state);
    h.seed(state).await;
    FakeGate::install(&h.pool, &binding).await;
    let receipt = FakeGate::initiate(&h.pool, &binding, false).await.unwrap();
    applied(send(&h.pool, "stop", stop(StopMode::Immediate)).await);
    FakeGate::fence(&h.pool, 2).await;
    h.reconnect().await;
    rejected(
        send(&h.pool, "unclassified", Command::ConfirmStopFence).await,
        RejectionCode::FenceIncomplete,
    );
    assert_eq!(
        FakeGate::initiate(&h.pool, &binding, false).await,
        Some(receipt.clone())
    );
    let mut wrong_incarnation = binding.clone();
    wrong_incarnation.session.incarnation = "previous-process".into();
    assert_eq!(
        FakeGate::initiate(&h.pool, &wrong_incarnation, false).await,
        None
    );
    applied(
        send(
            &h.pool,
            "late-receipt",
            observe(&binding, ActionObservation::Started { receipt }),
        )
        .await,
    );
    applied(ack(&h.pool, "ack", &binding.session.gate, 2).await);
    applied(send(&h.pool, "fenced", Command::ConfirmStopFence).await);
    rejected(
        send(&h.pool, "not-gone", Command::ConfirmStopped).await,
        RejectionCode::SessionsNotGone,
    );
    assert_eq!(FakeGate::load(&h.pool).await.effects, 1);
    assert!(
        view::view(
            &get(&h.pool, "campaign").await.unwrap().unwrap(),
            domain::now()
        )
        .unknown
    );
    h.close().await;
}

#[tokio::test]
async fn fake_gate_crash_after_start_before_receipt_keeps_unknown_and_never_repeats_pg() {
    let mut h = Harness::new().await;
    let state = domain::with_action(ActionState::StartPending, false);
    let binding = domain::binding(&state);
    h.seed(state).await;
    FakeGate::install(&h.pool, &binding).await;
    assert_eq!(FakeGate::initiate(&h.pool, &binding, true).await, None);
    h.reconnect().await;
    assert_eq!(FakeGate::initiate(&h.pool, &binding, false).await, None);
    assert_eq!(FakeGate::load(&h.pool).await.effects, 1);
    applied(
        send(
            &h.pool,
            "unknown",
            observe(
                &binding,
                ActionObservation::Unknown {
                    reason: "fake host crashed after effect, receipt missing".into(),
                },
            ),
        )
        .await,
    );
    let restored = get(&h.pool, "campaign").await.unwrap().unwrap();
    assert_eq!(
        restored.run.as_ref().unwrap().actions["action"].state,
        ActionState::Unknown
    );
    applied(send(&h.pool, "stop", stop(StopMode::Immediate)).await);
    FakeGate::fence(&h.pool, 2).await;
    h.reconnect().await;
    rejected(
        ack(&h.pool, "unclassified-ack", &binding.session.gate, 2).await,
        RejectionCode::UnreconciledActions,
    );
    rejected(
        send(&h.pool, "unclassified-fence", Command::ConfirmStopFence).await,
        RejectionCode::FenceIncomplete,
    );
    rejected(
        send(&h.pool, "unsafe-edit", edit(1, "Unsafe")).await,
        RejectionCode::InvalidState,
    );
    rejected(
        send(&h.pool, "unsafe-stopped", Command::ConfirmStopped).await,
        RejectionCode::FenceIncomplete,
    );
    assert_eq!(FakeGate::initiate(&h.pool, &binding, false).await, None);
    assert_eq!(FakeGate::load(&h.pool).await.effects, 1);
    h.close().await;
}

#[tokio::test]
async fn replaced_current_run_does_not_erase_old_attempt_action_or_runtime_pg() {
    let h = Harness::new().await;
    let original = domain::with_action(ActionState::Started, true);
    h.seed(original.clone()).await;
    // Future run creation is not a production command yet; seed its replacement only here.
    let mut replacement = domain::running();
    replacement.event_sequence = 2;
    replacement.run.as_mut().unwrap().id = "new-run".into();
    replacement.run.as_mut().unwrap().attempts.clear();
    replacement
        .nodes
        .values_mut()
        .for_each(|node| *node = NodeRuntime::default());
    let mut tx = h.pool.begin().await.unwrap();
    let event = Event {
        sequence: 2,
        run_id: Some("new-run".into()),
        command_id: "fixture-new-run".into(),
        observed_at: domain::now(),
        kind: EventKind::DefinitionCreated,
    };
    let result = CommandResult {
        definition_revision: 1,
        run_id: Some("new-run".into()),
        control_version: Some(1),
        event_sequence: 2,
        control: CampaignControl::Running,
    };
    write::persist(
        &mut tx,
        Some(&original),
        &Decision {
            state: replacement,
            events: vec![event],
            result,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        get(&h.pool, "campaign")
            .await
            .unwrap()
            .unwrap()
            .run
            .unwrap()
            .id,
        "new-run"
    );
    assert_eq!(attempts(&h.pool, "campaign", "run").await.unwrap().len(), 1);
    assert_eq!(
        action_history(&h.pool, "campaign", "run", "action", 0, 100)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        run_history(&h.pool, "campaign", "run", 0, 100)
            .await
            .unwrap()[0]
            .nodes,
        original.nodes
    );
    h.close().await;
}
