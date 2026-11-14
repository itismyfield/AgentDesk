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
