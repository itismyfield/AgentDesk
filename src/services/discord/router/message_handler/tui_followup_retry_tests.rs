use super::*;

#[cfg(test)]
mod busy_retry_fifo_tests {
    use super::super::tui_followup::*;
    use super::*;

    struct RuntimeRootGuard(Option<std::ffi::OsString>);

    impl Drop for RuntimeRootGuard {
        fn drop(&mut self) {
            unsafe {
                match self.0.take() {
                    Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                    None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
                }
            }
        }
    }

    fn intervention(author: u64, message: u64, text: &str, author_is_bot: bool) -> Intervention {
        let mut intervention = build_race_requeued_intervention(
            serenity::UserId::new(author),
            serenity::MessageId::new(message),
            text,
            false,
            None,
            false,
            false,
            Vec::new(),
            None,
        );
        intervention.author_is_bot = author_is_bot;
        intervention
    }

    #[tokio::test(flavor = "current_thread")]
    async fn headless_runtime_mismatch_defer_preserves_distinct_prompts_in_fifo_order_5015() {
        let temp = tempfile::tempdir().expect("temporary runtime root");
        let _root_guard = crate::config::set_agentdesk_root_for_test(temp.path());

        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = serenity::ChannelId::new(50_150_200);
        let human = intervention(50_150_201, 50_150_211, "human first", false);
        let persistence =
            crate::services::discord::queue_persistence_context(&shared, &provider, channel_id);
        shared
            .mailbox(channel_id)
            .replace_queue(vec![human.clone()], persistence)
            .await;

        for attempt in 0..6 {
            let retry = enqueue_headless_runtime_mismatch_defer(
                &shared,
                &provider,
                channel_id,
                serenity::MessageId::new(50_150_220 + attempt),
                &format!("headless retry {attempt}"),
            )
            .await;
            assert!(
                retry.enqueued,
                "distinct headless prompts must not deduplicate"
            );
        }
        let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
        assert_eq!(snapshot.intervention_queue.len(), 7);
        assert_eq!(snapshot.intervention_queue[0].message_id, human.message_id);
        for (index, queued) in snapshot.intervention_queue[1..].iter().enumerate() {
            assert!(queued.is_headless_runtime_mismatch_defer());
            assert_eq!(queued.text, format!("headless retry {index}"));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn headless_runtime_mismatch_marker_survives_disk_roundtrip_5015() {
        let temp = tempfile::tempdir().expect("temporary runtime root");
        let _root_guard = crate::config::set_agentdesk_root_for_test(temp.path());
        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = serenity::ChannelId::new(50_150_230);

        let retry = enqueue_headless_runtime_mismatch_defer(
            &shared,
            &provider,
            channel_id,
            serenity::MessageId::new(50_150_231),
            "durable headless retry",
        )
        .await;
        assert!(retry.enqueued);
        let (restored, _) =
            crate::services::turn_orchestrator::load_pending_queues(&provider, &shared.token_hash);
        let item = &restored[&channel_id][0];
        assert!(item.is_headless_runtime_mismatch_defer());
        assert_eq!(item.text, "durable headless retry");
    }

    // SAFETY: holds shared_test_env_lock across await to serialize the
    // AGENTDESK_ROOT_DIR mutation (RuntimeRootGuard tempdir) against parallel
    // tests. Test-only; the guard is a process-wide test serializer that cannot
    // deadlock a live task. Releasing it before the mailbox awaits would let a
    // concurrent test stomp the runtime root while this one is mid-flight.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn busy_retry_restores_dequeued_head_without_reversing_fifo_4795() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::tempdir().expect("temporary runtime root");
        let _root_guard = RuntimeRootGuard(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = serenity::ChannelId::new(4_795_100);
        let first = intervention(4_795_101, 4_795_111, "human A", false);
        let second = intervention(4_795_102, 4_795_112, "bot B", true);
        let persistence =
            crate::services::discord::queue_persistence_context(&shared, &provider, channel_id);
        shared
            .mailbox(channel_id)
            .replace_queue(vec![first.clone(), second.clone()], persistence.clone())
            .await;

        let dequeued = shared
            .mailbox(channel_id)
            .take_next_soft(persistence.clone())
            .await
            .intervention
            .expect("A is dequeued first");
        assert_eq!(dequeued.message_id, first.message_id);
        let _ = crate::services::discord::mailbox_clear_pending_dispatch_reservation(
            &shared,
            &provider,
            channel_id,
            dequeued.message_id,
        )
        .await;

        let retry = enqueue_busy_tui_followup_for_retry(
            &shared,
            &provider,
            channel_id,
            dequeued.author_id,
            dequeued.message_id,
            &dequeued.text,
            dequeued.preserve_on_cancel(),
            dequeued.reply_context,
            dequeued.has_reply_boundary,
            dequeued.merge_consecutive,
            dequeued.pending_uploads,
            dequeued.voice_announcement,
        )
        .await;
        assert!(retry.enqueued, "busy retry is restored at the queue front");

        let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
        let order: Vec<_> = snapshot
            .intervention_queue
            .iter()
            .map(|item| item.message_id)
            .collect();
        assert_eq!(order, vec![first.message_id, second.message_id]);
        assert!(!snapshot.intervention_queue[0].author_is_bot);
        assert!(snapshot.intervention_queue[1].author_is_bot);

        let retried_first = shared
            .mailbox(channel_id)
            .take_next_soft(persistence.clone())
            .await
            .intervention
            .expect("A retries before B");
        let _ = crate::services::discord::mailbox_abandon_pending_dispatch(
            &shared,
            &provider,
            channel_id,
            retried_first.message_id,
        )
        .await;
        let later_second = shared
            .mailbox(channel_id)
            .take_next_soft(persistence.clone())
            .await
            .intervention
            .expect("B remains second");
        assert_eq!(retried_first.message_id, first.message_id);
        assert_eq!(later_second.message_id, second.message_id);
        let _ = crate::services::discord::mailbox_abandon_pending_dispatch(
            &shared,
            &provider,
            channel_id,
            later_second.message_id,
        )
        .await;

        shared
            .mailbox(channel_id)
            .replace_queue(vec![first.clone(), second.clone()], persistence.clone())
            .await;
        let normal_first = shared
            .mailbox(channel_id)
            .take_next_soft(persistence.clone())
            .await
            .intervention
            .expect("normal FIFO returns A");
        let _ = crate::services::discord::mailbox_abandon_pending_dispatch(
            &shared,
            &provider,
            channel_id,
            normal_first.message_id,
        )
        .await;
        let normal_second = shared
            .mailbox(channel_id)
            .take_next_soft(persistence)
            .await
            .intervention
            .expect("normal FIFO returns B");
        assert_eq!(normal_first.message_id, first.message_id);
        assert_eq!(normal_second.message_id, second.message_id);
    }

    #[test]
    fn busy_retry_treats_pending_or_active_source_as_already_processing_4797() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::tempdir().expect("temporary runtime root");
        let _root_guard = RuntimeRootGuard(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");

        runtime.block_on(async {
            let shared = make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel_id = serenity::ChannelId::new(4_797_301);
            let first = intervention(4_797_302, 4_797_303, "pending A", false);
            let persistence =
                crate::services::discord::queue_persistence_context(&shared, &provider, channel_id);
            shared
                .mailbox(channel_id)
                .replace_queue(vec![first.clone()], persistence.clone())
                .await;
            let dequeued = shared
                .mailbox(channel_id)
                .take_next_soft(persistence)
                .await
                .intervention
                .expect("A is pending dispatch");

            let retry = enqueue_busy_tui_followup_for_retry(
                &shared,
                &provider,
                channel_id,
                dequeued.author_id,
                dequeued.message_id,
                &dequeued.text,
                dequeued.preserve_on_cancel(),
                dequeued.reply_context,
                dequeued.has_reply_boundary,
                dequeued.merge_consecutive,
                dequeued.pending_uploads,
                dequeued.voice_announcement,
            )
            .await;

            assert!(!retry.enqueued);
            assert_eq!(
                retry.refusal_reason,
                Some(
                    crate::services::turn_orchestrator::EnqueueRefusalReason::SourceIdPendingOrActive
                )
            );
            assert!(super::super::busy_retry::present_or_accepted(&retry));
            let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
            assert!(snapshot.intervention_queue.is_empty());
            assert_eq!(snapshot.pending_user_dispatch, Some(first.message_id));
        });
    }
}
