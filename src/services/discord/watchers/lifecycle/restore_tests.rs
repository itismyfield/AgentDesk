use super::*;

#[cfg(test)]
mod restored_session_cwd_channel_isolation_tests {
    //! #3207 (part 2) P0-b: watcher restart recovery resolves a restored
    //! session's cwd via `load_restored_session_cwd` and injects it into the
    //! restored runtime state (`session.current_path` via
    //! `select_restored_session_path`). The lookup must be scoped by the unique
    //! Discord channel id: two channels whose sanitized/truncated names collide
    //! share one `session_key`, and without the `channel_id = $2` predicate the
    //! recovering channel would recover into the OTHER channel's working tree.
    //! RED before the predicate was added, GREEN after.
    use super::{
        RestoreDispatchRebindOutcome, consume_dispatched_origin_ghost_if_current,
        load_restored_session_cwd, rebind_restored_dispatch_if_missing,
    };
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::db::dispatched_sessions::{HookSessionUpsert, upsert_hook_session_pg};
    use crate::services::discord::adk_session::build_namespaced_session_key;
    use crate::services::provider::ProviderKind;

    async fn write_turn_start_marker(
        pool: &sqlx::PgPool,
        session_key: &str,
        channel_id: u64,
        turn_nonce: &str,
        dispatched_origin: bool,
    ) {
        upsert_hook_session_pg(
            pool,
            HookSessionUpsert {
                session_key,
                instance_id: None,
                agent_id: None,
                provider: "claude",
                status: "turn_active",
                session_info: None,
                model: None,
                tokens: None,
                cwd: None,
                active_dispatch_id: None,
                thread_channel_id: None,
                channel_id: Some(&channel_id.to_string()),
                claude_session_id: None,
                raw_provider_session_id: None,
                turn_start_nonce: Some(turn_nonce),
                dispatched_origin,
            },
        )
        .await
        .expect("write turn-start marker");
    }

    async fn seed_session(
        pool: &sqlx::PgPool,
        session_key: &str,
        channel_id: Option<&str>,
        cwd: &str,
    ) {
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, cwd, channel_id, last_heartbeat)
             VALUES ($1, 'claude', 'idle', $2, $3, NOW())",
        )
        .bind(session_key)
        .bind(cwd)
        .bind(channel_id)
        .execute(pool)
        .await
        .expect("seed sessions row");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restored_dispatch_rebinds_valid_dispatch_with_cas() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "claude/test/ghost-rebind-4642";
        let channel_id = 464_200_001_u64;
        let dispatch_id = "dispatch-4642-valid";
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, channel_id, last_heartbeat)
             VALUES ($1, 'claude', 'turn_active', $2, NOW())",
        )
        .bind(session_key)
        .bind(channel_id.to_string())
        .execute(&pool)
        .await
        .expect("seed ghost session");
        sqlx::query("INSERT INTO task_dispatches (id, status) VALUES ($1, 'dispatched')")
            .bind(dispatch_id)
            .execute(&pool)
            .await
            .expect("seed active dispatch");

        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            crate::services::provider::ProviderKind::Claude,
            channel_id,
            Some("ghost-4642".to_string()),
            7,
            464_200_101,
            464_200_102,
            "ghost restore".to_string(),
            Some(session_key.to_string()),
            Some("AgentDesk-claude-ghost-4642".to_string()),
            None,
            None,
            0,
        );
        state.session_key = Some(session_key.to_string());
        state.dispatch_id = Some(dispatch_id.to_string());
        state.turn_nonce = Some("rebind-valid-4642".to_string());
        sqlx::query("UPDATE sessions SET active_turn_nonce = $2 WHERE session_key = $1")
            .bind(session_key)
            .bind("rebind-valid-4642")
            .execute(&pool)
            .await
            .expect("seed matching rebind nonce");
        assert_eq!(
            rebind_restored_dispatch_if_missing(Some(&pool), &state).await,
            RestoreDispatchRebindOutcome::Rebound,
            "a valid inflight dispatch must CAS-rebind the missing session link"
        );
        let linked: Option<String> =
            sqlx::query_scalar("SELECT active_dispatch_id FROM sessions WHERE session_key = $1")
                .bind(session_key)
                .fetch_one(&pool)
                .await
                .expect("load rebound dispatch");
        assert_eq!(linked.as_deref(), Some(dispatch_id));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restored_dispatch_does_not_clobber_concurrently_linked_turn() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "claude/test/dispatch-rebind-race-4642";
        let channel_id = 464_200_002_u64;
        let inflight_dispatch = "dispatch-4642-old";
        let newer_dispatch = "dispatch-4642-new";
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, channel_id, active_dispatch_id, last_heartbeat)
             VALUES ($1, 'claude', 'turn_active', $2, $3, NOW())",
        )
        .bind(session_key)
        .bind(channel_id.to_string())
        .bind(newer_dispatch)
        .execute(&pool)
        .await
        .expect("seed concurrently linked session");
        sqlx::query("INSERT INTO task_dispatches (id, status) VALUES ($1, 'dispatched')")
            .bind(inflight_dispatch)
            .execute(&pool)
            .await
            .expect("seed restored dispatch");

        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            crate::services::provider::ProviderKind::Claude,
            channel_id,
            Some("rebind-race-4642".to_string()),
            7,
            464_200_201,
            464_200_202,
            "restored dispatch".to_string(),
            Some(session_key.to_string()),
            Some("AgentDesk-claude-rebind-race-4642".to_string()),
            None,
            None,
            0,
        );
        state.session_key = Some(session_key.to_string());
        state.dispatch_id = Some(inflight_dispatch.to_string());
        assert_eq!(
            rebind_restored_dispatch_if_missing(Some(&pool), &state).await,
            RestoreDispatchRebindOutcome::NotRebound,
            "the CAS must not overwrite a dispatch linked by a newer turn"
        );
        let linked: Option<String> =
            sqlx::query_scalar("SELECT active_dispatch_id FROM sessions WHERE session_key = $1")
                .bind(session_key)
                .fetch_one(&pool)
                .await
                .expect("load preserved dispatch link");
        assert_eq!(linked.as_deref(), Some(newer_dispatch));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatched_origin_ghost_marker_is_consumed_only_for_matching_turn() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "claude/test/dispatched-origin-ghost-4642";
        let channel_id = 464_200_004_u64;
        let turn_nonce = "turn-nonce-4642";
        write_turn_start_marker(&pool, session_key, channel_id, turn_nonce, true).await;
        let marker: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT active_turn_nonce, dispatched_origin_turn_nonce FROM sessions WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(&pool)
        .await
        .expect("load persisted dispatched-origin marker");
        assert_eq!(marker.0.as_deref(), Some(turn_nonce));
        assert_eq!(marker.1.as_deref(), Some(turn_nonce));

        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            crate::services::provider::ProviderKind::Claude,
            channel_id,
            Some("dispatched-origin-ghost-4642".to_string()),
            7,
            464_200_401,
            464_200_402,
            "orphaned dispatch".to_string(),
            Some(session_key.to_string()),
            Some("AgentDesk-claude-dispatched-origin-ghost-4642".to_string()),
            None,
            None,
            0,
        );
        state.session_key = Some(session_key.to_string());
        state.turn_nonce = Some(turn_nonce.to_string());
        // No runtime root is configured in this database mutation proof, so the
        // identity-guarded clear reports Missing and is safe to consume.
        assert!(consume_dispatched_origin_ghost_if_current(Some(&pool), &state).await);
        let row: (String, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT status, active_dispatch_id, dispatched_origin_turn_nonce FROM sessions WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(&pool)
        .await
        .expect("load consumed ghost");
        assert_eq!(row.0, "idle");
        assert!(row.1.is_none());
        assert!(row.2.is_none());

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interactive_restore_without_dispatch_is_untouched() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "claude/test/interactive-restore-4642";
        let channel_id = 464_200_003_u64;
        let turn_nonce = "interactive-turn-4642";
        write_turn_start_marker(&pool, session_key, channel_id, turn_nonce, false).await;
        let marker: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT active_turn_nonce, dispatched_origin_turn_nonce FROM sessions WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(&pool)
        .await
        .expect("load persisted interactive marker");
        assert_eq!(marker.0.as_deref(), Some(turn_nonce));
        assert!(marker.1.is_none());

        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            crate::services::provider::ProviderKind::Claude,
            channel_id,
            Some("interactive-4642".to_string()),
            7,
            464_200_301,
            464_200_302,
            "ordinary user question".to_string(),
            Some(session_key.to_string()),
            Some("AgentDesk-claude-interactive-4642".to_string()),
            None,
            None,
            0,
        );
        state.session_key = Some(session_key.to_string());
        state.turn_nonce = Some(turn_nonce.to_string());
        assert_eq!(
            rebind_restored_dispatch_if_missing(Some(&pool), &state).await,
            RestoreDispatchRebindOutcome::NotRebound,
            "dispatch-less interactive inflight must retain ordinary restore behavior"
        );
        assert!(
            !consume_dispatched_origin_ghost_if_current(Some(&pool), &state).await,
            "dispatch-less interactive turn must fail closed without a dispatched-origin marker"
        );
        let row: (String, Option<String>) = sqlx::query_as(
            "SELECT status, active_dispatch_id FROM sessions WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(&pool)
        .await
        .expect("load interactive session after restore probe");
        assert_eq!(row.0, "turn_active");
        assert_eq!(row.1, None);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_recovery_cwd_does_not_cross_channels() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let provider = ProviderKind::Claude;
        let token_hash = "tok-3207-watcher";
        let collide_name = "shared-watcher-name";
        let tmux_name = provider.build_tmux_session_name(collide_name);
        let session_key = build_namespaced_session_key(token_hash, &provider, &tmux_name);
        let session_keys = vec![session_key.clone()];
        let channel_a: u64 = 777_777_777_777_777_777;
        let channel_b: u64 = 888_888_888_888_888_888;

        // `load_restored_session_cwd` only returns a path that exists on disk
        // (`is_dir()`), so seed the owner's cwd as a real temp directory.
        let owner_dir =
            std::env::temp_dir().join(format!("adk-3207-watcher-{}", std::process::id()));
        std::fs::create_dir_all(&owner_dir).expect("create owner cwd dir");
        let owner_cwd = owner_dir.to_string_lossy().to_string();

        seed_session(
            &pool,
            &session_key,
            Some(&channel_a.to_string()),
            &owner_cwd,
        )
        .await;

        // Owner channel recovers its own cwd.
        let owner = load_restored_session_cwd(Some(&pool), &session_keys, channel_a);
        assert_eq!(
            owner.as_deref(),
            Some(owner_cwd.as_str()),
            "the owning channel must recover its own persisted cwd"
        );

        // The colliding (different-id) channel must NOT recover channel A's cwd
        // (RED before the P0-b `channel_id = $2` fix).
        let cross = load_restored_session_cwd(Some(&pool), &session_keys, channel_b);
        assert_eq!(
            cross, None,
            "a different channel sharing the same session_key must NOT recover \
             another channel's working tree"
        );

        let _ = std::fs::remove_dir_all(&owner_dir);
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_recovery_cwd_legacy_null_channel_id_not_reused() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let provider = ProviderKind::Claude;
        let token_hash = "tok-3207-watcher-legacy";
        let channel_name = "legacy-watcher-chan";
        let tmux_name = provider.build_tmux_session_name(channel_name);
        let session_key = build_namespaced_session_key(token_hash, &provider, &tmux_name);
        let session_keys = vec![session_key.clone()];
        let channel_id: u64 = 999_999_999_999_999_999;

        let dir =
            std::env::temp_dir().join(format!("adk-3207-watcher-legacy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create legacy cwd dir");
        let cwd = dir.to_string_lossy().to_string();

        // A row written before the channel_id column existed has NULL channel_id.
        seed_session(&pool, &session_key, None, &cwd).await;

        let resolved = load_restored_session_cwd(Some(&pool), &session_keys, channel_id);
        assert_eq!(
            resolved, None,
            "a legacy NULL-channel_id row must not be reused for watcher recovery"
        );

        let _ = std::fs::remove_dir_all(&dir);
        pool.close().await;
        pg_db.drop().await;
    }
}
