use poise::serenity_prelude as serenity;
use crate::services::discord::tmux::tmux_watcher::terminal_commit_epilogue::continuation_marker_tests::{guard_with_stale, handle};
use crate::services::discord::tmux::tmux_watcher::pre_emit_guard::PreEmitGuardOutcome;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::provider::ProviderKind;
use std::sync::atomic::Ordering;

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn recovery_watcher_fence_precedes_legacy_reset_and_preserves_the_buffer_pg() {
    use crate::services::discord::inflight;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared = crate::services::discord::host_teardown_gate::test_support::shared_on(&pool).await;
    let channel = serenity::ChannelId::new(1_479_671_301_387_183_000);
    let _active = crate::services::discord::router::replay_test_activation::activate(channel.get());
    let h = handle("p3-watcher-only-fixture", "p3-output-unused");
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        channel.get(),
        None,
        7,
        channel.get() + 1,
        channel.get() + 2,
        "original".into(),
        Some("old-provider-session".into()),
        Some(h.tmux_session_name.clone()),
        None,
        None,
        0,
    );
    state.born_generation = 0;
    state.runtime_kind =
        Some(crate::services::agent_protocol::RuntimeHandoffKind::LegacyTmuxWrapper);
    inflight::save_inflight_state_create_new(&state).unwrap();
    sqlx::query("INSERT INTO intake_outbox(target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,user_text,provider,status,replay_only,replay_disposition,replay_source_message_ids,request_owner_id,turn_kind,agent_id)
        VALUES('worker','leader',$1,$2,'original','claude','unknown',TRUE,'withheld',ARRAY[$2],'7','standard','agent')")
        .bind(channel.get().to_string()).bind(state.user_msg_id.to_string()).execute(&pool).await.unwrap();
    let mut buffer = "captured partial output".to_string();
    assert_eq!(
        guard_with_stale(&shared, channel, &h, &mut buffer, false).await,
        PreEmitGuardOutcome::Proceed,
        "healthy non-stale control reaches the normal continuation"
    );
    assert_eq!(
        guard_with_stale(&shared, channel, &h, &mut buffer, true).await,
        PreEmitGuardOutcome::ContinueWatcherLoop
    );
    assert_eq!(buffer, "captured partial output");
    let kept = inflight::load_inflight_state(&ProviderKind::Claude, channel.get()).unwrap();
    assert_eq!(kept.session_id, state.session_id);
    assert!(!h.cancel.load(Ordering::Acquire));
    assert!(
        shared
            .mailbox(channel)
            .snapshot()
            .await
            .intervention_queue
            .is_empty()
    );
    pool.close().await;
    db.drop().await;
}
