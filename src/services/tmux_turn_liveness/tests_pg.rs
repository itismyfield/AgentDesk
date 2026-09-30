use super::idle_cleanup_session_is_unoccupied;

#[tokio::test]
async fn thread_gc_preserves_resume_rows_when_new_turn_starts_or_kill_failed_pg() {
    use crate::services::platform::tmux::SessionPresence;
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key = "test-host:AgentDesk-claude-gc-t1500628371829428350";
    sqlx::query(
        "INSERT INTO sessions (
             session_key, provider, status, thread_channel_id,
             claude_session_id, raw_provider_session_id, last_heartbeat
         ) VALUES ($1, 'claude', 'idle', '1500628371829428350',
                   'canonical-resume', 'native-resume', NOW() - INTERVAL '2 hours')",
    )
    .bind(key)
    .execute(&pool)
    .await
    .unwrap();
    let native = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        native.path(),
        "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
    )
    .unwrap();

    // Exercise the same candidate/occupancy/DELETE path as production. A
    // direct TUI turn starts during the probe while relay failure leaves the
    // DB heartbeat/status idle. Present must preserve the canonical row.
    let deleted =
        crate::db::dispatched_sessions::gc_stale_thread_sessions_with_probe_pg(&pool, |_| {
            std::fs::write(
                native.path(),
                "{\"type\":\"user\",\"message\":{\"content\":\"new turn\"}}\n",
            )
            .unwrap();
            std::future::ready(SessionPresence::Present)
        })
        .await;
    assert!(deleted.is_empty());
    assert_eq!(
        crate::services::tui_turn_state::observe_provider_jsonl_turn_state(
            &crate::services::provider::ProviderKind::Claude,
            native.path(),
        ),
        crate::services::tui_turn_state::TuiTurnState::UserSubmitted,
    );

    // An earlier idle-kill failure leaves tmux Present, even if the provider
    // is still idle. Neither that case nor an unavailable probe may DELETE.
    for presence in [SessionPresence::Present, SessionPresence::ProbeFailed] {
        std::fs::write(
            native.path(),
            "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
        )
        .unwrap();
        assert!(
            crate::db::dispatched_sessions::gc_stale_thread_sessions_with_probe_pg(&pool, |_| {
                std::future::ready(presence)
            },)
            .await
            .is_empty()
        );
        let selectors: (String, String) = sqlx::query_as(
            "SELECT claude_session_id, raw_provider_session_id FROM sessions WHERE session_key = $1",
        )
        .bind(key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            selectors,
            ("canonical-resume".into(), "native-resume".into())
        );
    }
    assert_eq!(
        crate::db::dispatched_sessions::gc_stale_thread_sessions_with_probe_pg(&pool, |_| {
            std::future::ready(SessionPresence::Missing)
        },)
        .await,
        vec![key.to_string()],
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn idle_cleanup_preserves_approval_and_background_children_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key = "test-host:AgentDesk-claude-idle-cleanup-parent";
    let parent_id: i64 = sqlx::query_scalar(
        "INSERT INTO sessions (session_key, provider, status)
         VALUES ($1, 'claude', 'idle') RETURNING id",
    )
    .bind(key)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(idle_cleanup_session_is_unoccupied(&pool, key).await);
    assert!(!idle_cleanup_session_is_unoccupied(&pool, "missing").await);

    for status in ["turn_active", "awaiting_user", "awaiting_bg"] {
        sqlx::query("UPDATE sessions SET status = $1 WHERE session_key = $2")
            .bind(status)
            .bind(key)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            !idle_cleanup_session_is_unoccupied(&pool, key).await,
            "{status}"
        );
    }
    sqlx::query("UPDATE sessions SET status = 'idle', active_children = 1 WHERE id = $1")
        .bind(parent_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(!idle_cleanup_session_is_unoccupied(&pool, key).await);

    // Preserve a real child even when the denormalized count was lost along
    // with relay state; stale counters must not authorize killing its parent.
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, parent_session_id)
         VALUES ('idle-cleanup-child', 'claude', 'turn_active', $1)",
    )
    .bind(parent_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE sessions SET active_children = 0 WHERE id = $1")
        .bind(parent_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(!idle_cleanup_session_is_unoccupied(&pool, key).await);
    sqlx::query("UPDATE sessions SET closed_at = NOW() WHERE parent_session_id = $1")
        .bind(parent_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(idle_cleanup_session_is_unoccupied(&pool, key).await);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn cleanup_host_never_reads_a_missing_or_unreadable_lookup_as_legacy_pg() {
    use super::cleanup_host::{
        CleanupHostRefusal, confirm_legacy_tmux_channel_pg, confirm_legacy_tmux_key_pg,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key = "test-host:AgentDesk-claude-host-check";
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status) VALUES ($1, 'claude', 'idle')",
    )
    .bind(key)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(confirm_legacy_tmux_key_pg(&pool, key).await, Ok(()));
    // Downstream steps refuse a missing row too, so only this check proves Missing is no legacy answer.
    let missing = "test-host:AgentDesk-claude-host-gone";
    assert_eq!(
        confirm_legacy_tmux_key_pg(&pool, missing).await,
        Err(CleanupHostRefusal::RowMissing)
    );
    assert_eq!(
        confirm_legacy_tmux_key_pg(&pool, " ").await,
        Err(CleanupHostRefusal::LookupFailed)
    );
    let channel = |session_key: Option<&str>| {
        let session_key = session_key.map(str::to_string);
        confirm_legacy_tmux_channel_pg(&pool, "claude", "discord_host", "4500", move || session_key)
    };
    assert_eq!(
        channel(Some(missing)).await,
        Err(CleanupHostRefusal::RowMissing)
    );
    assert_eq!(channel(None).await, Err(CleanupHostRefusal::RowMissing));
    assert_eq!(channel(Some(key)).await, Ok(key.to_string()));
    pool.close().await;
    db.drop().await;
}
