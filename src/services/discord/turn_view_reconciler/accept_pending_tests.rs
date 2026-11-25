//! #8398: the early ⏳ an idle owner puts on an accepted input, taken over by a start or settled
//! back to 📬 or cleared once its handoff returned without one.

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
    assert!(!settled, "a started turn keeps its ⏳");
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

    assert!(settled);
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

    assert!(settled);
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

    assert!(settled);
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

    assert!(!settled);
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

    assert!(!settled);
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('⏳', "intake-a")]
    );
}
