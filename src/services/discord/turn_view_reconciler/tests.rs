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

fn persisted_applied(target: TurnViewTarget) -> TurnViewState {
    let text = std::fs::read_to_string(persisted_path(target)).expect("persisted turn view state");
    let record: PersistedTargetState =
        serde_json::from_str(&text).expect("parse persisted turn view state");
    TurnViewState::from_str(&record.applied).expect("known persisted turn view state")
}

fn persisted_start_attempt(target: TurnViewTarget) -> Option<TurnStartAttempt> {
    let text = std::fs::read_to_string(persisted_path(target)).expect("persisted turn view state");
    let record: PersistedTargetState =
        serde_json::from_str(&text).expect("parse persisted turn view state");
    record.start_attempt_id.map(TurnStartAttempt)
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
        start_attempt_id: None,
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

/// Isolate each reconciler test from the process-global `AGENTDESK_ROOT_DIR`.
/// S4-a2 gave the reconciler a persisted target store (`persist_target` /
/// `load_persisted_target`), so every `note_state` transition now resolves the
/// runtime-store root; under the #3293 guard a live/unset `AGENTDESK_ROOT_DIR`
/// falls back to a shared throwaway tempdir (#4514), so per-test isolation still
/// requires installing a test root. Acquire the crate-wide env lock (the same
/// `config::shared_test_env_lock()` every other env-mutating test serializes
/// on) for the FULL test scope, point the env at a private temp dir, and
/// restore the prior value on drop. The `MutexGuard` is held across the test's
/// `.await` points; that is sound because reconciler tests run on the default
/// current-thread `#[tokio::test]` runtime, so the future never moves threads.
struct ScopedRuntimeRoot {
    _lock: std::sync::MutexGuard<'static, ()>,
    _temp: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
}

impl Drop for ScopedRuntimeRoot {
    fn drop(&mut self) {
        unsafe {
            match self.prev.take() {
                Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
            }
        }
    }
}

#[must_use]
fn scoped_runtime_root() -> ScopedRuntimeRoot {
    let lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
    let temp = tempfile::tempdir().expect("create temp runtime dir for reconciler test");
    unsafe {
        std::env::set_var(
            "AGENTDESK_ROOT_DIR",
            temp.path().to_str().expect("temp path must be valid utf-8"),
        );
    }
    ScopedRuntimeRoot {
        _lock: lock,
        _temp: temp,
        prev,
    }
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
    let _root = scoped_runtime_root();
    let reconciler = note_sequence(&[TurnViewState::Pending, TurnViewState::Completed]).await;

    assert_eq!(
        snapshot_reactions(&reconciler, target()),
        vec![expected('✅', "intake-a")]
    );
}

#[tokio::test]
async fn queued_then_started_swaps_mailbox_to_hourglass_without_residue() {
    let _root = scoped_runtime_root();
    let reconciler = note_sequence(&[TurnViewState::Queued, TurnViewState::Pending]).await;

    assert_eq!(
        snapshot_reactions(&reconciler, target()),
        vec![expected('⏳', "intake-a")]
    );
    let ops = reconciler.ops();
    assert!(ops.iter().any(|op| op.add && op.emoji == '📬'));
    assert!(ops.iter().any(|op| !op.add && op.emoji == '📬'));
    assert!(
        !snapshot_reactions(&reconciler, target())
            .iter()
            .any(|(emoji, _)| *emoji == '📬')
    );
}

#[tokio::test]
async fn requeue_renotification_of_queued_target_is_coalesced_noop() {
    let _root = scoped_runtime_root();
    let reconciler = note_sequence(&[TurnViewState::Queued, TurnViewState::Queued]).await;

    let ops = reconciler.ops();
    assert_eq!(ops.iter().filter(|op| op.emoji == '📬').count(), 1);
    assert_eq!(
        snapshot_reactions(&reconciler, target()),
        vec![expected('📬', "intake-a")]
    );
}

#[tokio::test]
async fn queue_cancel_removes_mailbox_marker() {
    let _root = scoped_runtime_root();
    let reconciler = note_sequence(&[TurnViewState::Queued, TurnViewState::None]).await;

    let ops = reconciler.ops();
    assert!(ops.iter().any(|op| op.add && op.emoji == '📬'));
    assert!(ops.iter().any(|op| !op.add && op.emoji == '📬'));
    assert_eq!(snapshot_reactions(&reconciler, target()), Vec::new());
}

#[tokio::test]
async fn regression_4109_pre_migration_untracked_queue_markers_still_remove_reaction() {
    let _root = scoped_runtime_root();
    let shared = crate::services::discord::make_shared_data_for_tests();

    for (index, emoji) in ['➕', '🔄'].into_iter().enumerate() {
        let reconciler = TurnViewReconciler::default();
        let target = target_with(
            100_000_000_000_131 + index as u64,
            100_000_000_000_135 + index as u64,
        );
        clear_persisted(target);

        let delivered = reconciler
            .note_queue_marker_removed(
                &shared,
                target,
                owner(35 + index as u64, "pre-migration-untracked"),
                TurnViewIdentity::Test("fallback-caller"),
                emoji,
                "test_pre_migration_untracked_queue_marker_clear",
            )
            .await;

        assert!(delivered);
        assert_eq!(
            reconciler.ops(),
            vec![TestReactionOp {
                target,
                emoji,
                add: false,
                identity: "fallback-caller".to_string(),
            }]
        );
        assert!(!persisted_exists(target));
        assert!(!reconciler.targets.contains_key(&target));
        assert_eq!(reconciler.target_lock_count(target), 0);
    }
}

#[tokio::test]
async fn regression_4049_start_rollback_to_queued_swaps_hourglass_to_mailbox_and_cancel_cleans() {
    let _root = scoped_runtime_root();
    let reconciler = TurnViewReconciler::default();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = target_with(100_000_000_000_141, 100_000_000_000_142);
    clear_persisted(target);
    let owner = owner(37, "rollback");

    let start_attempt = reconciler
        .note_turn_started_with_attempt(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("intake-a"),
            "test_seed_pending",
        )
        .await
        .attempt()
        .expect("pending start records an attempt");
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('⏳', "intake-a")]
    );
    assert_eq!(persisted_applied(target), TurnViewState::Pending);

    reconciler
        .note_start_rolled_back_to_queued(
            &shared,
            target,
            owner.clone(),
            start_attempt,
            "test_start_rolled_back_to_queued",
        )
        .await;

    let ops = reconciler.ops();
    assert_eq!(ops.len(), 3);
    assert!(ops[1].emoji == '⏳' && !ops[1].add);
    assert!(ops[2].emoji == '📬' && ops[2].add);
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('📬', "intake-a")]
    );
    assert_eq!(
        reconciler
            .targets
            .get(&target)
            .expect("rolled back target should remain tracked")
            .applied,
        TurnViewState::Queued
    );
    assert_eq!(persisted_applied(target), TurnViewState::Queued);

    reconciler
        .note_queue_marker_removed(
            &shared,
            target,
            owner,
            TurnViewIdentity::Test("ignored-cancel"),
            '📬',
            "test_queue_exit_after_rollback",
        )
        .await;

    assert_eq!(snapshot_reactions(&reconciler, target), Vec::new());
    assert!(!persisted_exists(target));
    assert!(!reconciler.targets.contains_key(&target));
}

#[tokio::test]
async fn regression_4049_same_generation_redispatch_stale_start_rollback_noop() {
    let _root = scoped_runtime_root();
    let reconciler = TurnViewReconciler::default();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = target_with(100_000_000_000_145, 100_000_000_000_146);
    clear_persisted(target);
    let owner = owner(40, "same-generation-redispatch");

    let attempt1 = reconciler
        .note_turn_started_with_attempt(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("intake-a"),
            "test_seed_attempt1",
        )
        .await
        .attempt()
        .expect("attempt1 recorded");
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('⏳', "intake-a")]
    );
    let ops_after_attempt1 = reconciler.ops().len();

    let attempt2 = reconciler
        .note_turn_started_with_attempt(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("intake-a"),
            "test_redispatch_attempt2",
        )
        .await
        .attempt()
        .expect("attempt2 recorded");
    assert_ne!(
        attempt1, attempt2,
        "same-generation re-dispatch must mint a new start attempt"
    );
    assert_eq!(
        reconciler.ops().len(),
        ops_after_attempt1,
        "same-state re-dispatch only refreshes identity, not emoji"
    );

    reconciler
        .note_start_rolled_back_to_queued(
            &shared,
            target,
            owner,
            attempt1,
            "test_delayed_attempt1_rollback",
        )
        .await;

    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('⏳', "intake-a")],
        "stale rollback for attempt1 must not clobber attempt2 pending"
    );
    let current = reconciler
        .targets
        .get(&target)
        .expect("redispatched pending target should remain tracked");
    assert_eq!(current.applied, TurnViewState::Pending);
    assert_eq!(current.start_attempt, Some(attempt2));
    assert_eq!(persisted_applied(target), TurnViewState::Pending);
}

#[tokio::test]
async fn regression_4049_attempt_scoped_clear_removes_matching_pending() {
    let _root = scoped_runtime_root();
    let reconciler = TurnViewReconciler::default();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = target_with(100_000_000_000_147, 100_000_000_000_148);
    clear_persisted(target);
    let owner = owner(42, "matching-clear");

    let attempt = reconciler
        .note_turn_started_with_attempt(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("intake-a"),
            "test_seed_pending",
        )
        .await
        .attempt()
        .expect("pending start records an attempt");
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('⏳', "intake-a")]
    );

    reconciler
        .note_turn_cleared_if_attempt_matches(
            &shared,
            target,
            owner,
            TurnViewIdentity::Test("ignored-clear"),
            attempt,
            "test_matching_attempt_clear",
        )
        .await;

    let ops = reconciler.ops();
    assert!(ops.iter().any(|op| !op.add && op.emoji == '⏳'));
    assert_eq!(snapshot_reactions(&reconciler, target), Vec::new());
    assert!(!persisted_exists(target));
    assert!(!reconciler.targets.contains_key(&target));
}

async fn assert_stale_attempt_clear_keeps_redispatch_pending(
    target: TurnViewTarget,
    owner_suffix: &str,
    source: &'static str,
) {
    let reconciler = TurnViewReconciler::default();
    let shared = crate::services::discord::make_shared_data_for_tests();
    clear_persisted(target);
    let owner = owner(43, owner_suffix);

    let attempt1 = reconciler
        .note_turn_started_with_attempt(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("intake-a"),
            "test_seed_attempt1",
        )
        .await
        .attempt()
        .expect("attempt1 recorded");
    let attempt2 = reconciler
        .note_turn_started_with_attempt(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("intake-a"),
            "test_redispatch_attempt2",
        )
        .await
        .attempt()
        .expect("attempt2 recorded");
    assert_ne!(attempt1, attempt2);
    let ops_after_attempt2 = reconciler.ops().len();

    reconciler
        .note_turn_cleared_if_attempt_matches(
            &shared,
            target,
            owner,
            TurnViewIdentity::Test("ignored-stale-clear"),
            attempt1,
            source,
        )
        .await;

    assert_eq!(
        reconciler.ops().len(),
        ops_after_attempt2,
        "stale attempt-scoped clear must not touch Discord"
    );
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('⏳', "intake-a")],
        "stale clear for attempt1 must not clobber attempt2 pending"
    );
    let current = reconciler
        .targets
        .get(&target)
        .expect("redispatched pending target should remain tracked");
    assert_eq!(current.applied, TurnViewState::Pending);
    assert_eq!(current.start_attempt, Some(attempt2));
    assert_eq!(persisted_applied(target), TurnViewState::Pending);
    assert_eq!(persisted_start_attempt(target), Some(attempt2));
}

#[tokio::test]
async fn regression_4049_dispatch_already_running_stale_clear_keeps_redispatch_pending() {
    let _root = scoped_runtime_root();
    assert_stale_attempt_clear_keeps_redispatch_pending(
        target_with(100_000_000_000_149, 100_000_000_000_150),
        "dispatch-already-running-clear",
        "race_loss_orphan_placeholder",
    )
    .await;
}

#[tokio::test]
async fn regression_4049_final_race_loss_stale_clear_keeps_redispatch_pending() {
    let _root = scoped_runtime_root();
    assert_stale_attempt_clear_keeps_redispatch_pending(
        target_with(100_000_000_000_153, 100_000_000_000_154),
        "final-race-loss-clear",
        "race_loss_message_queued",
    )
    .await;
}

#[tokio::test]
async fn regression_4049_stale_clear_after_newer_rollback_keeps_queued_marker() {
    let _root = scoped_runtime_root();
    let reconciler = TurnViewReconciler::default();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = target_with(100_000_000_000_155, 100_000_000_000_156);
    clear_persisted(target);
    let owner = owner(45, "stale-clear-after-rollback");

    let attempt1 = reconciler
        .note_turn_started_with_attempt(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("intake-a"),
            "test_seed_attempt1",
        )
        .await
        .attempt()
        .expect("attempt1 recorded");
    let attempt2 = reconciler
        .note_turn_started_with_attempt(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("intake-a"),
            "test_redispatch_attempt2",
        )
        .await
        .attempt()
        .expect("attempt2 recorded");
    assert_ne!(attempt1, attempt2);

    reconciler
        .note_start_rolled_back_to_queued(
            &shared,
            target,
            owner.clone(),
            attempt2,
            "test_attempt2_rollback_to_queued",
        )
        .await;

    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('📬', "intake-a")]
    );
    let current = reconciler
        .targets
        .get(&target)
        .expect("rolled-back queued target should remain tracked");
    assert_eq!(current.applied, TurnViewState::Queued);
    assert_eq!(current.start_attempt, None);
    drop(current);
    assert_eq!(persisted_applied(target), TurnViewState::Queued);
    assert_eq!(persisted_start_attempt(target), None);
    let ops_after_rollback = reconciler.ops().len();

    reconciler
        .note_turn_cleared_if_attempt_matches(
            &shared,
            target,
            owner,
            TurnViewIdentity::Test("ignored-stale-clear"),
            attempt1,
            "test_delayed_attempt1_clear_after_attempt2_queued",
        )
        .await;

    assert_eq!(
        reconciler.ops().len(),
        ops_after_rollback,
        "stale attempt-scoped clear must not touch Discord after rollback to queued"
    );
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('📬', "intake-a")],
        "stale attempt1 clear must not remove the newer queued marker"
    );
    let current = reconciler
        .targets
        .get(&target)
        .expect("queued target should survive stale attempt-scoped clear");
    assert_eq!(current.applied, TurnViewState::Queued);
    assert_eq!(current.start_attempt, None);
    assert_eq!(persisted_applied(target), TurnViewState::Queued);
    assert_eq!(persisted_start_attempt(target), None);
}

#[tokio::test]
async fn regression_4049_attempt_scoped_clear_preserves_pending_without_start_attempt() {
    let _root = scoped_runtime_root();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = target_with(100_000_000_000_157, 100_000_000_000_158);
    clear_persisted(target);
    let provider = shared.provider.as_str().to_string();
    let record = persisted_record(&shared, target, &provider, "pending");
    write_persisted(&record, target);

    let reconciler = TurnViewReconciler::default();
    reconciler
        .note_turn_cleared_if_attempt_matches(
            &shared,
            target,
            owner(91, "persisted"),
            TurnViewIdentity::Test("ignored-clear"),
            TurnStartAttempt(99),
            "test_pending_without_start_attempt_clear",
        )
        .await;

    assert!(
        reconciler.ops().is_empty(),
        "attempt-scoped clear without a matching pending nonce must not touch Discord"
    );
    let current = reconciler
        .targets
        .get(&target)
        .expect("pending target without nonce should survive attempt-scoped clear");
    assert_eq!(current.applied, TurnViewState::Pending);
    assert_eq!(current.start_attempt, None);
    assert_eq!(persisted_applied(target), TurnViewState::Pending);
    assert_eq!(persisted_start_attempt(target), None);
}

#[tokio::test]
async fn regression_4049_late_start_rollback_after_terminal_is_noop() {
    let _root = scoped_runtime_root();
    let reconciler = TurnViewReconciler::default();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = target_with(100_000_000_000_143, 100_000_000_000_144);
    clear_persisted(target);
    let owner = owner(39, "terminal-rollback");

    let start_attempt = reconciler
        .note_turn_started_with_attempt(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("intake-a"),
            "test_seed_pending",
        )
        .await
        .attempt()
        .expect("pending start records an attempt");
    reconciler
        .note_state(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("ignored-terminal"),
            TurnViewState::Completed,
            "test_terminal_before_rollback",
        )
        .await;
    let ops_after_terminal = reconciler.ops().len();
    let reactions_after_terminal = snapshot_reactions(&reconciler, target);

    reconciler
        .note_start_rolled_back_to_queued(
            &shared,
            target,
            owner,
            start_attempt,
            "test_late_start_rolled_back_to_queued",
        )
        .await;

    assert_eq!(
        reconciler.ops().len(),
        ops_after_terminal,
        "late rollback after terminal must not touch Discord"
    );
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        reactions_after_terminal
    );
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('✅', "intake-a")]
    );
    assert!(!persisted_exists(target));
    assert!(!reconciler.targets.contains_key(&target));
}

#[tokio::test]
async fn regression_4049_late_queued_after_started_and_cancel_are_noops() {
    let _root = scoped_runtime_root();
    let reconciler = TurnViewReconciler::default();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = target_with(100_000_000_000_151, 100_000_000_000_152);
    clear_persisted(target);
    let owner = owner(41, "late-queued");

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
    let ops_after_start = reconciler.ops().len();

    reconciler
        .note_state(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("ignored-late-queued"),
            TurnViewState::Queued,
            "test",
        )
        .await;

    assert_eq!(
        reconciler.ops().len(),
        ops_after_start,
        "late queued notification after start must not touch Discord"
    );
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('⏳', "intake-a")]
    );
    assert!(persisted_exists(target));
    assert_eq!(
        reconciler
            .targets
            .get(&target)
            .expect("pending target should remain tracked")
            .applied,
        TurnViewState::Pending
    );

    reconciler
        .note_queue_marker_removed(
            &shared,
            target,
            owner,
            TurnViewIdentity::Test("ignored-cancel"),
            '📬',
            "test",
        )
        .await;

    assert_eq!(
        reconciler.ops().len(),
        ops_after_start,
        "queued cancel after start must not touch Discord"
    );
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('⏳', "intake-a")]
    );
    assert!(persisted_exists(target));
    assert_eq!(
        reconciler
            .targets
            .get(&target)
            .expect("pending target should survive queued cancel")
            .applied,
        TurnViewState::Pending
    );
}

#[tokio::test]
async fn queued_cancel_ignores_nonmatching_generation() {
    let _root = scoped_runtime_root();
    let reconciler = TurnViewReconciler::default();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = target_with(100_000_000_000_161, 100_000_000_000_162);
    clear_persisted(target);
    let queued_owner = owner(43, "queued");
    let stale_cancel_owner = owner(44, "queued");

    reconciler
        .note_state(
            &shared,
            target,
            queued_owner,
            TurnViewIdentity::Test("intake-a"),
            TurnViewState::Queued,
            "test",
        )
        .await;
    let ops_after_queue = reconciler.ops().len();

    reconciler
        .note_queue_marker_removed(
            &shared,
            target,
            stale_cancel_owner,
            TurnViewIdentity::Test("ignored-cancel"),
            '📬',
            "test",
        )
        .await;

    assert_eq!(reconciler.ops().len(), ops_after_queue);
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('📬', "intake-a")]
    );
    assert!(persisted_exists(target));
    assert_eq!(
        reconciler
            .targets
            .get(&target)
            .expect("queued target should survive stale cancel")
            .applied,
        TurnViewState::Queued
    );
}

#[tokio::test]
async fn sequence_start_fail_leaves_only_failed_reaction() {
    let _root = scoped_runtime_root();
    let reconciler = note_sequence(&[TurnViewState::Pending, TurnViewState::Failed]).await;

    assert_eq!(
        snapshot_reactions(&reconciler, target()),
        vec![expected('⚠', "intake-a")]
    );
}

#[tokio::test]
async fn sequence_start_stop_leaves_only_stopped_reaction() {
    let _root = scoped_runtime_root();
    let reconciler = note_sequence(&[TurnViewState::Pending, TurnViewState::Stopped]).await;

    assert_eq!(
        snapshot_reactions(&reconciler, target()),
        vec![expected('🛑', "intake-a")]
    );
}

#[tokio::test]
async fn sequence_start_recover_complete_removes_hourglass_residue() {
    let _root = scoped_runtime_root();
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
    let _root = scoped_runtime_root();
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
    let _root = scoped_runtime_root();
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
    let _root = scoped_runtime_root();
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
    let _root = scoped_runtime_root();
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
    let _root = scoped_runtime_root();
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
async fn regression_4049_late_queued_after_terminal_eviction_is_noop() {
    let _root = scoped_runtime_root();
    let reconciler = TurnViewReconciler::default();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = target_with(100_000_000_000_471, 100_000_000_000_472);
    clear_persisted(target);
    let owner = owner(21, "terminal-late-queued");

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
    reconciler
        .note_state(
            &shared,
            target,
            owner.clone(),
            TurnViewIdentity::Test("ignored-terminal"),
            TurnViewState::Completed,
            "test",
        )
        .await;

    assert!(!persisted_exists(target));
    assert!(!reconciler.targets.contains_key(&target));
    let ops_after_terminal = reconciler.ops().len();

    reconciler
        .note_state(
            &shared,
            target,
            owner,
            TurnViewIdentity::Test("ignored-late-queued"),
            TurnViewState::Queued,
            "test_delayed_note_message_queued",
        )
        .await;

    assert_eq!(
        reconciler.ops().len(),
        ops_after_terminal,
        "delayed queued notification after terminal eviction must not touch Discord"
    );
    assert!(
        !snapshot_reactions(&reconciler, target)
            .iter()
            .any(|(emoji, _)| *emoji == '📬'),
        "late queued notification must not re-add mailbox next to the terminal reaction"
    );
    assert!(
        !persisted_exists(target),
        "late queued notification must not recreate persisted reconciler state"
    );
}

#[tokio::test]
async fn regression_3303_success_path_leaves_no_hourglass_residue() {
    let _root = scoped_runtime_root();
    let reconciler = note_sequence(&[TurnViewState::Pending, TurnViewState::Completed]).await;

    assert!(
        !snapshot_reactions(&reconciler, target())
            .iter()
            .any(|(emoji, _)| *emoji == '⏳')
    );
}

#[tokio::test]
async fn concurrent_terminal_notifications_leave_exactly_one_terminal_reaction() {
    let _root = scoped_runtime_root();
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
    let _root = scoped_runtime_root();
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
    let _root = scoped_runtime_root();
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
    let _root = scoped_runtime_root();
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
    let _root = scoped_runtime_root();
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
    let _root = scoped_runtime_root();
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
    let _root = scoped_runtime_root();
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
