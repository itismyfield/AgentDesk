use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::services::discord::tmux::tmux_watcher::placeholder_reclaim::reclaim_orphan_external_input_placeholder;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::{self as serenity, ChannelId};

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
    let created = crate::services::platform::tmux::create_session(&tmux_session, None, "sleep 60")
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
        for elapsed in [0, 30, 90, 210, 450] {
            let nudged = registry
                .redrive_undelivered_backlog_at(
                    &provider,
                    shared.clone(),
                    channel_id,
                    base + elapsed,
                )
                .await
                .expect("redrive pass");
            assert!(nudged, "scheduled redrive must fire at +{elapsed}s");
            nudges += 1;
            *resume_offset.lock().expect("resume offset") = None;
            if placeholder_msg_id.is_none() {
                placeholder_msg_id = Some(message_id_at(base + 300, 2));
                creates += 1;
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
                let preserved_msg_id =
                    placeholder_msg_id.expect("shielded reclaim must preserve the placeholder id");
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
                .redrive_undelivered_backlog_at(&provider, shared.clone(), channel_id, base + 930,)
                .await
                .expect("sixth redrive pass")
        );
        nudges += 1;
        *resume_offset.lock().expect("resume offset") = None;
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
        assert!(
            !registry
                .redrive_undelivered_backlog_at(
                    &provider,
                    shared.clone(),
                    channel_id,
                    base + 4_529,
                )
                .await
                .expect("pre-rearm boundary pass")
        );
        assert!(
            registry
                .redrive_undelivered_backlog_at(
                    &provider,
                    shared.clone(),
                    channel_id,
                    base + 4_530,
                )
                .await
                .expect("post-rearm boundary pass")
        );
        nudges += 1;
        (creates, deletes, nudges)
    });

    assert_eq!(create_count, 1, "the shield must force placeholder reuse");
    assert_eq!(
        delete_count, 0,
        "no post-nudge placeholder may churn inside 900s"
    );
    assert_eq!(
        nudge_count, 7,
        "redrive must remain capped for one hour, then re-arm"
    );
    assert_eq!(*resume_offset.lock().unwrap(), Some(0));
    if let Some((_, handle)) = shared.tmux_watchers.remove(&channel_id) {
        handle.cancel.store(true, Ordering::Release);
    }
    crate::services::discord::inflight::clear_inflight_state(&provider, channel_id.get());
    let _ = crate::services::platform::tmux::kill_session(&tmux_session, "#4299 E2E cleanup");
}
