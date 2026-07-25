use super::completion_admission::{CompletionAdmission, publish_claimed_queue_eligible};
use super::*;

pub(super) fn handle_completion_admission_message(
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
    msg: FinalizeMsg,
) {
    match msg {
        FinalizeMsg::MailboxReleased { key, shared } => {
            update_completion_admission(ledger, key, &shared, |admission| {
                admission.note_mailbox_released();
            });
        }
        FinalizeMsg::TerminalProjectionSettled {
            key,
            allow_queue,
            shared,
        } => {
            update_completion_admission(ledger, key, &shared, |admission| {
                admission.note_terminal_projection_settled(allow_queue);
            });
        }
        FinalizeMsg::TerminalDispositionSettled {
            key,
            allow_queue,
            shared,
        } => {
            update_completion_admission(ledger, key, &shared, |admission| {
                admission.note_terminal_disposition_settled(allow_queue);
            });
        }
        _ => unreachable!("completion-admission dispatcher received another message"),
    }
}

fn update_completion_admission(
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
    key: TurnKey,
    shared: &SharedData,
    update: impl FnOnce(&mut CompletionAdmission),
) {
    let ledger_key = resolve_ledger_key(ledger, key);
    if let Some(entry) = ledger.get_mut(&ledger_key) {
        update(&mut entry.completion_admission);
        publish_claimed_queue_eligible(shared, entry);
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
    if entry.relay_owner == RelayOwnerKind::None {
        entry
            .completion_admission
            .note_terminal_projection_settled(true);
        entry
            .completion_admission
            .note_terminal_disposition_settled(true);
    }
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
