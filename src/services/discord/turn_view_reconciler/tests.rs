use super::serenity::{ChannelId, MessageId};
use super::*;

fn target() -> TurnViewTarget {
    target_with(100_000_000_000_001, 100_000_000_000_101)
}

fn target_with(channel_id: u64, message_id: u64) -> TurnViewTarget {
    TurnViewTarget::intake_user_message(ChannelId::new(channel_id), MessageId::new(message_id))
}

fn owner(generation: u64, suffix: &str) -> TurnViewOwner {
    TurnViewOwner::new(generation, format!("turn-{suffix}"))
}

fn expected(emoji: char, identity: &str) -> (char, String) {
    (emoji, identity.to_string())
}

fn clear_persisted(target: TurnViewTarget) {
    TurnViewReconciler::default().delete_persisted_target(target, "test_clear");
}

fn persisted_path(target: TurnViewTarget) -> std::path::PathBuf {
    TurnViewReconciler::persisted_target_path(target).expect("turn view persisted path")
}

fn persisted_exists(target: TurnViewTarget) -> bool {
    persisted_path(target).exists()
}

fn persisted_record(
    shared: &SharedData,
    target: TurnViewTarget,
    provider: &str,
    applied: &str,
) -> PersistedTargetState {
    PersistedTargetState {
        version: PERSISTED_STATE_VERSION,
        provider: provider.to_string(),
        kind: target.kind.as_str().to_string(),
        channel_id: target.channel_id.get(),
        message_id: target.message_id.get(),
        owner_generation: 91,
        owner_turn_id: "turn-persisted".to_string(),
        applied: applied.to_string(),
        identity_label: target.kind.identity_label().to_string(),
        token_hash: Some(shared.token_hash.clone()),
    }
}

fn write_persisted(record: &PersistedTargetState, target: TurnViewTarget) {
    let json = serde_json::to_string_pretty(record).expect("serialize persisted turn view state");
    super::super::runtime_store::atomic_write(&persisted_path(target), &json)
        .expect("write persisted turn view state");
}

fn snapshot_reactions(
    reconciler: &TurnViewReconciler,
    target: TurnViewTarget,
) -> Vec<(char, String)> {
    let mut reactions = Vec::<(char, String)>::new();
    for op in reconciler
        .ops
        .lock()
        .expect("turn view test op lock")
        .iter()
    {
        if op.target != target {
            continue;
        }
        if op.add {
            let reaction = (op.emoji, op.identity.clone());
            if !reactions.contains(&reaction) {
                reactions.push(reaction);
            }
        } else {
            reactions.retain(|reaction| *reaction != (op.emoji, op.identity.clone()));
        }
    }
    reactions
}

async fn note_sequence(states: &[TurnViewState]) -> TurnViewReconciler {
    let reconciler = TurnViewReconciler::default();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = target();
    clear_persisted(target);
    let owner = owner(1, "a");
    for state in states {
        reconciler
            .note_state(
                &shared,
                target,
                owner.clone(),
                TurnViewIdentity::Test("intake-a"),
                *state,
                "test",
            )
            .await;
    }
    reconciler
}

#[tokio::test]
async fn sequence_start_complete_leaves_only_completed_reaction() {
    let reconciler = note_sequence(&[TurnViewState::Pending, TurnViewState::Completed]).await;

    assert_eq!(
        snapshot_reactions(&reconciler, target()),
        vec![expected('✅', "intake-a")]
    );
}

#[tokio::test]
async fn sequence_start_fail_leaves_only_failed_reaction() {
    let reconciler = note_sequence(&[TurnViewState::Pending, TurnViewState::Failed]).await;

    assert_eq!(
        snapshot_reactions(&reconciler, target()),
        vec![expected('⚠', "intake-a")]
    );
}

#[tokio::test]
async fn sequence_start_stop_leaves_only_stopped_reaction() {
    let reconciler = note_sequence(&[TurnViewState::Pending, TurnViewState::Stopped]).await;

    assert_eq!(
        snapshot_reactions(&reconciler, target()),
        vec![expected('🛑', "intake-a")]
    );
}

#[tokio::test]
async fn sequence_start_recover_complete_removes_hourglass_residue() {
    let reconciler = note_sequence(&[
        TurnViewState::Pending,
        TurnViewState::None,
        TurnViewState::Completed,
    ])
    .await;

    assert_eq!(
        snapshot_reactions(&reconciler, target()),
        vec![expected('✅', "intake-a")]
    );
}

#[tokio::test]
async fn cold_clear_removes_possible_lifecycle_residue() {
    let reconciler = TurnViewReconciler::default();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = TurnViewTarget::intake_user_message(
        ChannelId::new(100_000_000_000_301),
        MessageId::new(100_000_000_000_302),
    );
    clear_persisted(target);

    reconciler
        .note_state(
            &shared,
            target,
            owner(11, "cold-clear"),
            TurnViewIdentity::Test("intake-a"),
            TurnViewState::None,
            "test",
        )
        .await;

    let ops = reconciler.ops();
    assert!(
        ops.iter().any(|op| !op.add && op.emoji == '⏳'),
        "cold clear must issue a stale hourglass removal"
    );
    assert_eq!(snapshot_reactions(&reconciler, target), Vec::new());
}

#[tokio::test]
async fn stale_completion_after_newer_turn_started_is_ignored() {
    let reconciler = TurnViewReconciler::default();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = target();
    clear_persisted(target);
    let older = owner(1, "old");
    let newer = owner(2, "new");
    reconciler
        .note_state(
            &shared,
            target,
            older.clone(),
            TurnViewIdentity::Test("intake-a"),
            TurnViewState::Pending,
            "test",
        )
        .await;
    reconciler
        .note_state(
            &shared,
            target,
            newer,
            TurnViewIdentity::Test("intake-b"),
            TurnViewState::Pending,
            "test",
        )
        .await;
    reconciler
        .note_state(
            &shared,
            target,
            older,
            TurnViewIdentity::Test("intake-a"),
            TurnViewState::Completed,
            "test",
        )
        .await;

    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('⏳', "intake-a")]
    );
    assert_eq!(
        reconciler
            .ops()
            .iter()
            .filter(|op| op.emoji == '✅')
            .count(),
        0
    );
}

#[tokio::test]
async fn regression_3164_adder_identity_equals_remover_identity_on_thread_target() {
    let reconciler = TurnViewReconciler::default();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let parent = ChannelId::new(100_000_000_000_201);
    let thread = ChannelId::new(100_000_000_000_202);
    shared.dispatch.thread_parents.insert(parent, thread);
    let target = TurnViewTarget::tui_direct_bot_anchor(thread, MessageId::new(100_000_000_000_203));
    clear_persisted(target);
    let owner = owner(7, "tui");
    reconciler
        .note_state(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("provider-bot"),
            TurnViewState::Pending,
            "test",
        )
        .await;
    reconciler
        .note_state(
            &shared,
            target,
            owner,
            TurnViewIdentity::Test("ignored-later"),
            TurnViewState::Completed,
            "test",
        )
        .await;

    let ops = reconciler.ops();
    assert_eq!(ops.len(), 3);
    assert!(ops.iter().all(|op| op.identity == "provider-bot"));
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('✅', "provider-bot")]
    );
}

#[tokio::test]
async fn cold_terminal_uses_persisted_pending_adder_identity_after_identity_change() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = TurnViewTarget::intake_user_message(
        ChannelId::new(100_000_000_000_401),
        MessageId::new(100_000_000_000_402),
    );
    clear_persisted(target);
    let owner = owner(17, "persisted");
    let adder = TurnViewReconciler::default();
    adder
        .note_state(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("adder-bot"),
            TurnViewState::Pending,
            "test",
        )
        .await;

    let cold = TurnViewReconciler::default();
    cold.note_state(
        &shared,
        target,
        owner.clone(),
        TurnViewIdentity::Test("current-caller"),
        TurnViewState::Completed,
        "test",
    )
    .await;

    let ops = cold.ops();
    assert_eq!(ops.len(), 2);
    assert!(ops.iter().all(|op| op.identity == "adder-bot"));
    assert!(ops.iter().any(|op| !op.add && op.emoji == '⏳'));
    assert!(ops.iter().any(|op| op.add && op.emoji == '✅'));
    assert_eq!(
        snapshot_reactions(&cold, target),
        vec![expected('✅', "adder-bot")]
    );
    cold.evict_finalized(target, &owner);
}

#[tokio::test]
async fn terminal_delivery_evicts_persisted_target_and_lock() {
    let reconciler = TurnViewReconciler::default();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = target_with(100_000_000_000_451, 100_000_000_000_452);
    clear_persisted(target);
    let owner = owner(19, "terminal-evict");

    reconciler
        .note_state(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("intake-a"),
            TurnViewState::Pending,
            "test",
        )
        .await;

    assert!(persisted_exists(target));
    assert!(reconciler.targets.contains_key(&target));
    assert_eq!(reconciler.target_lock_count(target), 1);

    reconciler
        .note_state(
            &shared,
            target,
            owner,
            TurnViewIdentity::Test("ignored-terminal"),
            TurnViewState::Completed,
            "test",
        )
        .await;

    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('✅', "intake-a")]
    );
    assert!(!persisted_exists(target));
    assert!(!reconciler.targets.contains_key(&target));
    assert_eq!(reconciler.target_lock_count(target), 0);
}

#[tokio::test]
async fn regression_3303_success_path_leaves_no_hourglass_residue() {
    let reconciler = note_sequence(&[TurnViewState::Pending, TurnViewState::Completed]).await;

    assert!(
        !snapshot_reactions(&reconciler, target())
            .iter()
            .any(|(emoji, _)| *emoji == '⏳')
    );
}

#[tokio::test]
async fn concurrent_terminal_notifications_leave_exactly_one_terminal_reaction() {
    let reconciler = TurnViewReconciler::default();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = TurnViewTarget::intake_user_message(
        ChannelId::new(100_000_000_000_501),
        MessageId::new(100_000_000_000_502),
    );
    clear_persisted(target);
    let owner = owner(23, "race");
    reconciler
        .note_state(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("intake-a"),
            TurnViewState::Pending,
            "test",
        )
        .await;

    let completed = reconciler.note_state(
        &shared,
        target,
        owner.clone(),
        TurnViewIdentity::Test("ignored-complete"),
        TurnViewState::Completed,
        "test",
    );
    let failed = reconciler.note_state(
        &shared,
        target,
        owner,
        TurnViewIdentity::Test("ignored-fail"),
        TurnViewState::Failed,
        "test",
    );
    let _ = tokio::join!(completed, failed);

    let reactions = snapshot_reactions(&reconciler, target);
    assert!(
        !reactions.iter().any(|(emoji, _)| *emoji == '⏳'),
        "pending residue must be removed"
    );
    assert_eq!(
        reactions
            .iter()
            .filter(|(emoji, _)| matches!(emoji, '✅' | '⚠' | '🛑'))
            .count(),
        1,
        "serialized terminal notifications must converge to one terminal reaction: {reactions:?}"
    );
}

#[tokio::test]
async fn queued_terminal_notification_uses_existing_lock_while_prior_terminal_evicts() {
    let reconciler = std::sync::Arc::new(TurnViewReconciler::default());
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = target_with(100_000_000_000_551, 100_000_000_000_552);
    clear_persisted(target);
    let owner = owner(29, "lock-race");
    reconciler
        .note_state(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("intake-a"),
            TurnViewState::Pending,
            "test",
        )
        .await;

    let held_lock = reconciler.target_lock(target);
    let held_guard = held_lock.lock().await;
    let complete_task = {
        let reconciler = std::sync::Arc::clone(&reconciler);
        let shared = std::sync::Arc::clone(&shared);
        let owner = owner.clone();
        tokio::spawn(async move {
            reconciler
                .note_state(
                    &shared,
                    target,
                    owner,
                    TurnViewIdentity::Test("ignored-complete"),
                    TurnViewState::Completed,
                    "test",
                )
                .await
        })
    };
    let fail_task = {
        let reconciler = std::sync::Arc::clone(&reconciler);
        let shared = std::sync::Arc::clone(&shared);
        tokio::spawn(async move {
            reconciler
                .note_state(
                    &shared,
                    target,
                    owner,
                    TurnViewIdentity::Test("ignored-fail"),
                    TurnViewState::Failed,
                    "test",
                )
                .await
        })
    };

    for _ in 0..50 {
        if std::sync::Arc::strong_count(&held_lock) >= 4 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        std::sync::Arc::strong_count(&held_lock) >= 4,
        "both queued notifications must be waiting on the original target lock"
    );
    drop(held_guard);
    drop(held_lock);
    let (complete, fail) = tokio::join!(complete_task, fail_task);
    complete.expect("complete task join");
    fail.expect("fail task join");

    let reactions = snapshot_reactions(&reconciler, target);
    assert!(
        !reactions.iter().any(|(emoji, _)| *emoji == '⏳'),
        "pending residue must be removed"
    );
    assert_eq!(
        reactions
            .iter()
            .filter(|(emoji, _)| matches!(emoji, '✅' | '⚠' | '🛑'))
            .count(),
        1,
        "queued terminal notifications must stay serialized after eviction: {reactions:?}"
    );
    assert!(!persisted_exists(target));
    assert!(!reconciler.targets.contains_key(&target));
    assert_eq!(reconciler.target_lock_count(target), 0);
}

#[tokio::test]
async fn regression_4041_duplicate_transitions_are_coalesced() {
    let reconciler = note_sequence(&[
        TurnViewState::Pending,
        TurnViewState::Pending,
        TurnViewState::Completed,
        TurnViewState::Completed,
    ])
    .await;

    let ops = reconciler.ops();
    assert_eq!(
        ops.iter().filter(|op| op.add && op.emoji == '⏳').count(),
        1
    );
    assert!(ops.iter().any(|op| !op.add && op.emoji == '⏳'));
    assert_eq!(
        snapshot_reactions(&reconciler, target()),
        vec![expected('✅', "intake-a")]
    );
    assert!(!persisted_exists(target()));
    assert!(!reconciler.targets.contains_key(&target()));
    assert_eq!(reconciler.target_lock_count(target()), 0);
}

#[tokio::test]
async fn permanent_failure_deletes_persisted_pending_and_stays_cold() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = target_with(100_000_000_000_601, 100_000_000_000_602);
    clear_persisted(target);
    let owner = owner(31, "gone");
    let starter = TurnViewReconciler::default();
    starter
        .note_state(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("intake-a"),
            TurnViewState::Pending,
            "test",
        )
        .await;
    assert!(persisted_exists(target));

    let failing = TurnViewReconciler::with_test_deliveries(vec![TurnViewDelivery::FailedPermanent]);
    let delivery = failing
        .note_state_delivery(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("current-caller"),
            TurnViewState::Completed,
            "test",
        )
        .await;

    assert_eq!(delivery, TurnViewDelivery::FailedPermanent);
    assert!(!persisted_exists(target));
    assert!(!failing.targets.contains_key(&target));
    assert_eq!(failing.target_lock_count(target), 0);

    let retry = TurnViewReconciler::with_test_deliveries(vec![TurnViewDelivery::FailedPermanent]);
    let retry_delivery = retry
        .note_state_delivery(
            &shared,
            target,
            owner,
            TurnViewIdentity::Test("current-caller"),
            TurnViewState::Completed,
            "test",
        )
        .await;

    assert_eq!(retry_delivery, TurnViewDelivery::FailedPermanent);
    assert!(!persisted_exists(target));
    assert!(!retry.targets.contains_key(&target));
    assert_eq!(retry.target_lock_count(target), 0);
    assert!(
        retry.ops().iter().all(|op| !op.add),
        "cold retry after a permanent-gone target must not recreate terminal state"
    );
}

#[tokio::test]
async fn dispatch_parent_retry_transient_keeps_persisted_pending_and_retries() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = target_with(100_000_000_000_621, 100_000_000_000_622);
    clear_persisted(target);
    let owner = owner(33, "dispatch-parent-transient");
    let starter = TurnViewReconciler::default();
    starter
        .note_state(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("intake-a"),
            TurnViewState::Pending,
            "test",
        )
        .await;
    assert!(persisted_exists(target));

    let combined_status =
        super::super::reaction_lifecycle::test_parent_retry_failure_status(Some(403), None);
    let combined_delivery = TurnViewDelivery::from_reaction_error_status(combined_status);
    assert_ne!(combined_delivery, TurnViewDelivery::FailedPermanent);

    let retrying = TurnViewReconciler::with_test_deliveries(vec![
        combined_delivery,
        TurnViewDelivery::Delivered,
        TurnViewDelivery::Delivered,
        TurnViewDelivery::Delivered,
    ]);
    let delivery = retrying
        .note_state_delivery(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("current-caller"),
            TurnViewState::Completed,
            "test",
        )
        .await;

    assert_eq!(delivery, TurnViewDelivery::Failed);
    assert!(persisted_exists(target));
    assert!(!retrying.targets.contains_key(&target));
    let ops_before_retry = retrying.ops();

    let retry_delivery = retrying
        .note_state_delivery(
            &shared,
            target,
            owner,
            TurnViewIdentity::Test("current-caller"),
            TurnViewState::Completed,
            "test",
        )
        .await;

    assert_eq!(retry_delivery, TurnViewDelivery::Delivered);
    assert!(!persisted_exists(target));
    let ops_after_retry = retrying.ops();
    assert!(ops_after_retry.len() > ops_before_retry.len());
    assert_eq!(
        ops_after_retry
            .iter()
            .filter(|op| !op.add && op.emoji == '⏳')
            .count(),
        2
    );
    assert_eq!(
        ops_after_retry
            .iter()
            .filter(|op| op.add && op.emoji == '✅')
            .count(),
        2
    );
}

#[test]
fn persisted_provider_mismatch_deletes_file_and_loads_cold() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = target_with(100_000_000_000_701, 100_000_000_000_702);
    clear_persisted(target);
    let record = persisted_record(&shared, target, "codex", "pending");
    write_persisted(&record, target);

    let reconciler = TurnViewReconciler::default();
    assert!(
        reconciler
            .load_persisted_target(target, &shared, "test")
            .is_none()
    );
    assert!(!persisted_exists(target));
    assert!(!reconciler.targets.contains_key(&target));
}

#[test]
fn persisted_unknown_applied_value_deletes_file_and_loads_cold() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = target_with(100_000_000_000_711, 100_000_000_000_712);
    clear_persisted(target);
    let provider = shared.provider.as_str().to_string();
    let record = persisted_record(&shared, target, &provider, "mystery");
    write_persisted(&record, target);

    let reconciler = TurnViewReconciler::default();
    assert!(
        reconciler
            .load_persisted_target(target, &shared, "test")
            .is_none()
    );
    assert!(!persisted_exists(target));
    assert!(!reconciler.targets.contains_key(&target));
}
