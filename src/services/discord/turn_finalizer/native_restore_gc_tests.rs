use super::*;
use crate::services::tui_input::human_frame::history::v2::native_tests::Fixture;
use crate::test_support::wait::{WAIT_CAP, await_within};

#[tokio::test]
async fn native_8148_p2_gc_keeps_boot_debt_after_an_unconfirmed_terminal_phase() {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let root = runtime.path().join("runtime");
    let f = Fixture::at(root.join("injected_frames.json"), 81489952, true);
    f.history
        .note_native_any_key("native-fixture", &f.ticket)
        .unwrap();
    let mut shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    Arc::get_mut(&mut shared)
        .unwrap()
        .restart
        .current_generation = 2;
    let plan = BootInputs::prepare(&root, 2).unwrap();
    let key = plan.inputs[0].key;
    let mut ledger = HashMap::new();
    super::restore(plan.inputs, RestoreStep::Register, &shared, &mut ledger)
        .await
        .unwrap();
    let old = Instant::now()
        .checked_sub(COMPLETION_ADMISSION_TTL + std::time::Duration::from_secs(1))
        .unwrap();
    let entry = ledger.get_mut(&key.exact_key()).unwrap();
    entry.phase = Phase::Finalized;
    entry.finalized_at = Some(old);
    entry.completion_admission.native_input = Some(true);
    let ordinary = TurnKey::new(ChannelId::new(81489953), 81489954, 2);
    let mut settled = LedgerEntry::registered(
        ordinary,
        ProviderKind::Claude,
        RelayOwnerKind::None,
        None,
        CompletionAdmissionPlan::Immediate,
        None,
    );
    settled.phase = Phase::Finalized;
    settled.finalized_at = Some(old);
    ledger.insert(ordinary.exact_key(), settled);
    super::super::super::reconcile::reconcile(&mut ledger, &mut HashMap::new(), &shared).await;
    assert!(ledger.contains_key(&key.exact_key()));
    assert!(!ledger.contains_key(&ordinary.exact_key()));
    let entry = ledger.get(&key.exact_key()).unwrap();
    assert_eq!(entry.native_boot_inputs.len(), 1);
    assert!(
        entry
            .native_boot_inputs
            .values()
            .all(|input| !input.settled)
    );
    let (ack, rx) = oneshot::channel();
    super::super::handle(
        FinalizeMsg::NativeBootReady {
            channel: key.channel_id,
            ack,
        },
        &mut ledger,
    )
    .await;
    assert!(!await_within(rx, WAIT_CAP).await.unwrap());
    let actual = f
        .history
        .native_settlement("native-fixture", &f.ticket)
        .unwrap();
    assert!(actual.outcome.is_none());
    assert!(!actual.cleanup_complete);
}

#[tokio::test]
async fn native_8148_control_not_sent_restores_original_without_native_boot_debt() {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let root = runtime.path().join("runtime");
    let f = Fixture::at(root.join("injected_frames.json"), 81489750, true);
    let mut shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    Arc::get_mut(&mut shared)
        .unwrap()
        .restart
        .current_generation = 1;
    let e = f.ticket.owning_episode.as_ref().unwrap();
    let key = TurnKey::new(
        ChannelId::new(f.ticket.channel_id),
        e.user_message_id,
        e.generation,
    )
    .with_episode_nonce(Some(&e.turn_nonce));
    let actor = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        e.turn_nonce.clone(),
    )));
    await_within(
        shared.mailbox(key.channel_id).restore_active_turn(
            actor.clone(),
            serenity::model::id::UserId::new(7),
            serenity::model::id::MessageId::new(e.user_message_id),
        ),
        WAIT_CAP,
    )
    .await;
    shared
        .turn_finalizer
        .register_start(key, ProviderKind::Claude, RelayOwnerKind::None, &shared);
    let custody = await_within(
        super::super::Handle::prepare(
            &shared,
            key,
            Some(actor.clone()),
            f.history.clone(),
            "native-fixture".into(),
            f.ticket.clone(),
        ),
        WAIT_CAP,
    )
    .await
    .unwrap();
    let (ack, rx) = oneshot::channel();
    shared
        .turn_finalizer
        .tx
        .send(FinalizeMsg::NativeKeyed {
            key,
            input: serde_json::to_string(&("native-fixture", &f.ticket.key)).unwrap(),
            keyed: false,
            history: f.history.clone(),
            shared: shared.clone(),
            ack,
        })
        .unwrap();
    await_within(rx, WAIT_CAP).await.unwrap().unwrap();
    let stored = f
        .history
        .native_settlement("native-fixture", &f.ticket)
        .unwrap();
    assert!(stored.outcome.is_none());
    assert!(!stored.cleanup_complete);
    assert_eq!(stored.extra["not_sent"], true);
    let plan = BootInputs::prepare(&root, e.generation + 1).unwrap();
    assert!(plan.inputs.is_empty());
    let finish = await_within(
        shared.mailbox(key.channel_id).finish_turn_if_token(
            Some(actor),
            crate::services::discord::queue_persistence_context(
                &shared,
                &ProviderKind::Claude,
                key.channel_id,
            ),
        ),
        WAIT_CAP,
    )
    .await;
    assert!(matches!(
        finish,
        crate::services::turn_orchestrator::TokenFinish::Finished(_)
    ));
    assert!(
        f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap()
            .outcome
            .is_none()
    );
    drop(custody);
}
