use super::*;

#[cfg(test)]
mod dispatched_origin_ghost_order_pg_tests {
    //! #5462 S1: `consume_dispatched_origin_ghost_if_current` used to destroy
    //! the inflight row FIRST and only then ask `sessions` whether the turn was
    //! a dispatched-origin ghost at all. A live turn that never had a dispatch
    //! matched zero rows in that verdict — so the function returned `false`
    //! without a word, having already unlinked the row of a turn that was still
    //! streaming, and the terminal frame landed with no delivery owner.
    //!
    //! These tests pin the repaired order (non-destructive probe → ownership
    //! proof → state mutation) and the return-value contract. RED before the
    //! probe was hoisted ahead of the clear, GREEN after.
    use super::consume_dispatched_origin_ghost_if_current;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::db::dispatched_sessions::{HookSessionUpsert, upsert_hook_session_pg};
    use crate::services::discord::inflight::{
        InflightTurnState, load_inflight_state, save_inflight_state,
    };
    use crate::services::provider::ProviderKind;

    /// Write the durable turn-start marker the restore-time ghost verdict reads.
    /// `dispatched_origin` is the whole difference between a dispatch-born turn
    /// and an ordinary interactive one: only the former gets
    /// `dispatched_origin_turn_nonce`.
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

    fn inflight_row(
        channel_id: u64,
        session_key: &str,
        user_msg_id: u64,
        turn_nonce: &str,
    ) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            channel_id,
            Some(format!("ghost-5462-{channel_id}")),
            7,
            user_msg_id,
            user_msg_id + 1,
            "restore this turn".to_string(),
            Some(session_key.to_string()),
            Some(format!("AgentDesk-claude-ghost-5462-{channel_id}")),
            None,
            None,
            0,
        );
        state.session_key = Some(session_key.to_string());
        state.turn_nonce = Some(turn_nonce.to_string());
        state
    }

    async fn session_row(pool: &sqlx::PgPool, session_key: &str) -> (String, Option<String>) {
        sqlx::query_as(
            "SELECT status, dispatched_origin_turn_nonce FROM sessions WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(pool)
        .await
        .expect("load session row")
    }

    /// (i) The consume path itself is preserved: a real dispatched-origin ghost
    /// with a matching inflight row on disk loses both the row and the marker,
    /// and the caller is told the channel is done.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_dispatched_origin_ghost_is_consumed_after_the_probe() {
        let root = tempfile::tempdir().expect("runtime root");
        let _root_env = crate::config::set_agentdesk_root_for_test(root.path());
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "claude/test/ghost-consume-5462";
        let channel_id = 546_200_001_u64;
        let turn_nonce = "ghost-nonce-5462";
        write_turn_start_marker(&pool, session_key, channel_id, turn_nonce, true).await;

        let state = inflight_row(channel_id, session_key, 546_200_101, turn_nonce);
        save_inflight_state(&state).expect("seed the ghost's inflight row");
        assert!(
            load_inflight_state(&ProviderKind::Claude, channel_id).is_some(),
            "fixture must start with the ghost row on disk"
        );

        assert!(
            consume_dispatched_origin_ghost_if_current(Some(&pool), &state).await,
            "a real ghost must still be consumed: the probe matches, the guarded clear owns the row, and the release takes it"
        );
        assert!(
            load_inflight_state(&ProviderKind::Claude, channel_id).is_none(),
            "the consumed ghost's inflight row must be gone"
        );
        let (status, origin_nonce) = session_row(&pool, session_key).await;
        assert_eq!(status, "idle");
        assert_eq!(origin_nonce, None);

        pool.close().await;
        pg_db.drop().await;
    }

    /// (ii) The incident. A live turn that was never dispatched is not a ghost,
    /// and the probe now answers that before anything is destroyed: the row
    /// stays on disk so the turn keeps a delivery owner.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_turn_without_dispatch_keeps_its_inflight_row() {
        let root = tempfile::tempdir().expect("runtime root");
        let _root_env = crate::config::set_agentdesk_root_for_test(root.path());
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "claude/test/live-interactive-5462";
        let channel_id = 546_200_002_u64;
        let turn_nonce = "live-nonce-5462";
        write_turn_start_marker(&pool, session_key, channel_id, turn_nonce, false).await;

        let state = inflight_row(channel_id, session_key, 546_200_201, turn_nonce);
        save_inflight_state(&state).expect("seed the live turn's inflight row");

        assert!(
            !consume_dispatched_origin_ghost_if_current(Some(&pool), &state).await,
            "a dispatch-less turn has no dispatched-origin marker to consume"
        );
        let preserved = load_inflight_state(&ProviderKind::Claude, channel_id).expect(
            "a live turn's inflight row must survive a verdict that says it is not a ghost",
        );
        assert_eq!(preserved.user_msg_id, state.user_msg_id);
        assert_eq!(preserved.turn_nonce.as_deref(), Some(turn_nonce));
        let (status, origin_nonce) = session_row(&pool, session_key).await;
        assert_eq!(status, "turn_active");
        assert_eq!(origin_nonce, None);

        pool.close().await;
        pg_db.drop().await;
    }

    /// [ERRATUM R3-E1] A row born in the running process — the shape intake
    /// mints while the reconcile window is still open — can still satisfy the
    /// durable ghost predicate, which reads `sessions` and knows nothing about
    /// generations. The S2 fence is what preserves it and forces restore to
    /// continue rather than consuming the marker or skipping watcher
    /// re-registration. The fixture is born through `InflightTurnState::new`
    /// under a pinned generation, so `born_generation` is stamped by the real
    /// birth path rather than asserted into place.
    ///
    /// Scope, stated because the fence is easy to over-read: it covers rows this
    /// process *authored*. A row readopted from an earlier generation keeps that
    /// earlier `born_generation` — readoption does not restamp it — so the fence
    /// passes it through to the clear. That population is §9-1's open hole,
    /// deferred to α/β, and no test here covers it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn current_generation_live_row_is_not_consumed() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::tempdir().expect("runtime root");
        let _root_env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let current_generation = 54_620;
        crate::services::discord::runtime_store::set_process_generation_for_tests(Some(
            current_generation,
        ));
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "claude/test/current-generation-live-5462";
        let channel_id = 546_200_005_u64;
        let turn_nonce = "current-generation-live-nonce-5462";
        write_turn_start_marker(&pool, session_key, channel_id, turn_nonce, true).await;

        let state = inflight_row(channel_id, session_key, 546_200_501, turn_nonce);
        assert_eq!(
            state.born_generation, current_generation,
            "InflightTurnState::new must stamp the pinned process generation at birth"
        );
        save_inflight_state(&state).expect("seed current-generation live row");

        assert!(
            !consume_dispatched_origin_ghost_if_current(Some(&pool), &state).await,
            "a current-generation row is live ownership, not a consumable ghost"
        );
        let preserved = load_inflight_state(&ProviderKind::Claude, channel_id)
            .expect("the current-generation row must survive the reconcile clear");
        assert_eq!(preserved.born_generation, state.born_generation);
        let (status, origin_nonce) = session_row(&pool, session_key).await;
        assert_eq!(status, "turn_active");
        assert_eq!(origin_nonce.as_deref(), Some(turn_nonce));

        pool.close().await;
        pg_db.drop().await;
        crate::services::discord::runtime_store::set_process_generation_for_tests(None);
    }

    /// Return-value contract, `UserMsgMismatch` row: the marker is there, so the
    /// probe matches, but a newer turn now owns the inflight row. Neither the
    /// row nor the session may move — the release must not be layered on a
    /// clear that did not happen.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_ownership_proof_withholds_the_session_release() {
        let root = tempfile::tempdir().expect("runtime root");
        let _root_env = crate::config::set_agentdesk_root_for_test(root.path());
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "claude/test/ghost-superseded-5462";
        let channel_id = 546_200_003_u64;
        let ghost_nonce = "ghost-nonce-superseded-5462";
        write_turn_start_marker(&pool, session_key, channel_id, ghost_nonce, true).await;

        let newer = inflight_row(channel_id, session_key, 546_200_301, "newer-nonce-5462");
        save_inflight_state(&newer).expect("seed the newer turn's inflight row");
        let ghost = inflight_row(channel_id, session_key, 546_200_302, ghost_nonce);

        assert!(
            !consume_dispatched_origin_ghost_if_current(Some(&pool), &ghost).await,
            "a clear the caller does not own must not report a consumed ghost"
        );
        let preserved = load_inflight_state(&ProviderKind::Claude, channel_id)
            .expect("the newer turn's inflight row must survive");
        assert_eq!(preserved.user_msg_id, newer.user_msg_id);
        let (status, origin_nonce) = session_row(&pool, session_key).await;
        assert_eq!(
            status, "turn_active",
            "the session must not be released while the inflight row is still owned"
        );
        assert_eq!(origin_nonce.as_deref(), Some(ghost_nonce));

        pool.close().await;
        pg_db.drop().await;
    }

    /// Return-value contract, release-took-no-row: the clear succeeded but the
    /// session moved on, so this call consumed no ghost and must say so. The
    /// caller then keeps restoring — which is how the channel still gets a
    /// watcher after the row is gone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn release_that_takes_no_session_row_reports_no_consumed_ghost() {
        let root = tempfile::tempdir().expect("runtime root");
        let _root_env = crate::config::set_agentdesk_root_for_test(root.path());
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "claude/test/ghost-release-lost-5462";
        let channel_id = 546_200_004_u64;
        let turn_nonce = "ghost-nonce-release-lost-5462";
        write_turn_start_marker(&pool, session_key, channel_id, turn_nonce, true).await;

        let state = inflight_row(channel_id, session_key, 546_200_401, turn_nonce);
        save_inflight_state(&state).expect("seed the ghost's inflight row");

        // Stand in for a session that changed hands after the probe: an update
        // that reaches this row is skipped, so the release CAS reports zero
        // affected rows exactly as a nonce that moved on would.
        sqlx::query(
            "CREATE FUNCTION skip_session_release() RETURNS trigger
             LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$",
        )
        .execute(&pool)
        .await
        .expect("create release-skipping trigger function");
        sqlx::query(
            "CREATE TRIGGER skip_session_release_trigger BEFORE UPDATE ON sessions
             FOR EACH ROW EXECUTE FUNCTION skip_session_release()",
        )
        .execute(&pool)
        .await
        .expect("install release-skipping trigger");

        assert!(
            !consume_dispatched_origin_ghost_if_current(Some(&pool), &state).await,
            "a release that took no row must not tell the caller this channel is done"
        );
        let (status, origin_nonce) = session_row(&pool, session_key).await;
        assert_eq!(status, "turn_active");
        assert_eq!(origin_nonce.as_deref(), Some(turn_nonce));

        pool.close().await;
        pg_db.drop().await;
    }
}
