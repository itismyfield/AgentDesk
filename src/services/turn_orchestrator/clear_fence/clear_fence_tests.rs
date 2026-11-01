use crate::services::turn_orchestrator::pending_queue_persistence::{
    load_channel_pending_dispatch_marker_checked, load_channel_pending_queue_checked,
};
use crate::services::turn_orchestrator::*;
use poise::serenity_prelude::{ChannelId, UserId};

fn run(future: impl std::future::Future<Output = ()>) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future);
}

fn context() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, "clear-preserve", None)
}

fn item(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(id),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 1,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("waiting {id}"),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    }
}

async fn claim(handle: &ChannelMailboxHandle, id: u64) -> bool {
    handle
        .try_start_turn_kinded_with_persistence(
            Arc::new(CancelToken::new()),
            UserId::new(7),
            MessageId::new(id),
            ActiveTurnKind::UserOrAgent,
            TurnAdmissionOrder::Immediate,
            context(),
        )
        .await
        .started
}

#[test]
fn preserve_discards_waiting_inputs_without_releasing_the_actual_active_turn() {
    run(async {
        let channel = ChannelId::new(7_186_801);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        let mut active = item(801);
        active.source_message_ids.push(MessageId::new(800));
        assert!(handle.enqueue(active, context()).await.enqueued);
        let active_lease = handle
            .take_next_soft(context())
            .await
            .dispatch_lease
            .unwrap();
        assert!(claim(&handle, 801).await, "actual actor admission control");
        let before = handle.snapshot().await;
        assert!(handle.enqueue(item(802), context()).await.enqueued);
        let inflight = crate::config::runtime_root()
            .unwrap()
            .join("runtime/discord_inflight/claude")
            .join(format!("{}.json", channel.get()));
        std::fs::create_dir_all(inflight.parent().unwrap()).unwrap();
        std::fs::write(&inflight, b"the original execution evidence").unwrap();
        let result = handle.discard_waiting_or_refused(context()).await.unwrap();
        let after = handle.snapshot().await;
        assert!(result.removed_token.is_none());
        assert!(result.discarded_message_ids.contains(&MessageId::new(802)));
        assert!(!result.discarded_message_ids.contains(&MessageId::new(801)));
        assert_eq!(
            result.persistence.not_waiting,
            [MessageId::new(801), MessageId::new(800)]
        );
        assert!(!active_lease.superseded_by_clear());
        assert!(result.persistence.failed.is_empty());
        assert!(after.intervention_queue.is_empty());
        assert!(Arc::ptr_eq(
            before.cancel_token.as_ref().unwrap(),
            after.cancel_token.as_ref().unwrap()
        ));
        assert!(
            !after
                .cancel_token
                .as_ref()
                .unwrap()
                .cancelled
                .load(Ordering::Acquire)
        );
        assert_eq!(after.active_request_owner, before.active_request_owner);
        assert_eq!(after.active_user_message_id, before.active_user_message_id);
        assert_eq!(
            after.active_absorbed_source_ids,
            before.active_absorbed_source_ids
        );
        assert_eq!(after.active_turn_nonce, before.active_turn_nonce);
        assert_eq!(after.active_turn_kind, before.active_turn_kind);
        assert_eq!(after.turn_started_at, before.turn_started_at);
        assert_eq!(after.recovery_started_at, before.recovery_started_at);
        assert_eq!(after.claim_observation, before.claim_observation);
        assert_eq!(
            std::fs::read(inflight).unwrap(),
            b"the original execution evidence"
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                registry.turn_finished(channel).wait()
            )
            .await
            .is_err(),
            "Preserve must not publish turn completion"
        );
        assert!(!handle.recovery_done.latched.load(Ordering::Acquire));
        assert!(
            load_channel_pending_queue_checked(&ProviderKind::Claude, "clear-preserve", channel)
                .unwrap()
                .0
                .is_empty()
        );
    });
}

#[test]
fn a_cleared_dispatch_lease_refuses_late_claim_restore_and_marker_deletion() {
    run(async {
        let channel = ChannelId::new(7_186_802);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        let mut original = item(811);
        original.source_message_ids.push(MessageId::new(812));
        assert!(handle.enqueue(original.clone(), context()).await.enqueued);
        let taken = handle.take_next_soft(context()).await;
        assert_eq!(
            taken.intervention.as_ref().unwrap().message_id,
            original.message_id
        );
        let lease = taken.dispatch_lease.unwrap();
        assert!(
            !lease.superseded_by_clear(),
            "live production lease control"
        );
        assert_eq!(
            handle.snapshot().await.pending_user_dispatch,
            Some(original.message_id)
        );
        let cleared = handle.discard_waiting_or_refused(context()).await.unwrap();
        assert!(cleared.persistence.not_waiting.is_empty());
        for id in [811, 812] {
            assert!(cleared.discarded_message_ids.contains(&MessageId::new(id)));
            assert!(
                !claim(&handle, id).await,
                "a cleared source must not claim the actor"
            );
        }
        assert!(lease.superseded_by_clear());
        for owned in [true, false] {
            let restored = if owned {
                handle
                    .restore_dequeued_head(original.clone(), context(), lease.clone())
                    .await
            } else {
                handle.requeue_front(original.clone(), context()).await
            };
            assert!(!restored.enqueued);
            assert_eq!(
                restored.refusal_reason,
                Some(EnqueueRefusalReason::SupersededByClear)
            );
            assert!(restored.persistence_error.is_none());
        }
        handle
            .restore_active_turn(
                Arc::new(CancelToken::new()),
                UserId::new(7),
                original.message_id,
            )
            .await;
        assert!(handle.snapshot().await.cancel_token.is_none());
        assert_eq!(
            handle
                .recovery_kickoff(
                    Arc::new(CancelToken::new()),
                    UserId::new(7),
                    Some(original.message_id),
                )
                .await,
            RecoveryKickoffResult::Unavailable
        );
        assert_eq!(
            handle
                .recovery_kickoff(
                    Arc::new(CancelToken::new()),
                    UserId::new(7),
                    Some(MessageId::new(814)),
                )
                .await,
            RecoveryKickoffResult::Activated,
            "a distinct recovery input still reaches its actual activation branch"
        );
        handle.finish_turn(context()).await;
        assert!(handle.enqueue(item(813), context()).await.enqueued);
        let next = handle.take_next_soft(context()).await;
        assert_eq!(next.intervention.unwrap().message_id, MessageId::new(813));
        assert!(
            !handle
                .abandon_pending_dispatch_if_lease_matches(original.message_id, lease, context())
                .await
        );
        assert_eq!(
            load_channel_pending_dispatch_marker_checked(
                &ProviderKind::Claude,
                "clear-preserve",
                channel
            )
            .unwrap()
            .unwrap()
            .0
            .message_id,
            MessageId::new(813)
        );
        assert!(
            claim(&handle, 813).await,
            "a distinct current input remains claimable"
        );
    });
}

#[test]
fn discarded_memory_sources_refuse_old_and_fresh_observations_across_actor_purge() {
    run(async {
        let channel = ChannelId::new(7_186_803);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        let old = handle.snapshot().await.claim_observation;
        assert!(handle.enqueue(item(821), context()).await.enqueued);
        handle.discard_waiting_or_refused(context()).await.unwrap();
        let fresh = handle.snapshot().await.claim_observation;
        for observation in [Some(old), Some(fresh), None] {
            let late = handle
                .enqueue_observed(item(821), context(), observation)
                .await;
            assert!(
                !late.enqueued,
                "the actual discarded input cannot be reclassified as fresh"
            );
            assert_eq!(
                late.refusal_reason,
                Some(EnqueueRefusalReason::SupersededByClear)
            );
        }
        assert_eq!(
            registry.remove_idle_entry(channel).await,
            registry_purge::MailboxPurgeOutcome::Removed
        );
        let successor = registry.handle(channel);
        assert!(!successor.sender.same_channel(&handle.sender));
        assert!(
            !claim(&successor, 821).await,
            "the next actor retains the exact discard fence"
        );
        assert_eq!(
            successor.enqueue(item(821), context()).await.refusal_reason,
            Some(EnqueueRefusalReason::SupersededByClear)
        );
        assert!(successor.enqueue(item(822), context()).await.enqueued);
        assert!(
            successor
                .take_next_soft(context())
                .await
                .intervention
                .is_some()
        );
    });
}

#[test]
fn clear_rejects_late_queue_and_marker_restore_without_dropping_a_current_input() {
    run(async {
        let channel = ChannelId::new(7_186_804);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        let old = item(831);
        assert!(handle.enqueue(old.clone(), context()).await.enqueued);
        let cleared = handle.discard_waiting_or_refused(context()).await.unwrap();
        assert!(cleared.persistence.failed.is_empty());
        assert!(handle.enqueue(item(832), context()).await.enqueued);
        save_channel_pending_dispatch_marker(
            &ProviderKind::Claude,
            "clear-preserve",
            channel,
            &old,
            None,
        )
        .unwrap();
        assert_eq!(
            load_channel_pending_dispatch_marker_checked(
                &ProviderKind::Claude,
                "clear-preserve",
                channel,
            )
            .unwrap()
            .unwrap()
            .0
            .message_id,
            old.message_id
        );
        let merged = handle
            .merge_restored_dispatch_marker(old.clone(), None, context())
            .await;
        assert!(merged.persistence_error.is_none());
        assert_eq!(merged.absorbed, 0);
        save_channel_queue(
            &ProviderKind::Claude,
            "clear-preserve",
            channel,
            std::slice::from_ref(&old),
            None,
        )
        .unwrap();
        let loaded = handle.hydrate_pending_queue_from_disk(context()).await;
        assert_eq!(
            loaded.absorbed, 0,
            "a late disk snapshot must not restore the discarded source"
        );
        let snapshot = handle.snapshot().await;
        assert_eq!(snapshot.intervention_queue.len(), 1);
        assert_eq!(
            snapshot.intervention_queue[0].message_id,
            MessageId::new(832)
        );
        save_channel_pending_dispatch_marker(
            &ProviderKind::Claude,
            "clear-preserve",
            channel,
            &old,
            None,
        )
        .unwrap();
        let next = handle.take_next_soft(context()).await;
        assert_eq!(
            next.intervention.unwrap().message_id,
            MessageId::new(832),
            "a late discarded marker must not mint a new dispatch lease or block the current input"
        );
    });
}

#[test]
fn clear_consumes_late_injection_handbacks_and_reports_an_unnamed_reservation() {
    run(async {
        for (channel, message) in [(7_186_805, Some(MessageId::new(841))), (7_186_806, None)] {
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(ChannelId::new(channel));
            let ticket = match handle.reserve_injection(message, None, context()).await {
                ReserveOutcome::Reserved(ticket) => ticket,
                other => panic!("actual reservation control: {other:?}"),
            };
            let cleared = handle.discard_waiting_or_refused(context()).await.unwrap();
            assert_eq!(cleared.persistence.count_unknown, message.is_none());
            if let Some(id) = message {
                assert!(cleared.discarded_message_ids.contains(&id));
            }
            let settled = handle
                .settle_injected_input(
                    ticket,
                    InjectionSettlement::HandBack(Box::new(item(841))),
                    context(),
                )
                .await;
            let injected_inputs::SettleOutcome::Committed { queue_exit_events } = settled else {
                panic!("a superseded handback must end without retrying")
            };
            assert_eq!(queue_exit_events.len(), 1);
            assert_eq!(queue_exit_events[0].kind, QueueExitKind::Superseded);
            assert!(handle.snapshot().await.intervention_queue.is_empty());
            if message.is_some() {
                assert!(matches!(
                    handle.reserve_injection(message, None, context()).await,
                    ReserveOutcome::Consumed
                ));
            }
        }
    });
}

#[test]
fn clear_keeps_a_queued_head_ticket_stale_instead_of_reinjecting_it() {
    run(async {
        let channel = ChannelId::new(7_186_807);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        let mut waiting = item(851);
        waiting.source_message_queued_generations =
            vec![SourceMessageQueuedGeneration::user_instruction(
                waiting.message_id,
                waiting.queued_generation,
            )];
        assert!(handle.enqueue(waiting.clone(), context()).await.enqueued);
        let ticket = match handle
            .claim_queued_head(QueueIdentity::of(&waiting), context())
            .await
        {
            QueuedHeadClaim::Reserved(ticket) => ticket,
            _ => panic!("actual queued head reservation control"),
        };
        handle.discard_waiting_or_refused(context()).await.unwrap();
        assert!(matches!(
            handle
                .settle_queued_head(ticket, HeadOutcome::NotSent, context())
                .await,
            HeadSettle::Stale
        ));
        assert!(handle.snapshot().await.intervention_queue.is_empty());
    });
}

#[test]
fn preserve_commits_memory_despite_failed_writes_and_reports_unknown_marker_count() {
    run(async {
        let channel = ChannelId::new(7_186_808);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        assert!(claim(&handle, 861).await);
        assert!(handle.enqueue(item(862), context()).await.enqueued);
        let marker = pending_queue_persistence::pending_queue_file_path(
            &ProviderKind::Claude,
            "clear-preserve",
            channel,
        )
        .unwrap()
        .with_extension("dispatch");
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, b"invalid marker").unwrap();
        ChannelMailboxRegistry::fail_clear_removals(channel, false, 2, false);
        ChannelMailboxRegistry::fail_clear_removals(channel, true, 2, false);
        test_support::fail_queue_saves(channel, 2);
        let cleared = handle.discard_waiting_or_refused(context()).await.unwrap();
        assert!(cleared.persistence.count_unknown);
        assert_eq!(
            cleared.persistence.failed,
            ["queue_unlink", "marker_unlink", "queue_publish"]
        );
        assert!(cleared.removed_token.is_none());
        assert!(handle.snapshot().await.intervention_queue.is_empty());
        assert_eq!(
            handle.snapshot().await.active_user_message_id,
            Some(MessageId::new(861))
        );
        assert!(!handle.enqueue(item(862), context()).await.enqueued);
        assert!(
            load_channel_pending_dispatch_marker_checked(
                &ProviderKind::Claude,
                "clear-preserve",
                channel
            )
            .is_err()
        );
    });
}

#[test]
fn preserve_reads_disk_only_waiting_ids_before_clearing_and_reports_unreadable_queue() {
    run(async {
        let channel = ChannelId::new(7_186_810);
        let mut waiting = item(881);
        waiting.source_message_ids.push(MessageId::new(882));
        save_channel_queue(
            &ProviderKind::Claude,
            "clear-preserve",
            channel,
            &[waiting],
            None,
        )
        .unwrap();
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        assert!(handle.snapshot().await.intervention_queue.is_empty());
        assert_eq!(
            load_channel_pending_queue_checked(&ProviderKind::Claude, "clear-preserve", channel)
                .unwrap()
                .0
                .len(),
            1,
            "the actual waiting input exists only on disk"
        );
        let cleared = handle.discard_waiting_or_refused(context()).await.unwrap();
        assert!(!cleared.persistence.count_unknown);
        for id in [881, 882] {
            assert!(cleared.discarded_message_ids.contains(&MessageId::new(id)));
            assert_eq!(
                handle.enqueue(item(id), context()).await.refusal_reason,
                Some(EnqueueRefusalReason::SupersededByClear)
            );
        }
        assert!(
            load_channel_pending_queue_checked(&ProviderKind::Claude, "clear-preserve", channel)
                .unwrap()
                .0
                .is_empty()
        );
        assert!(handle.enqueue(item(883), context()).await.enqueued);

        let unreadable = ChannelId::new(7_186_811);
        let path = pending_queue_persistence::pending_queue_file_path(
            &ProviderKind::Claude,
            "clear-preserve",
            unreadable,
        )
        .unwrap();
        std::fs::write(path, b"unreadable waiting queue").unwrap();
        let cleared = registry
            .handle(unreadable)
            .discard_waiting_or_refused(context())
            .await
            .unwrap();
        assert!(
            cleared.persistence.count_unknown,
            "a failed inventory read is not an exact zero"
        );
        assert!(
            load_channel_pending_queue_checked(&ProviderKind::Claude, "clear-preserve", unreadable)
                .unwrap()
                .0
                .is_empty()
        );
    });
}

#[test]
fn release_clear_still_releases_the_active_turn_and_publishes_completion() {
    run(async {
        let channel = ChannelId::new(7_186_809);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        assert!(claim(&handle, 871).await);
        assert!(handle.enqueue(item(872), context()).await.enqueued);
        let result = handle.clear_or_refused(context()).await.unwrap();
        assert!(result.removed_token.is_some());
        assert!(handle.snapshot().await.cancel_token.is_none());
        assert!(handle.snapshot().await.intervention_queue.is_empty());
        assert!(
            handle.enqueue(item(872), context()).await.enqueued,
            "legacy Release keeps a newly observed, unprocessed input rereadable"
        );
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            registry.turn_finished(channel).wait(),
        )
        .await
        .unwrap();
    });
}

#[test]
fn release_clear_preserves_a_live_injection_for_a_late_front_handback() {
    use crate::services::provider::ProviderKind;
    use crate::services::turn_orchestrator::*;
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};

    let _root = crate::config::TestRuntimeRootGuard::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let context = || {
                QueuePersistenceContext::new(&ProviderKind::Claude, "release-reservation", None)
            };
            let item = |id: u64| Intervention {
                author_id: UserId::new(id),
                author_is_bot: false,
                message_id: MessageId::new(id),
                queued_generation: 1,
                source_message_ids: vec![MessageId::new(id)],
                source_message_queued_generations: Vec::new(),
                source_text_segments: Vec::new(),
                text: format!("unsent {id}"),
                mode: InterventionMode::Soft,
                created_at: std::time::Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
                pending_uploads: Vec::new(),
            };
            let channel = ChannelId::new(7_186_812);
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(channel);
            let returning = item(7_186_881);
            let discarded = item(7_186_882);
            let later = item(7_186_883);
            let ticket = match handle
                .reserve_injection(Some(returning.message_id), None, context())
                .await
            {
                ReserveOutcome::Reserved(ticket) => ticket,
                other => panic!("actual live injection reservation control: {other:?}"),
            };
            assert!(handle.enqueue(discarded.clone(), context()).await.enqueued);
            assert!(
                handle.take_next_soft(context()).await.intervention.is_none(),
                "the actual live reservation must hold the queued input before clear"
            );
            let cleared = handle.clear_or_refused(context()).await.unwrap();
            assert!(cleared.persistence.failed.is_empty());
            assert!(!cleared.persistence.count_unknown);
            assert!(cleared.discarded_message_ids.contains(&discarded.message_id));
            assert!(handle.snapshot().await.intervention_queue.is_empty());
            assert!(handle.enqueue(later.clone(), context()).await.enqueued);
            let exits = handle
                .hand_back_injected_input(ticket, returning.clone(), context())
                .await
                .expect("the unsent input's owner must complete its actual handback");
            assert_eq!(
                handle
                    .snapshot()
                    .await
                    .intervention_queue
                    .iter()
                    .map(|item| item.message_id)
                    .collect::<Vec<_>>(),
                vec![returning.message_id, later.message_id],
                "Release clear must let the live reservation hand its unsent input back to the queue front"
            );
            assert!(exits.is_empty(), "a Release handback is not superseded");
            let next = handle.take_next_soft(context()).await;
            assert!(next.persistence_error.is_none());
            assert_eq!(
                next.intervention.map(|item| item.message_id),
                Some(returning.message_id),
                "the actual dequeue must select the handed-back front input"
            );
        });
}
