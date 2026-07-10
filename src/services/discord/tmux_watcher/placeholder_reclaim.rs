//! #3351 relay-placeholder orphan reclaim helpers (sibling of the #3003
//! status-panel orphan arms in the parent watcher loop).

use super::*;

/// #3351 (#3003 r21 mirror): drop a durable orphan record once the placeholder
/// lifecycle finished for the turn (consumed into the final response, deleted,
/// or intentionally preserved with content) so a later drain cannot delete a
/// message that is no longer an orphan spinner.
pub(super) fn drop_placeholder_orphan_record(
    provider: &ProviderKind,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    msg_id: serenity::MessageId,
) {
    crate::services::discord::status_panel_orphan_store::remove(
        provider,
        &shared.token_hash,
        channel_id.get(),
        msg_id.get(),
    );
}

/// #3351: reclaim the same turn's relay placeholder alongside the orphan status
/// panel. Caller has already passed `watcher_should_reclaim_orphan_turn_placeholder`.
/// Outcome handling mirrors the panel arm (#3003 r10/r16): transient failure keeps
/// the local id for an in-turn retry + enqueues a durable record; committed /
/// permanent failure drops the handles and compare-and-clears the persisted
/// `current_msg_id` (#3077 pattern) so a later segment cannot edit the stale id.
/// `false` preserves the local handle for retry. The fresh-idle no-result caller
/// also treats it as a bounded finalize deferral; a redrive shield deliberately
/// uses that non-destructive path for at most 900s.
#[allow(clippy::too_many_arguments)]
pub(super) async fn reclaim_orphan_external_input_placeholder(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    placeholder_msg_id: &mut Option<serenity::MessageId>,
    placeholder_from_restored_inflight: &mut bool,
    last_edit_text: &mut String,
    provider: &ProviderKind,
    tmux_session_name: &str,
) -> bool {
    let Some(msg_id) = *placeholder_msg_id else {
        return true;
    };
    if let Some((nudged_at_millis, frontier_at_nudge, shield_identity)) =
        shared.redrive_placeholder_shield_context(provider, channel_id)
        && shield_identity.is_some()
        && shield_identity
            == crate::services::discord::inflight::load_inflight_state(provider, channel_id.get())
                .map(|state| {
                    crate::services::discord::inflight::InflightTurnIdentity::from_state(&state)
                })
        && super::panel_decisions::redrive_shielded_placeholder(
            nudged_at_millis,
            msg_id.created_at().timestamp_millis(),
            shared.committed_relay_offset(channel_id) <= frontier_at_nudge,
            chrono::Utc::now().timestamp_millis(),
        )
    {
        tracing::debug!(
            target: "agentdesk::discord::relay_recovery",
            event = "redrive_placeholder_reclaim_shielded",
            channel_id = channel_id.get(),
            message_id = msg_id.get(),
            frontier_at_nudge,
            "redrive-created placeholder preserved while the frontier is frozen"
        );
        return false;
    }
    let outcome = delete_nonterminal_placeholder(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        msg_id,
        "watcher_orphan_external_input_placeholder_cleanup",
    )
    .await;
    if !outcome.is_committed() && !outcome.is_permanent_failure() {
        crate::services::discord::status_panel_orphan_store::enqueue(
            provider,
            &shared.token_hash,
            channel_id.get(),
            msg_id.get(),
        );
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ watcher: orphan placeholder delete did not commit for channel {} msg {}; kept local id + enqueued durable retry",
            channel_id.get(),
            msg_id.get()
        );
        return false;
    }
    // #3351 (#3003 r21 mirror): an earlier transient attempt this turn may have
    // enqueued this placeholder in the durable store; the delete has now
    // committed (or permanently failed and is treated as committed), so drop
    // the record before the local handle is cleared.
    crate::services::discord::status_panel_orphan_store::remove(
        provider,
        &shared.token_hash,
        channel_id.get(),
        msg_id.get(),
    );
    if !outcome.is_committed() {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ watcher: orphan placeholder delete permanently failed for channel {} msg {}; giving up (treated as committed)",
            channel_id.get(),
            msg_id.get()
        );
    }
    *placeholder_msg_id = None;
    *placeholder_from_restored_inflight = false;
    last_edit_text.clear();
    let _ = crate::services::discord::inflight::clear_current_msg_if_matches(
        provider,
        channel_id.get(),
        msg_id.get(),
        Some(tmux_session_name),
    );
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🧹 watcher: cleaned orphan relay placeholder for TUI-direct turn (channel {}, tmux={}, msg={})",
        channel_id.get(),
        tmux_session_name,
        msg_id.get()
    );
    true
}

#[cfg(all(test, unix))]
mod redrive_reclaim_e2e_tests {
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    fn message_id_at(unix_secs: i64, sequence: u64) -> serenity::MessageId {
        const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
        let discord_ms = u64::try_from(unix_secs * 1_000 - DISCORD_EPOCH_MS)
            .expect("test timestamp after Discord epoch");
        serenity::MessageId::new((discord_ms << 22) | sequence)
    }

    #[test]
    fn live_tmux_redrive_reclaim_cycle_terminates_4299() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("temp runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tmp.path(),
        );
        if !crate::services::platform::tmux::is_available() {
            eprintln!("skipping #4299 redrive/reclaim E2E: tmux is not available");
            return;
        }

        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(4_299_004);
        let tmux_session = format!("AgentDesk-codex-e2e4299-{}", std::process::id());
        let created =
            crate::services::platform::tmux::create_session(&tmux_session, None, "sleep 60")
                .expect("create tmux session");
        assert!(created.status.success(), "live tmux session must start");
        let output_path = tmp.path().join("frozen-backlog.jsonl");
        std::fs::write(&output_path, vec![b'x'; 4_096]).expect("stage transcript backlog");
        let output_path = output_path.display().to_string();
        let base = chrono::Utc::now().timestamp();
        let user_msg_id = message_id_at(base - 60, 1).get();
        let mut inflight = crate::services::discord::inflight::InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            None,
            77,
            user_msg_id,
            0,
            "stale foreground".to_string(),
            None,
            Some(tmux_session.clone()),
            Some(output_path.clone()),
            None,
            0,
        );
        inflight.turn_source = crate::services::discord::inflight::TurnSource::Managed;
        assert!(
            crate::services::discord::inflight::save_inflight_state_if_absent(&inflight)
                .expect("persist stale foreground row")
        );

        let shared = crate::services::discord::make_shared_data_for_tests();
        let resume_offset = Arc::new(Mutex::new(None));
        shared.tmux_watchers.insert(
            channel_id,
            crate::services::discord::TmuxWatcherHandle {
                tmux_session_name: tmux_session.clone(),
                output_path: output_path.clone(),
                paused: Arc::new(AtomicBool::new(false)),
                resume_offset: resume_offset.clone(),
                cancel: Arc::new(AtomicBool::new(false)),
                pause_epoch: Arc::new(AtomicU64::new(0)),
                turn_delivered: Arc::new(AtomicBool::new(true)),
                last_heartbeat_ts_ms: Arc::new(AtomicI64::new(
                    crate::services::discord::tmux_watcher_now_ms(),
                )),
            },
        );
        let registry = crate::services::discord::health::HealthRegistry::new();
        let http = Arc::new(serenity::Http::new("Bot test-token"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        let (create_count, delete_count, nudge_count) = runtime.block_on(async {
            const BACKLOG_GRACE_SECS: i64 = 180;
            assert!(
                !registry
                    .redrive_undelivered_backlog_at(
                        &provider,
                        shared.clone(),
                        channel_id,
                        base - BACKLOG_GRACE_SECS,
                    )
                    .await
                    .expect("prime redrive observation")
            );
            let mut placeholder_msg_id = None;
            let mut restored = false;
            let mut last_edit_text = String::new();
            let mut creates = 0;
            let mut deletes = 0;
            let mut nudges = 0;
            for pass in 0..20 {
                let nudged = registry
                    .redrive_undelivered_backlog_at(
                        &provider,
                        shared.clone(),
                        channel_id,
                        base + i64::from(pass) * 30,
                    )
                    .await
                    .expect("redrive pass");
                if nudged {
                    nudges += 1;
                    if placeholder_msg_id.is_none() {
                        placeholder_msg_id = Some(message_id_at(base + 300, 2));
                        creates += 1;
                    }
                }
                if placeholder_msg_id.is_some() {
                    let reclaimed = reclaim_orphan_external_input_placeholder(
                        &http,
                        &shared,
                        channel_id,
                        &mut placeholder_msg_id,
                        &mut restored,
                        &mut last_edit_text,
                        &provider,
                        &tmux_session,
                    )
                    .await;
                    deletes += usize::from(reclaimed);
                    assert!(!reclaimed, "shielded reclaim must preserve and defer");
                    let preserved_msg_id = placeholder_msg_id
                        .expect("shielded reclaim must preserve the placeholder id");
                    assert!(
                        !crate::services::discord::status_panel_orphan_store::is_queued(
                            &provider,
                            &shared.token_hash,
                            channel_id.get(),
                            preserved_msg_id.get(),
                        ),
                        "shield must return before delete and durable-retry enqueue"
                    );
                }
            }
            assert!(
                registry
                    .redrive_undelivered_backlog_at(
                        &provider,
                        shared.clone(),
                        channel_id,
                        base + 930,
                    )
                    .await
                    .expect("sixth redrive pass")
            );
            nudges += 1;
            assert!(
                !registry
                    .redrive_undelivered_backlog_at(
                        &provider,
                        shared.clone(),
                        channel_id,
                        base + 1_890,
                    )
                    .await
                    .expect("capped redrive pass")
            );
            (creates, deletes, nudges)
        });

        assert_eq!(create_count, 1, "the shield must force placeholder reuse");
        assert_eq!(
            delete_count, 0,
            "no post-nudge placeholder may churn inside 900s"
        );
        assert_eq!(nudge_count, 6, "redrive must stop permanently at the cap");
        assert_eq!(*resume_offset.lock().unwrap(), Some(0));
        if let Some((_, handle)) = shared.tmux_watchers.remove(&channel_id) {
            handle.cancel.store(true, Ordering::Release);
        }
        crate::services::discord::inflight::clear_inflight_state(&provider, channel_id.get());
        let _ = crate::services::platform::tmux::kill_session(&tmux_session, "#4299 E2E cleanup");
    }
}
