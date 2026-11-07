use super::*;

/// After an edit failure on an already committed range, the shared reconciliation deletes the
/// placeholder only when a different message is proven to hold the delivered body; without that
/// proof the empty placeholder and its tracking stay.
#[test]
fn committed_edit_failure_deletes_placeholder_only_with_delivered_elsewhere_proof() {
    use super::super::terminal_long_chunks::{
        GuardedWatcherDeliveryResult, record_watcher_terminal_delivery, watcher_delivery_identity,
    };
    use super::terminal_send::committed_placeholder_cleanup::{
        CommittedEditFailureReconcileCtx, reconcile_already_committed_after_edit_failure,
    };
    use crate::services::discord::health::test_support::MockDiscord;
    use crate::test_support::wait::{WAIT_CAP, await_within};
    let root = tempfile::tempdir().expect("runtime root");
    let _root_guard = crate::config::set_agentdesk_root_for_test(root.path());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    for (index, delivered_elsewhere) in [(0u64, true), (1, false)] {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(4_508_100 + index);
        let (placeholder, delivered) = (MessageId::new(4_508_200), 4_508_300);
        let tmux = format!("committed-edit-failure-{index}");
        let generation = crate::services::tmux_common::session_temp_path(&tmux, "generation");
        std::fs::create_dir_all(std::path::Path::new(&generation).parent().unwrap()).unwrap();
        std::fs::write(&generation, b"1").unwrap();
        let transcript = root.path().join(format!("{tmux}.jsonl"));
        std::fs::File::create(&transcript)
            .and_then(|file| file.set_len(1_000))
            .expect("transcript");
        shared.tmux_watchers.insert(
            channel,
            crate::services::discord::TmuxWatcherHandle {
                tmux_session_name: tmux.clone(),
                output_path: transcript.to_string_lossy().into_owned(),
                paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                resume_offset: Arc::new(std::sync::Mutex::new(None)),
                cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                    crate::services::discord::tmux_watcher_now_ms(),
                )),
            },
        );
        if delivered_elsewhere {
            let identity = watcher_delivery_identity(
                dr::current_generation_mtime_ns(&tmux),
                shared.relay_frontier_token(channel).reset_incarnation,
                None,
            );
            let recorded = record_watcher_terminal_delivery(
                crate::services::discord::tmux::WatcherDeliveryTarget {
                    shared: &shared,
                    provider: &ProviderKind::Claude,
                    channel_id: channel,
                    tmux_session_name: &tmux,
                },
                identity,
                (100, 300),
                Some(delivered),
                "delivered body",
            );
            assert_eq!(recorded, GuardedWatcherDeliveryResult::Persisted);
        }
        let mut placeholder_msg_id = Some(placeholder);
        let (mut direct_send_delivered, mut anchor_visible, mut restored) = (false, true, true);
        let mut last_edit_text = String::new();
        let calls = rt.block_on(async {
            let discord = MockDiscord::start().await;
            await_within(
                reconcile_already_committed_after_edit_failure(CommittedEditFailureReconcileCtx {
                    http: &discord.http,
                    shared: &shared,
                    provider: &ProviderKind::Claude,
                    channel_id: channel,
                    tmux_session_name: &tmux,
                    msg_id: placeholder,
                    inflight_before_relay: None,
                    range: (150, 300),
                    response_sent_offset: 0,
                    edit_error: "500 edit failed".to_string(),
                    direct_send_delivered: &mut direct_send_delivered,
                    tui_direct_anchor_terminal_body_visible: &mut anchor_visible,
                    placeholder_msg_id: &mut placeholder_msg_id,
                    placeholder_from_restored_inflight: &mut restored,
                    last_edit_text: &mut last_edit_text,
                    cleanup_source: "committed_edit_failure_test",
                    record_source: "committed_edit_failure_test",
                }),
                WAIT_CAP,
            )
            .await;
            discord.calls_for(channel.get())
        });

        let deletes: Vec<&String> = calls.iter().filter(|c| c.starts_with("DELETE")).collect();
        let expected_delete = format!("DELETE /api/v10/channels/{channel}/messages/{placeholder}");
        assert_eq!(
            deletes,
            if delivered_elsewhere { vec![&expected_delete] } else { Vec::new() },
            "delivered_elsewhere={delivered_elsewhere}"
        );
        assert_eq!(
            (placeholder_msg_id, restored),
            if delivered_elsewhere { (None, false) } else { (Some(placeholder), true) },
            "tracking clears only after the delete commits"
        );
    }
}

#[test]
fn committed_cleanup_preserves_tracking_until_delete_commits_4508() {
    use std::cell::Cell;

    use crate::services::discord::placeholder_cleanup::PlaceholderCleanupOutcome;
    use super::terminal_send::committed_placeholder_cleanup::reconcile_committed_placeholder_cleanup;

    fn run(
        outcome: Option<&PlaceholderCleanupOutcome>,
    ) -> (bool, Option<MessageId>, bool, String, bool) {
        let mut placeholder_msg_id = Some(MessageId::new(45_080_900));
        let mut restored = true;
        let mut last_edit_text = "streamed body".to_string();
        let orphan_dropped = Cell::new(false);
        let committed = reconcile_committed_placeholder_cleanup(
            outcome,
            &mut placeholder_msg_id,
            &mut restored,
            &mut last_edit_text,
            || orphan_dropped.set(true),
        );
        (
            committed,
            placeholder_msg_id,
            restored,
            last_edit_text,
            orphan_dropped.get(),
        )
    }

    let preserved = run(Some(&PlaceholderCleanupOutcome::failed(
        "transient delete failure",
    )));
    assert_eq!(
        preserved,
        (
            false,
            Some(MessageId::new(45_080_900)),
            true,
            "streamed body".to_string(),
            false,
        ),
        "uncommitted cleanup must retain local and durable orphan tracking"
    );

    for committed_outcome in [
        PlaceholderCleanupOutcome::Succeeded,
        PlaceholderCleanupOutcome::AlreadyGone,
    ] {
        assert_eq!(
            run(Some(&committed_outcome)),
            (true, None, false, String::new(), true),
            "committed delete is the sole authority to clear lifecycle tracking"
        );
    }
    assert!(!run(None).0);
}

#[test]
fn recovered_task_response_identity_is_stable_without_inflight_or_context() {
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_055_902);
    let session_name = "AgentDesk-claude-4055-durable-fallback-gate";
    let turn_key = crate::services::discord::task_notification_delivery::fallback_response_turn_key(
        channel_id.get(),
        provider.as_str(),
        session_name,
        20,
        "done",
    );
    let same = crate::services::discord::task_notification_delivery::fallback_response_turn_key(
        channel_id.get(),
        provider.as_str(),
        session_name,
        20,
        "done",
    );
    let next = crate::services::discord::task_notification_delivery::fallback_response_turn_key(
        channel_id.get(),
        provider.as_str(),
        session_name,
        30,
        "done",
    );
    assert_eq!(turn_key, same);
    assert_ne!(turn_key, next);
    let event = crate::services::discord::task_notification_delivery::TaskCardEvent::from_recovered_terminal(
        channel_id.get(),
        provider.as_str(),
        session_name,
        TaskNotificationKind::Background,
        &turn_key,
    );
    assert_eq!(event.event_key(), format!("turn:{turn_key}"));
}
