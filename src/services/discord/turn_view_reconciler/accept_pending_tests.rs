//! The early ⏳ is taken over by a start, else settled back to 📬 or cleared after the handoff.

use super::*;

fn shared_at(generation: u64) -> Arc<SharedData> {
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared)
        .expect("new runtime has a unique owner")
        .restart
        .current_generation = generation;
    shared
}

fn hourglass_adds(reconciler: &TurnViewReconciler, target: TurnViewTarget) -> usize {
    reconciler
        .ops()
        .iter()
        .filter(|op| op.target == target && op.add && op.emoji == '⏳')
        .count()
}

#[tokio::test]
async fn accept_pending_start_takes_attempt_without_second_add_and_settle_is_noop() {
    let _root = scoped_runtime_root();
    let shared = shared_at(51);
    let reconciler = TurnViewReconciler::default();
    let target = target_with(100_000_000_000_801, 100_000_000_000_802);
    clear_persisted(target);

    let early = reconciler
        .note_accept_pending(
            &shared,
            target,
            TurnViewIdentity::Test("intake-a"),
            "accept",
        )
        .await
        .expect("an untouched target takes the early ⏳");
    assert_eq!(reconciler.accept_pending_recorded(target), Some(early));
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('⏳', "intake-a")]
    );

    // The start site re-notes Pending under the same message owner.
    let owner = TurnViewOwner::for_message(target.channel_id, target.message_id, 51);
    let started = reconciler
        .note_turn_started_with_attempt(
            &shared,
            target,
            owner,
            TurnViewIdentity::Test("intake-b"),
            "queued_replay_input_started",
        )
        .await;
    assert!(started.delivered());
    assert_ne!(
        started.attempt(),
        Some(early),
        "the start takes a fresh attempt"
    );
    assert_eq!(hourglass_adds(&reconciler, target), 1, "no second ⏳ add");

    let settled = reconciler
        .settle_accept_pending(&shared, target, std::future::ready(true), "settle")
        .await;
    assert_eq!(
        settled,
        AcceptSettle::Nothing,
        "a started turn keeps its ⏳"
    );
    assert_eq!(reconciler.ops().len(), 1);
    assert_eq!(reconciler.accept_pending_recorded(target), None);
    assert_eq!(persisted_applied(target), TurnViewState::Pending);
}

#[tokio::test]
async fn untaken_accept_pending_rolls_back_to_mailbox_while_owner_holds_input() {
    let _root = scoped_runtime_root();
    let shared = shared_at(52);
    let reconciler = TurnViewReconciler::default();
    let target = target_with(100_000_000_000_811, 100_000_000_000_812);
    clear_persisted(target);

    reconciler
        .note_accept_pending(
            &shared,
            target,
            TurnViewIdentity::Test("intake-a"),
            "accept",
        )
        .await
        .expect("early ⏳");
    let settled = reconciler
        .settle_accept_pending(&shared, target, std::future::ready(true), "settle")
        .await;

    assert_eq!(settled, AcceptSettle::Settled);
    let ops = reconciler.ops();
    assert_eq!(ops.len(), 3);
    assert!(!ops[1].add && ops[1].emoji == '⏳');
    assert!(ops[2].add && ops[2].emoji == '📬');
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('📬', "intake-a")]
    );
    assert_eq!(persisted_applied(target), TurnViewState::Queued);
}

#[tokio::test]
async fn untaken_accept_pending_clears_once_input_left_the_queue() {
    let _root = scoped_runtime_root();
    let shared = shared_at(53);
    let reconciler = TurnViewReconciler::default();
    let target = target_with(100_000_000_000_821, 100_000_000_000_822);
    clear_persisted(target);

    reconciler
        .note_accept_pending(
            &shared,
            target,
            TurnViewIdentity::Test("intake-a"),
            "accept",
        )
        .await
        .expect("early ⏳");
    let settled = reconciler
        .settle_accept_pending(&shared, target, std::future::ready(false), "settle")
        .await;

    assert_eq!(settled, AcceptSettle::Settled);
    assert_eq!(snapshot_reactions(&reconciler, target), Vec::new());
    assert!(!persisted_exists(target));
    assert!(!reconciler.targets.contains_key(&target));
}

#[tokio::test]
async fn accept_pending_leaves_an_existing_turn_view_alone() {
    let _root = scoped_runtime_root();
    let shared = shared_at(54);
    let reconciler = TurnViewReconciler::default();
    let target = target_with(100_000_000_000_831, 100_000_000_000_832);
    clear_persisted(target);
    let owner = TurnViewOwner::for_message(target.channel_id, target.message_id, 54);
    reconciler
        .note_queue_marker_added(
            &shared,
            target,
            owner,
            TurnViewIdentity::Test("intake-a"),
            '📬',
            "queued",
        )
        .await;

    let early = reconciler
        .note_accept_pending(
            &shared,
            target,
            TurnViewIdentity::Test("intake-a"),
            "accept",
        )
        .await;

    assert_eq!(early, None);
    assert_eq!(hourglass_adds(&reconciler, target), 0);
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('📬', "intake-a")]
    );
}

#[tokio::test]
async fn restart_leftover_accept_pending_settles_under_the_new_generation() {
    let _root = scoped_runtime_root();
    let target = target_with(100_000_000_000_841, 100_000_000_000_842);
    clear_persisted(target);
    let before = TurnViewReconciler::default();
    before
        .note_accept_pending(
            &shared_at(60),
            target,
            TurnViewIdentity::Test("intake-a"),
            "accept",
        )
        .await
        .expect("early ⏳ before the restart");

    // The restart loses the in-memory record; the persisted Pending stays.
    let after = TurnViewReconciler::default();
    let settled = after
        .settle_accept_pending(&shared_at(61), target, std::future::ready(false), "settle")
        .await;

    assert_eq!(settled, AcceptSettle::Settled);
    let ops = after.ops();
    assert_eq!(ops.len(), 1);
    assert!(!ops[0].add && ops[0].emoji == '⏳');
    assert!(!persisted_exists(target));
}

#[tokio::test]
async fn restart_leftover_taken_over_by_a_new_start_is_not_settled() {
    let _root = scoped_runtime_root();
    let target = target_with(100_000_000_000_851, 100_000_000_000_852);
    clear_persisted(target);
    TurnViewReconciler::default()
        .note_accept_pending(
            &shared_at(62),
            target,
            TurnViewIdentity::Test("intake-a"),
            "accept",
        )
        .await
        .expect("early ⏳ before the restart");

    let shared = shared_at(63);
    let after = TurnViewReconciler::default();
    let owner = TurnViewOwner::for_message(target.channel_id, target.message_id, 63);
    after
        .note_turn_started_with_attempt(
            &shared,
            target,
            owner,
            TurnViewIdentity::Test("intake-a"),
            "queued_replay_input_started",
        )
        .await;
    let settled = after
        .settle_accept_pending(&shared, target, std::future::ready(false), "settle")
        .await;

    assert_eq!(settled, AcceptSettle::Nothing);
    assert_eq!(
        after.ops().len(),
        0,
        "the transfer reuses the ⏳ and nothing is removed"
    );
    assert_eq!(persisted_applied(target), TurnViewState::Pending);
}

#[tokio::test]
async fn ordinary_start_without_accept_pending_is_never_settled() {
    let _root = scoped_runtime_root();
    let shared = shared_at(64);
    let reconciler = TurnViewReconciler::default();
    let target = target_with(100_000_000_000_861, 100_000_000_000_862);
    clear_persisted(target);
    let owner = TurnViewOwner::for_message(target.channel_id, target.message_id, 64);
    reconciler
        .note_turn_started_with_attempt(
            &shared,
            target,
            owner,
            TurnViewIdentity::Test("intake-a"),
            "intake_start",
        )
        .await;

    let settled = reconciler
        .settle_accept_pending(&shared, target, std::future::ready(false), "settle")
        .await;

    assert_eq!(settled, AcceptSettle::Nothing);
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('⏳', "intake-a")]
    );
}

fn persisted_accept(target: TurnViewTarget) -> Option<u64> {
    let text = std::fs::read_to_string(persisted_path(target)).expect("persisted turn view state");
    let record: PersistedTargetState =
        serde_json::from_str(&text).expect("parse persisted turn view state");
    record.accept_attempt_id
}

#[tokio::test]
async fn the_early_mark_is_persisted_until_a_start_takes_the_attempt() {
    let _root = scoped_runtime_root();
    let shared = shared_at(70);
    let reconciler = TurnViewReconciler::default();
    let target = target_with(100_000_000_000_871, 100_000_000_000_872);
    clear_persisted(target);

    let early = reconciler
        .note_accept_pending(
            &shared,
            target,
            TurnViewIdentity::Test("intake-a"),
            "accept",
        )
        .await
        .expect("early ⏳");
    assert_eq!(persisted_accept(target), Some(early.get()));

    let owner = TurnViewOwner::for_message(target.channel_id, target.message_id, 70);
    reconciler
        .note_turn_started_with_attempt(
            &shared,
            target,
            owner,
            TurnViewIdentity::Test("intake-a"),
            "intake_start",
        )
        .await;
    assert_eq!(
        persisted_accept(target),
        None,
        "a started turn is no early ⏳"
    );
}

/// After a restart only a persisted early mark is settled; an ordinary start's Pending of
/// the older generation stays.
#[tokio::test]
async fn an_older_generations_ordinary_start_is_never_settled_after_restart() {
    let _root = scoped_runtime_root();
    let target = target_with(100_000_000_000_881, 100_000_000_000_882);
    clear_persisted(target);
    let owner = TurnViewOwner::for_message(target.channel_id, target.message_id, 71);
    TurnViewReconciler::default()
        .note_turn_started_with_attempt(
            &shared_at(71),
            target,
            owner,
            TurnViewIdentity::Test("intake-a"),
            "intake_start",
        )
        .await;

    let after = TurnViewReconciler::default();
    let settled = after
        .settle_accept_pending(&shared_at(72), target, std::future::ready(false), "settle")
        .await;

    assert_eq!(settled, AcceptSettle::Nothing);
    assert_eq!(after.ops().len(), 0);
    assert_eq!(persisted_applied(target), TurnViewState::Pending);
}

/// A recoverable clear failure keeps the record and the retry within the same settle
/// clears the ⏳.
#[tokio::test]
async fn a_failed_clear_is_retried_until_it_lands() {
    let _root = scoped_runtime_root();
    let shared = shared_at(73);
    let reconciler = TurnViewReconciler::with_test_deliveries(vec![
        TurnViewDelivery::Delivered,
        TurnViewDelivery::Failed,
    ]);
    let target = target_with(100_000_000_000_891, 100_000_000_000_892);
    clear_persisted(target);
    reconciler
        .note_accept_pending(
            &shared,
            target,
            TurnViewIdentity::Test("intake-a"),
            "accept",
        )
        .await
        .expect("early ⏳");

    let settled = reconciler
        .settle_accept_pending(&shared, target, std::future::ready(false), "settle")
        .await;

    assert_eq!(settled, AcceptSettle::Settled);
    assert_eq!(snapshot_reactions(&reconciler, target), Vec::new());
    assert_eq!(reconciler.accept_pending_recorded(target), None);
    assert!(!persisted_exists(target));
}

/// Every try failing keeps the record, so a later settle still clears; a rollback whose
/// first removal failed lands on the retry.
#[tokio::test]
async fn a_settle_that_keeps_failing_leaves_the_record_for_the_next() {
    let _root = scoped_runtime_root();
    let shared = shared_at(74);
    let reconciler = TurnViewReconciler::with_test_deliveries(vec![
        TurnViewDelivery::Delivered,
        TurnViewDelivery::Failed,
        TurnViewDelivery::Failed,
        TurnViewDelivery::Failed,
    ]);
    let target = target_with(100_000_000_000_901, 100_000_000_000_902);
    clear_persisted(target);
    let early = reconciler
        .note_accept_pending(
            &shared,
            target,
            TurnViewIdentity::Test("intake-a"),
            "accept",
        )
        .await
        .expect("early ⏳");

    let kept = reconciler
        .settle_accept_pending(&shared, target, std::future::ready(true), "settle")
        .await;
    assert_eq!(kept, AcceptSettle::Kept);
    assert_eq!(reconciler.accept_pending_recorded(target), Some(early));
    assert_eq!(persisted_applied(target), TurnViewState::Pending);

    let settled = reconciler
        .settle_accept_pending(&shared, target, std::future::ready(true), "settle")
        .await;
    assert_eq!(settled, AcceptSettle::Settled);
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('📬', "intake-a")]
    );
}

/// A start that took the attempt before the retry keeps its ⏳.
#[tokio::test]
async fn a_start_before_the_retry_keeps_its_hourglass() {
    let _root = scoped_runtime_root();
    let shared = shared_at(75);
    let reconciler = TurnViewReconciler::with_test_deliveries(vec![
        TurnViewDelivery::Delivered,
        TurnViewDelivery::Failed,
        TurnViewDelivery::Failed,
        TurnViewDelivery::Failed,
    ]);
    let target = target_with(100_000_000_000_911, 100_000_000_000_912);
    clear_persisted(target);
    reconciler
        .note_accept_pending(
            &shared,
            target,
            TurnViewIdentity::Test("intake-a"),
            "accept",
        )
        .await
        .expect("early ⏳");
    let kept = reconciler
        .settle_accept_pending(&shared, target, std::future::ready(false), "settle")
        .await;
    assert_eq!(kept, AcceptSettle::Kept);

    let owner = TurnViewOwner::for_message(target.channel_id, target.message_id, 75);
    reconciler
        .note_turn_started_with_attempt(
            &shared,
            target,
            owner,
            TurnViewIdentity::Test("intake-a"),
            "queued_replay_input_started",
        )
        .await;
    let settled = reconciler
        .settle_accept_pending(&shared, target, std::future::ready(false), "settle")
        .await;

    assert_eq!(settled, AcceptSettle::Nothing);
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('⏳', "intake-a")]
    );
}

/// A rollback whose 📬 add failed puts the ⏳ back; the retry rolls it back for good.
#[tokio::test]
async fn a_rollback_whose_mailbox_add_failed_is_retried() {
    let _root = scoped_runtime_root();
    let shared = shared_at(76);
    let reconciler = TurnViewReconciler::with_test_deliveries(vec![
        TurnViewDelivery::Delivered,
        TurnViewDelivery::Delivered,
        TurnViewDelivery::Failed,
        TurnViewDelivery::Delivered,
    ]);
    let target = target_with(100_000_000_000_921, 100_000_000_000_922);
    clear_persisted(target);
    reconciler
        .note_accept_pending(
            &shared,
            target,
            TurnViewIdentity::Test("intake-a"),
            "accept",
        )
        .await
        .expect("early ⏳");

    let settled = reconciler
        .settle_accept_pending(&shared, target, std::future::ready(true), "settle")
        .await;

    assert_eq!(settled, AcceptSettle::Settled);
    let ops = reconciler.ops();
    assert_eq!(
        ops.len(),
        6,
        "add, remove, failed 📬, compensating ⏳, remove, 📬"
    );
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('📬', "intake-a")]
    );
    assert_eq!(persisted_applied(target), TurnViewState::Queued);
}
