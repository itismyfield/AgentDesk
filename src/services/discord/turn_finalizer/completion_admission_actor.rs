use super::completion_admission::{CompletionAdmission, publish_claimed_queue_eligible};
use super::*;

pub(super) fn handle_completion_admission_message(
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
    pending_admission: &mut HashMap<LedgerKey, PendingCompletionAdmission>,
    msg: FinalizeMsg,
) {
    match msg {
        FinalizeMsg::MailboxReleased { key, shared } => {
            update_completion_admission(ledger, pending_admission, key, &shared, |admission| {
                admission.note_mailbox_released();
            });
        }
        FinalizeMsg::TerminalProjectionSettled {
            key,
            allow_queue,
            shared,
        } => {
            update_completion_admission(ledger, pending_admission, key, &shared, |admission| {
                admission.note_terminal_projection_settled(allow_queue);
            });
        }
        FinalizeMsg::TerminalDispositionSettled {
            key,
            allow_queue,
            shared,
        } => {
            update_completion_admission(ledger, pending_admission, key, &shared, |admission| {
                admission.note_terminal_disposition_settled(allow_queue);
            });
        }
        _ => unreachable!("completion-admission dispatcher received another message"),
    }
}

fn canonical_admission_ledger_key(
    ledger: &HashMap<LedgerKey, LedgerEntry>,
    pending_admission: &HashMap<LedgerKey, PendingCompletionAdmission>,
    key: TurnKey,
) -> LedgerKey {
    if key.user_msg_id != 0 {
        let exact_key = key.exact_key();
        if ledger.contains_key(&exact_key) || pending_admission.contains_key(&exact_key) {
            return exact_key;
        }
        if let Some((ledger_key, _)) = ledger
            .iter()
            .filter(|(ledger_key, entry)| {
                ledger_key.channel_id == key.channel_id
                    && ledger_key.user_msg_id == key.user_msg_id
                    && entry.turn_key.user_msg_id == key.user_msg_id
            })
            .max_by_key(|(ledger_key, entry)| {
                (entry.phase != Phase::Finalized, ledger_key.generation)
            })
        {
            return *ledger_key;
        }
        // Pre-ledger edges remain generation-bound. Only an existing ledger is
        // stable logical-turn authority for reconciling a late generation edge;
        // otherwise an older pending edge could pre-settle a newer generation.
        return exact_key;
    }
    resolve_ledger_key(ledger, key)
}

pub(super) fn take_exact_pending_completion_admission(
    pending_admission: &mut HashMap<LedgerKey, PendingCompletionAdmission>,
    ledger_key: LedgerKey,
) -> Option<PendingCompletionAdmission> {
    pending_admission.remove(&ledger_key)
}

fn update_completion_admission(
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
    pending_admission: &mut HashMap<LedgerKey, PendingCompletionAdmission>,
    key: TurnKey,
    shared: &SharedData,
    update: impl FnOnce(&mut CompletionAdmission),
) {
    let ledger_key = canonical_admission_ledger_key(ledger, pending_admission, key);
    if let Some(entry) = ledger.get_mut(&ledger_key) {
        update(&mut entry.completion_admission);
        publish_claimed_queue_eligible(shared, entry);
        return;
    }

    let pending =
        pending_admission
            .entry(ledger_key)
            .or_insert_with(|| PendingCompletionAdmission {
                turn_key: TurnKey::new(key.channel_id, key.user_msg_id, ledger_key.generation),
                completion_admission: CompletionAdmission::new(CompletionAdmissionPlan::Immediate),
                updated_at: Instant::now(),
            });
    update(&mut pending.completion_admission);
    pending.updated_at = Instant::now();
}

pub(super) fn apply_pending_completion_admission(
    entry: &mut LedgerEntry,
    pending: Option<PendingCompletionAdmission>,
) {
    let Some(pending) = pending else {
        return;
    };
    entry
        .completion_admission
        .update_plan(pending.completion_admission.plan);
    if pending.completion_admission.mailbox_released {
        entry.completion_admission.note_mailbox_released();
    }
    if pending.completion_admission.terminal_projection_settled {
        entry.completion_admission.note_terminal_projection_settled(
            pending
                .completion_admission
                .terminal_projection_allows_queue,
        );
    }
    if pending.completion_admission.terminal_disposition_settled {
        entry
            .completion_admission
            .note_terminal_disposition_settled(
                pending
                    .completion_admission
                    .terminal_disposition_allows_queue,
            );
    }
}

pub(super) fn note_mailbox_release_after_finalize(
    outcome: &FinalizeOutcome,
    entry: &mut LedgerEntry,
    shared: &SharedData,
) {
    if !matches!(
        outcome,
        FinalizeOutcome::Finalized {
            removed_token: Some(_),
            ..
        }
    ) {
        return;
    }
    entry.completion_admission.note_mailbox_released();
    publish_claimed_queue_eligible(shared, entry);
}

#[cfg(test)]
mod tests {
    use super::super::tests::with_isolated_runtime_root;
    use super::super::*;
    use std::sync::atomic::Ordering;

    use crate::services::discord::{
        make_shared_data_for_tests_with_storage, turn_completion_events,
    };

    async fn seed_active_turn(
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        user_msg_id: u64,
    ) -> Arc<CancelToken> {
        use serenity::model::id::{MessageId, UserId};
        let token = Arc::new(CancelToken::new());
        shared
            .mailbox(channel_id)
            .restore_active_turn(token.clone(), UserId::new(7), MessageId::new(user_msg_id))
            .await;
        token
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn early_deferred_registration_survives_later_immediate_refresh_4888() {
        with_isolated_runtime_root(|| async move {
            let shared = make_shared_data_for_tests_with_storage(None);
            let mut completion_events =
                turn_completion_events::subscribe_turn_completion_events(&shared);
            let channel_id = ChannelId::new(4_888_101);
            let turn_id = 4_888_102;
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let _token = seed_active_turn(&shared, channel_id, turn_id).await;
            let finalizer = TurnFinalizer::spawn();
            let key = TurnKey::new(channel_id, turn_id, 0);
            finalizer.register_start_with_completion_admission(
                key,
                ProviderKind::Claude,
                RelayOwnerKind::Watcher,
                CompletionAdmissionPlan::AfterTerminalProjectionSettled,
                &shared,
            );
            finalizer.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            let watcher = finalizer
                .submit_terminal(
                    key,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(watcher, FinalizeOutcome::Finalized { .. }));
            let released = completion_events
                .try_recv()
                .expect("mailbox release must publish its non-eligible edge");
            assert!(!released.queue_is_eligible());
            assert!(completion_events.try_recv().is_err());

            let bridge = finalizer
                .submit_terminal(
                    key,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(bridge, FinalizeOutcome::AlreadyFinalized));
            finalizer.note_terminal_projection_settled(key, true, shared.clone());
            finalizer.note_terminal_projection_settled(key, true, shared.clone());
            assert!(!finalizer.has_live_watcher_pending(channel_id, 0).await);

            let eligible = completion_events
                .try_recv()
                .expect("settled edge must release deferred queue admission");
            assert_eq!(eligible.channel_id, channel_id);
            assert_eq!(eligible.turn_id, Some(turn_id));
            assert!(eligible.queue_is_eligible());
            assert!(
                completion_events.try_recv().is_err(),
                "duplicate settled edges must not republish QueueEligible"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn denied_projection_settlement_cannot_be_upgraded_by_duplicate_owner_4888() {
        with_isolated_runtime_root(|| async move {
            let shared = make_shared_data_for_tests_with_storage(None);
            let mut completion_events =
                turn_completion_events::subscribe_turn_completion_events(&shared);
            let channel_id = ChannelId::new(4_888_111);
            let turn_id = 4_888_112;
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let _token = seed_active_turn(&shared, channel_id, turn_id).await;
            let finalizer = TurnFinalizer::spawn();
            let key = TurnKey::new(channel_id, turn_id, 0);
            finalizer.register_start_with_completion_admission(
                key,
                ProviderKind::Claude,
                RelayOwnerKind::Watcher,
                CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled,
                &shared,
            );

            let outcome = finalizer
                .submit_terminal(
                    key,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));
            let released = completion_events
                .try_recv()
                .expect("mailbox release must publish its non-eligible edge");
            assert!(!released.queue_is_eligible());

            finalizer.note_terminal_projection_settled(key, true, shared.clone());
            finalizer.note_terminal_disposition_settled(key, false, shared.clone());
            finalizer.note_terminal_disposition_settled(key, true, shared.clone());
            assert!(!finalizer.has_live_watcher_pending(channel_id, 0).await);
            assert!(
                completion_events.try_recv().is_err(),
                "a capped or failed retry decision must remain a permanent queue-admission veto"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn conflicting_pending_generations_do_not_cross_settle_and_orphan_expires_4906() {
        with_isolated_runtime_root(|| async move {
            let shared = make_shared_data_for_tests_with_storage(None);
            let mut completion_events =
                turn_completion_events::subscribe_turn_completion_events(&shared);
            let channel_id = ChannelId::new(4_906_101);
            let turn_id = 4_906_102;
            let old_key = TurnKey::new(channel_id, turn_id, 41);
            let current_key = TurnKey::new(channel_id, turn_id, 42);
            let finalizer = TurnFinalizer::spawn();

            finalizer.note_mailbox_released(old_key, shared.clone());
            finalizer.note_terminal_projection_settled(old_key, false, shared.clone());
            finalizer.note_mailbox_released(current_key, shared.clone());
            finalizer.note_terminal_projection_settled(current_key, true, shared.clone());
            assert!(completion_events.try_recv().is_err());

            finalizer.register_start_with_completion_admission(
                current_key,
                ProviderKind::Claude,
                RelayOwnerKind::Watcher,
                CompletionAdmissionPlan::AfterTerminalProjectionSettled,
                &shared,
            );
            assert!(
                finalizer
                    .has_live_watcher_pending(channel_id, current_key.generation)
                    .await
            );
            let eligible = completion_events
                .try_recv()
                .expect("the exact current-generation allow edge must release admission");
            assert!(eligible.queue_is_eligible());
            assert_eq!(eligible.turn_id, Some(turn_id));
            assert!(completion_events.try_recv().is_err());

            finalizer.note_terminal_projection_settled(old_key, true, shared.clone());
            assert!(
                completion_events.try_recv().is_err(),
                "the old generation's first denied edge must remain isolated and immutable"
            );

            let exact_old = old_key.exact_key();
            let mut pending = HashMap::from([(
                exact_old,
                PendingCompletionAdmission {
                    turn_key: old_key,
                    completion_admission: CompletionAdmission::new(
                        CompletionAdmissionPlan::AfterTerminalProjectionSettled,
                    ),
                    updated_at: Instant::now() - COMPLETION_ADMISSION_TTL,
                },
            )]);
            let mut ledger = HashMap::new();
            reconcile(&mut ledger, &mut pending, &shared).await;
            assert!(
                !pending.contains_key(&exact_old),
                "the orphan pending authority must expire at the shared TTL"
            );
            assert!(completion_events.try_recv().is_err());
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn finalized_authority_accepts_edge_after_legacy_sixty_second_window_4906() {
        with_isolated_runtime_root(|| async move {
            let shared = make_shared_data_for_tests_with_storage(None);
            let mut completion_events =
                turn_completion_events::subscribe_turn_completion_events(&shared);
            let channel_id = ChannelId::new(4_906_111);
            let turn_id = 4_906_112;
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let _token = seed_active_turn(&shared, channel_id, turn_id).await;
            let finalizer = TurnFinalizer::spawn();
            let key = TurnKey::new(channel_id, turn_id, 7);
            finalizer.register_start_with_completion_admission(
                key,
                ProviderKind::Claude,
                RelayOwnerKind::Watcher,
                CompletionAdmissionPlan::AfterTerminalProjectionSettled,
                &shared,
            );

            let outcome = finalizer
                .submit_terminal(
                    key,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));
            let released = completion_events.try_recv().expect("mailbox release edge");
            assert!(!released.queue_is_eligible());

            tokio::time::advance(Duration::from_secs(61)).await;
            tokio::task::yield_now().await;
            finalizer.note_terminal_projection_settled(key, true, shared.clone());
            assert!(!finalizer.has_live_watcher_pending(channel_id, 7).await);

            let eligible = completion_events
                .try_recv()
                .expect("late projection edge must retain admission authority beyond 60 seconds");
            assert!(eligible.queue_is_eligible());
            assert_eq!(eligible.turn_id, Some(turn_id));
            finalizer.note_terminal_projection_settled(key, true, shared.clone());
            assert!(completion_events.try_recv().is_err());
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn missing_ledger_mailbox_edge_replays_once_after_exact_registration_4906() {
        with_isolated_runtime_root(|| async move {
            let shared = make_shared_data_for_tests_with_storage(None);
            let mut completion_events =
                turn_completion_events::subscribe_turn_completion_events(&shared);
            let channel_id = ChannelId::new(4_906_121);
            let turn_id = 4_906_122;
            let key = TurnKey::new(channel_id, turn_id, 14);
            let finalizer = TurnFinalizer::spawn();

            finalizer.note_mailbox_released(key, shared.clone());
            assert!(completion_events.try_recv().is_err());
            finalizer.register_start_with_completion_admission(
                key,
                ProviderKind::Claude,
                RelayOwnerKind::None,
                CompletionAdmissionPlan::Immediate,
                &shared,
            );
            assert!(
                !finalizer
                    .has_live_watcher_pending(channel_id, key.generation)
                    .await
            );

            let eligible = completion_events
                .try_recv()
                .expect("stored exact missing-ledger mailbox edge must replay on registration");
            assert!(eligible.queue_is_eligible());
            assert_eq!(eligible.turn_id, Some(turn_id));
            finalizer.note_mailbox_released(key, shared.clone());
            assert!(completion_events.try_recv().is_err());
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn projection_settlement_before_mailbox_release_is_not_lost_4888() {
        with_isolated_runtime_root(|| async move {
            let shared = make_shared_data_for_tests_with_storage(None);
            let mut completion_events =
                turn_completion_events::subscribe_turn_completion_events(&shared);
            let channel_id = ChannelId::new(4_888_121);
            let turn_id = 4_888_122;
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let _token = seed_active_turn(&shared, channel_id, turn_id).await;
            let finalizer = TurnFinalizer::spawn();
            let key = TurnKey::new(channel_id, turn_id, 0);
            finalizer.register_start_with_completion_admission(
                key,
                ProviderKind::Claude,
                RelayOwnerKind::Watcher,
                CompletionAdmissionPlan::AfterTerminalProjectionSettled,
                &shared,
            );
            finalizer.note_terminal_projection_settled(key, true, shared.clone());
            assert!(finalizer.has_live_watcher_pending(channel_id, 0).await);
            assert!(completion_events.try_recv().is_err());

            let outcome = finalizer
                .submit_terminal(
                    key,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));
            let released = completion_events
                .try_recv()
                .expect("mailbox release must publish its non-eligible edge");
            assert!(!released.queue_is_eligible());
            let eligible = completion_events
                .try_recv()
                .expect("mailbox release must consume the previously settled projection edge");
            assert!(eligible.queue_is_eligible());
            assert_eq!(eligible.turn_id, Some(turn_id));
            assert!(completion_events.try_recv().is_err());
        })
        .await;
    }
}
