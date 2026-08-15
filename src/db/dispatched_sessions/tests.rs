use super::{
    RetryDispatchMeta, create_retry_dispatch_pg, disconnect_session_and_prepare_retry_pg,
    fail_force_killed_dispatch_without_retry_pg, prepare_retry_owner_on_pg_tx,
};
use crate::db::auto_queue::test_support::TestPostgresDb;
use sqlx::{PgPool, Row};

fn dispatch_observability_count(dispatch_id: &str, event_type: &str) -> usize {
    crate::services::observability::events::recent(usize::MAX)
        .into_iter()
        .filter(|event| {
            event.event_type == event_type && event.payload["dispatch_id"] == dispatch_id
        })
        .count()
}

async fn setup() -> (TestPostgresDb, PgPool) {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate_with_max_connections(12).await;
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ('retry-agent', 'Retry Agent', 'codex', 'retry-channel')
         ON CONFLICT (id) DO NOTHING",
    )
    .execute(&pool)
    .await
    .expect("seed retry agent");
    (database, pool)
}

async fn finish(database: TestPostgresDb, pool: PgPool) {
    pool.close().await;
    database.drop().await;
}

async fn seed_origin(pool: &PgPool, suffix: &str) -> (String, String) {
    let card_id = format!("retry-card-{suffix}");
    let dispatch_id = format!("retry-origin-{suffix}");
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
         VALUES ($1, $1, 'in_progress', 'retry-agent')",
    )
    .bind(&card_id)
    .execute(pool)
    .await
    .expect("seed retry card");
    sqlx::query(
        "INSERT INTO task_dispatches
            (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, retry_count)
         VALUES ($1, $2, 'retry-agent', 'implementation', 'dispatched', $1, '{}', 2)",
    )
    .bind(&dispatch_id)
    .bind(&card_id)
    .execute(pool)
    .await
    .expect("seed origin dispatch");
    (card_id, dispatch_id)
}

async fn seed_owner(
    pool: &PgPool,
    suffix: &str,
    card_id: &str,
    dispatch_id: &str,
    run_status: &str,
    entry_status: &str,
    current_link: bool,
) -> (String, String) {
    let run_id = format!("retry-run-{suffix}");
    let entry_id = format!("retry-entry-{suffix}");
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, 'repo', 'retry-agent', $2)",
    )
    .bind(&run_id)
    .bind(run_status)
    .execute(pool)
    .await
    .expect("seed retry run");
    sqlx::query(
        "INSERT INTO auto_queue_entries
            (id, run_id, kanban_card_id, agent_id, status, dispatch_id, retry_count, slot_index)
         VALUES ($1, $2, $3, 'retry-agent', $4, $5, 4, 3)",
    )
    .bind(&entry_id)
    .bind(&run_id)
    .bind(card_id)
    .bind(entry_status)
    .bind(current_link.then_some(dispatch_id))
    .execute(pool)
    .await
    .expect("seed retry entry");
    sqlx::query(
        "INSERT INTO auto_queue_entry_dispatch_history (entry_id, dispatch_id, trigger_source)
         VALUES ($1, $2, 'retry_test')",
    )
    .bind(&entry_id)
    .bind(dispatch_id)
    .execute(pool)
    .await
    .expect("seed retry history");
    sqlx::query(
        "INSERT INTO auto_queue_slots
            (agent_id, slot_index, assigned_run_id, assigned_thread_group)
         VALUES ('retry-agent', 3, $1, 0)
         ON CONFLICT (agent_id, slot_index) DO UPDATE
             SET assigned_run_id = EXCLUDED.assigned_run_id",
    )
    .bind(&run_id)
    .execute(pool)
    .await
    .expect("seed retry slot");
    (run_id, entry_id)
}

fn retry_meta(card_id: String, origin_dispatch_id: String) -> RetryDispatchMeta {
    RetryDispatchMeta {
        origin_dispatch_id,
        card_id,
        to_agent_id: Some("retry-agent".to_string()),
        dispatch_type: Some("implementation".to_string()),
        title: Some("replacement".to_string()),
        context: Some("{}".to_string()),
        retry_count: 2,
    }
}

async fn dispatch_status(pool: &PgPool, dispatch_id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
        .bind(dispatch_id)
        .fetch_one(pool)
        .await
        .expect("load dispatch status")
}

async fn entry_state(pool: &PgPool, entry_id: &str) -> (String, Option<String>, i64, Option<i64>) {
    let row = sqlx::query(
        "SELECT status, dispatch_id, retry_count::BIGINT AS retry_count,
                slot_index::BIGINT AS slot_index
         FROM auto_queue_entries WHERE id = $1",
    )
    .bind(entry_id)
    .fetch_one(pool)
    .await
    .expect("load entry state");
    (
        row.get("status"),
        row.get("dispatch_id"),
        row.get("retry_count"),
        row.get("slot_index"),
    )
}

#[tokio::test]
async fn owner_status_matrix_preserves_retry_budget_and_slot_pg() {
    let (database, pool) = setup().await;
    for status in ["dispatched", "pending", "skipped"] {
        let suffix = format!("matrix-{status}");
        let (card_id, origin_id) = seed_origin(&pool, &suffix).await;
        let (run_id, entry_id) =
            seed_owner(&pool, &suffix, &card_id, &origin_id, "active", status, true).await;
        let replacement =
            create_retry_dispatch_pg(&pool, &retry_meta(card_id.clone(), origin_id.clone()))
                .await
                .expect("create owned retry");
        assert_eq!(dispatch_status(&pool, &origin_id).await, "failed");
        assert_eq!(
            entry_state(&pool, &entry_id).await,
            ("dispatched".to_string(), Some(replacement), 4, Some(3))
        );
        let run_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = $1")
                .bind(run_id)
                .fetch_one(&pool)
                .await
                .expect("load live run");
        assert_eq!(run_status, "active");
    }
    finish(database, pool).await;
}

#[tokio::test]
async fn terminal_entries_and_non_live_runs_abort_without_revival_pg() {
    let (database, pool) = setup().await;
    for (index, entry_status) in ["done", "user_cancelled"].into_iter().enumerate() {
        let suffix = format!("terminal-entry-{index}");
        let (card_id, origin_id) = seed_origin(&pool, &suffix).await;
        seed_owner(
            &pool,
            &suffix,
            &card_id,
            &origin_id,
            "active",
            entry_status,
            true,
        )
        .await;
        assert!(
            create_retry_dispatch_pg(&pool, &retry_meta(card_id, origin_id.clone()))
                .await
                .is_err()
        );
        assert_eq!(dispatch_status(&pool, &origin_id).await, "dispatched");
    }
    for run_status in ["completed", "cancelled"] {
        let suffix = format!("terminal-run-{run_status}");
        let (card_id, origin_id) = seed_origin(&pool, &suffix).await;
        seed_owner(
            &pool, &suffix, &card_id, &origin_id, run_status, "failed", true,
        )
        .await;
        assert!(
            create_retry_dispatch_pg(&pool, &retry_meta(card_id, origin_id.clone()))
                .await
                .is_err()
        );
        assert_eq!(dispatch_status(&pool, &origin_id).await, "dispatched");
    }
    finish(database, pool).await;
}

#[tokio::test]
async fn history_filter_accepts_one_live_owner_and_rejects_ambiguity_pg() {
    let (database, pool) = setup().await;
    let (card_id, origin_id) = seed_origin(&pool, "history-filter").await;
    seed_owner(
        &pool,
        "history-stale",
        &card_id,
        &origin_id,
        "active",
        "skipped",
        false,
    )
    .await;
    let (_, live_entry) = seed_owner(
        &pool,
        "history-live",
        &card_id,
        &origin_id,
        "active",
        "dispatched",
        true,
    )
    .await;
    let replacement = create_retry_dispatch_pg(&pool, &retry_meta(card_id, origin_id.clone()))
        .await
        .expect("stale history must not mask live owner");
    assert_eq!(entry_state(&pool, &live_entry).await.1, Some(replacement));

    let (card_id, origin_id) = seed_origin(&pool, "history-ambiguous").await;
    seed_owner(
        &pool,
        "history-live-a",
        &card_id,
        &origin_id,
        "active",
        "dispatched",
        true,
    )
    .await;
    seed_owner(
        &pool,
        "history-live-b",
        &card_id,
        &origin_id,
        "active",
        "dispatched",
        true,
    )
    .await;
    assert!(
        create_retry_dispatch_pg(&pool, &retry_meta(card_id, origin_id.clone()))
            .await
            .expect_err("two live owners must abort")
            .contains("2 candidates")
    );
    assert_eq!(dispatch_status(&pool, &origin_id).await, "dispatched");
    finish(database, pool).await;
}

#[tokio::test]
async fn history_card_mismatch_aborts_before_dispatch_failure_pg() {
    let (database, pool) = setup().await;
    let (card_id, origin_id) = seed_origin(&pool, "card-mismatch").await;
    let wrong_card = "retry-card-card-mismatch-wrong";
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
         VALUES ($1, $1, 'in_progress', 'retry-agent')",
    )
    .bind(wrong_card)
    .execute(&pool)
    .await
    .expect("seed wrong card");
    seed_owner(
        &pool,
        "card-mismatch-owner",
        wrong_card,
        &origin_id,
        "active",
        "dispatched",
        true,
    )
    .await;
    assert!(
        create_retry_dispatch_pg(&pool, &retry_meta(card_id, origin_id.clone()))
            .await
            .expect_err("card mismatch must abort")
            .contains("card mismatch")
    );
    assert_eq!(dispatch_status(&pool, &origin_id).await, "dispatched");
    finish(database, pool).await;
}

#[tokio::test]
async fn stale_history_only_and_no_history_both_use_zero_owner_relaxation_pg() {
    let (database, pool) = setup().await;
    for stale_history in [false, true] {
        let suffix = format!("zero-owner-{stale_history}");
        let (card_id, origin_id) = seed_origin(&pool, &suffix).await;
        if stale_history {
            seed_owner(
                &pool, &suffix, &card_id, &origin_id, "active", "skipped", false,
            )
            .await;
        }
        let replacement =
            create_retry_dispatch_pg(&pool, &retry_meta(card_id.clone(), origin_id.clone()))
                .await
                .expect("zero-owner retry");
        assert_eq!(dispatch_status(&pool, &origin_id).await, "failed");
        assert_eq!(dispatch_status(&pool, &replacement).await, "pending");
        let attached: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM auto_queue_entries WHERE dispatch_id = $1")
                .bind(replacement)
                .fetch_one(&pool)
                .await
                .expect("count replacement owners");
        assert_eq!(attached, 0);
    }
    finish(database, pool).await;
}

#[tokio::test]
async fn concurrent_retries_for_one_origin_create_exactly_one_replacement_pg() {
    let (database, pool) = setup().await;
    let (card_id, origin_id) = seed_origin(&pool, "concurrent").await;
    let mut origin_blocker = pool.begin().await.expect("begin concurrent origin blocker");
    sqlx::query("SELECT id FROM task_dispatches WHERE id = $1 FOR NO KEY UPDATE")
        .bind(&origin_id)
        .fetch_one(&mut *origin_blocker)
        .await
        .expect("lock concurrent origin dispatch");
    let left = tokio::spawn({
        let pool = pool.clone();
        let meta = retry_meta(card_id.clone(), origin_id.clone());
        async move { create_retry_dispatch_pg(&pool, &meta).await }
    });
    wait_until_retry_blocks_on_origin(&pool).await;
    let right = tokio::spawn({
        let pool = pool.clone();
        let meta = retry_meta(card_id.clone(), origin_id.clone());
        async move { create_retry_dispatch_pg(&pool, &meta).await }
    });
    wait_until_retry_blocks_on_retry_token(&pool).await;
    origin_blocker
        .commit()
        .await
        .expect("release concurrent origin dispatch");
    let results = [
        left.await.expect("left retry"),
        right.await.expect("right retry"),
    ];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let replacements: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_dispatches
         WHERE kanban_card_id = $1 AND id <> $2",
    )
    .bind(card_id)
    .bind(origin_id)
    .fetch_one(&pool)
    .await
    .expect("count replacements");
    assert_eq!(replacements, 1);
    finish(database, pool).await;
}

#[tokio::test]
async fn retry_first_keeps_run_slot_and_cancel_later_cleans_replacement_pg() {
    let (database, pool) = setup().await;
    let (card_id, origin_id) = seed_origin(&pool, "retry-then-cancel").await;
    let (run_id, entry_id) = seed_owner(
        &pool,
        "retry-then-cancel",
        &card_id,
        &origin_id,
        "active",
        "dispatched",
        true,
    )
    .await;
    // This test intentionally checks the sequential retry-then-cancel outcome;
    // deterministic retry/cancel overlap is outside this test's scope.
    let replacement = create_retry_dispatch_pg(&pool, &retry_meta(card_id, origin_id))
        .await
        .expect("create replacement before cancel");
    let run_status: String = sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = $1")
        .bind(&run_id)
        .fetch_one(&pool)
        .await
        .expect("load run before cancel");
    assert_eq!(run_status, "active");
    let slot_owner: Option<String> = sqlx::query_scalar(
        "SELECT assigned_run_id FROM auto_queue_slots
         WHERE agent_id = 'retry-agent' AND slot_index = 3",
    )
    .fetch_one(&pool)
    .await
    .expect("load slot before cancel");
    assert_eq!(slot_owner.as_deref(), Some(run_id.as_str()));
    let completion_notifies: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM message_outbox WHERE content LIKE '자동큐 완료:%'",
    )
    .fetch_one(&pool)
    .await
    .expect("count premature completion notifications");
    assert_eq!(completion_notifies, 0);

    crate::services::auto_queue::cancel_run::cancel_selected_runs_with_pg(
        None,
        &pool,
        std::slice::from_ref(&run_id),
        "retry_test_cancel",
    )
    .await
    .expect("cancel retry owner run");
    assert_eq!(dispatch_status(&pool, &replacement).await, "cancelled");
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM auto_queue_entries
         WHERE run_id = $1 AND status IN ('pending', 'dispatched')",
    )
    .bind(&run_id)
    .fetch_one(&pool)
    .await
    .expect("count live entries after cancel");
    assert_eq!(pending, 0);
    assert_ne!(entry_state(&pool, &entry_id).await.0, "pending");
    finish(database, pool).await;
}

#[tokio::test]
async fn cancel_first_rejects_retry_and_retry_without_flag_preserves_finalization_pg() {
    let (database, pool) = setup().await;
    let (card_id, origin_id) = seed_origin(&pool, "cancel-first").await;
    let (run_id, _) = seed_owner(
        &pool,
        "cancel-first",
        &card_id,
        &origin_id,
        "active",
        "dispatched",
        true,
    )
    .await;
    crate::services::auto_queue::cancel_run::cancel_selected_runs_with_pg(
        None,
        &pool,
        std::slice::from_ref(&run_id),
        "retry_test_cancel_first",
    )
    .await
    .expect("cancel before retry");
    assert!(
        create_retry_dispatch_pg(&pool, &retry_meta(card_id, origin_id))
            .await
            .is_err()
    );

    let (card_id, origin_id) = seed_origin(&pool, "no-retry-finalize").await;
    let (run_id, _) = seed_owner(
        &pool,
        "no-retry-finalize",
        &card_id,
        &origin_id,
        "active",
        "dispatched",
        true,
    )
    .await;
    assert_eq!(
        fail_force_killed_dispatch_without_retry_pg(&pool, &origin_id, None)
            .await
            .expect("standalone failure"),
        1
    );
    let run_status: String = sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = $1")
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .expect("load finalized run");
    assert_eq!(run_status, "completed");
    finish(database, pool).await;
}

async fn wait_until_retry_blocks_on_origin(pool: &PgPool) {
    for _ in 0..100 {
        let blocked: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM pg_stat_activity
                 WHERE datname = current_database()
                   AND pid <> pg_backend_pid()
                   AND wait_event_type = 'Lock'
                   AND query LIKE 'UPDATE task_dispatches%'
             )",
        )
        .fetch_one(pool)
        .await
        .expect("inspect blocked retry");
        if blocked {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("retry did not block on the origin dispatch row");
}

async fn wait_until_retry_blocks_on_retry_token(pool: &PgPool) {
    for _ in 0..100 {
        let blocked: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1
                 FROM pg_stat_activity a
                 JOIN pg_locks l ON l.pid = a.pid
                 WHERE a.datname = current_database()
                   AND a.pid <> pg_backend_pid()
                   AND a.wait_event_type = 'Lock'
                   AND a.query LIKE 'SELECT pg_advisory_xact_lock%aq_retry%'
                   AND l.locktype = 'advisory'
                   AND NOT l.granted
             )",
        )
        .fetch_one(pool)
        .await
        .expect("inspect contended retry token");
        if blocked {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("second retry did not contend on the origin retry token");
}

#[tokio::test]
async fn zero_owner_revalidation_aborts_on_new_visible_ownership_without_late_run_lock_pg() {
    let (database, pool) = setup().await;
    let (card_id, origin_id) = seed_origin(&pool, "revalidate").await;
    let (late_run_id, late_entry_id) = seed_owner(
        &pool,
        "revalidate-late-owner",
        &card_id,
        &origin_id,
        "active",
        "dispatched",
        true,
    )
    .await;
    sqlx::query("DELETE FROM auto_queue_entry_dispatch_history WHERE entry_id = $1")
        .bind(&late_entry_id)
        .execute(&pool)
        .await
        .expect("hide owner history before retry lookup");
    let mut blocker = pool.begin().await.expect("begin origin blocker");
    sqlx::query("SELECT id FROM task_dispatches WHERE id = $1 FOR NO KEY UPDATE")
        .bind(&origin_id)
        .fetch_one(&mut *blocker)
        .await
        .expect("lock origin dispatch without blocking history FK");
    let retry = tokio::spawn({
        let pool = pool.clone();
        let meta = retry_meta(card_id.clone(), origin_id.clone());
        async move { create_retry_dispatch_pg(&pool, &meta).await }
    });
    wait_until_retry_blocks_on_origin(&pool).await;
    sqlx::query(
        "INSERT INTO auto_queue_entry_dispatch_history (entry_id, dispatch_id, trigger_source)
         VALUES ($1, $2, 'late_retry_test')",
    )
    .bind(&late_entry_id)
    .bind(&origin_id)
    .execute(&pool)
    .await
    .expect("publish owner history before retry revalidation");
    let mut run_blocker = pool.begin().await.expect("begin late run-token blocker");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('aq_run:' || $1))")
        .bind(&late_run_id)
        .execute(&mut *run_blocker)
        .await
        .expect("hold late owner run token");
    blocker.commit().await.expect("release origin blocker");
    let error = tokio::time::timeout(std::time::Duration::from_secs(1), retry)
        .await
        .expect("revalidation must not wait on the late owner run token")
        .expect("join retry")
        .expect_err("visible ownership must abort zero-owner retry");
    assert!(error.contains("ownership appeared"));
    assert_eq!(dispatch_status(&pool, &origin_id).await, "dispatched");
    run_blocker
        .rollback()
        .await
        .expect("release late run token");
    finish(database, pool).await;
}

#[tokio::test]
async fn precommit_backend_crash_rolls_back_failure_after_session_disconnect_pg() {
    let (database, pool) = setup().await;
    let (card_id, origin_id) = seed_origin(&pool, "crash").await;
    seed_owner(
        &pool,
        "crash",
        &card_id,
        &origin_id,
        "active",
        "dispatched",
        true,
    )
    .await;
    let session_key = "retry-crash-session";
    sqlx::query(
        "INSERT INTO sessions (session_key, agent_id, status, active_dispatch_id)
         VALUES ($1, 'retry-agent', 'turn_active', $2)",
    )
    .bind(session_key)
    .bind(&origin_id)
    .execute(&pool)
    .await
    .expect("seed retry session");
    let meta = disconnect_session_and_prepare_retry_pg(&pool, session_key, Some(&origin_id), true)
        .await
        .expect("disconnect and prepare retry")
        .expect("retry metadata");

    let mut tx = pool.begin().await.expect("begin doomed retry tx");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('aq_retry:' || $1))")
        .bind(&origin_id)
        .execute(&mut *tx)
        .await
        .expect("lock retry token");
    prepare_retry_owner_on_pg_tx(&mut tx, &meta)
        .await
        .expect("prepare retry owner")
        .expect("retry owner");
    let backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *tx)
        .await
        .expect("load retry backend pid");
    let mut deferred_observability = Vec::new();
    crate::dispatch::set_dispatch_status_on_pg_tx_async(
        &mut tx,
        &origin_id,
        "failed",
        Some(&serde_json::json!({"reason": "crash_test"})),
        "force_kill_session",
        Some(&["pending", "dispatched"]),
        true,
        true,
        Some(&mut deferred_observability),
    )
    .await
    .expect("stage failed sync");
    assert_eq!(dispatch_status(&pool, &origin_id).await, "dispatched");
    let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(backend_pid)
        .fetch_one(&pool)
        .await
        .expect("terminate retry backend");
    assert!(terminated);
    drop(tx);
    assert_eq!(dispatch_status(&pool, &origin_id).await, "dispatched");
    assert_eq!(
        dispatch_observability_count(&origin_id, "dispatch_result"),
        0
    );
    assert_eq!(
        dispatch_observability_count(&origin_id, "agent_quality_event"),
        0
    );
    let session =
        sqlx::query("SELECT status, active_dispatch_id FROM sessions WHERE session_key = $1")
            .bind(session_key)
            .fetch_one(&pool)
            .await
            .expect("load disconnected session");
    assert_eq!(session.get::<String, _>("status"), "disconnected");
    assert_eq!(session.get::<Option<String>, _>("active_dispatch_id"), None);
    assert_eq!(
        fail_force_killed_dispatch_without_retry_pg(&pool, &origin_id, None)
            .await
            .expect("standalone failure fallback"),
        1
    );
    assert_eq!(dispatch_status(&pool, &origin_id).await, "failed");
    assert_eq!(
        dispatch_observability_count(&origin_id, "dispatch_result"),
        1
    );
    assert_eq!(
        dispatch_observability_count(&origin_id, "agent_quality_event"),
        1
    );
    finish(database, pool).await;
}
