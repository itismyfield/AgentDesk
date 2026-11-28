use crate::services::auto_queue::route::command::force_pause_with_pg;
use crate::test_support::wait::poll_within;

async fn advisory_waiters_sample(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>,
    expected: i64,
) -> Result<(), String> {
    sqlx::query("SELECT pg_stat_clear_snapshot()")
        .execute(&mut **conn)
        .await
        .expect("clear advisory-waiter snapshot");
    let waiting = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
             FROM pg_stat_activity
             WHERE datname = current_database()
               AND wait_event_type = 'Lock'
               AND query LIKE '%pg_advisory_xact_lock%'",
    )
    .fetch_one(&mut **conn)
    .await
    .expect("count advisory-lock waiters");
    if waiting >= expected {
        Ok(())
    } else {
        Err(format!(
            "expected {expected} advisory-lock waiters, observed {waiting}"
        ))
    }
}

async fn seed_inverse_run_orders(pool: &sqlx::PgPool) {
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('lock-order-agent', 'Lock Order Agent', 'claude', 'lock-order-channel')",
    )
    .execute(pool)
    .await
    .expect("seed lock-order agent");
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, completed_at)
             VALUES ('lock-order-card', 'Lock Order Card', 'done', 'lock-order-agent', NOW())",
    )
    .execute(pool)
    .await
    .expect("seed lock-order card");
    sqlx::query(
        "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, completed_at)
             VALUES
                ('lock-order-dispatch', 'lock-order-card', 'lock-order-agent',
                 'implementation', 'completed', NOW())",
    )
    .execute(pool)
    .await
    .expect("seed lock-order dispatch");
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at)
             VALUES
                ('lock-run-z', 'lock-order-repo', 'lock-order-agent', 'active', NOW() - INTERVAL '1 hour'),
                ('lock-run-a', 'lock-order-repo', 'lock-order-agent', 'active', NOW())",
    )
    .execute(pool)
    .await
    .expect("seed inverse-id/creation-order runs");
    sqlx::query(
        "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, completed_at)
             VALUES
                ('lock-entry-z', 'lock-run-z', 'lock-order-card', 'lock-order-agent',
                 'done', 'lock-order-dispatch', NOW()),
                ('lock-entry-a', 'lock-run-a', 'lock-order-card', 'lock-order-agent',
                 'done', 'lock-order-dispatch', NOW())",
    )
    .execute(pool)
    .await
    .expect("seed inverse-order done entries");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_and_force_pause_share_multi_run_advisory_order_pg() {
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "agentdesk_lock_order",
        "reopen and force-pause advisory order",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(8).await;
    seed_inverse_run_orders(&pool).await;

    let mut holder = pool.acquire().await.expect("acquire run-token holder");
    sqlx::query("BEGIN")
        .execute(&mut *holder)
        .await
        .expect("begin run-token holder");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('aq_run:' || 'lock-run-z'))")
        .execute(&mut *holder)
        .await
        .expect("hold first created run token");

    let pause_pool = pool.clone();
    let pause = tokio::spawn(async move {
        force_pause_with_pg(
            crate::services::discord::health::slot_row_host_refusal,
            None,
            &pause_pool,
        )
        .await
    });
    poll_within(
        async || advisory_waiters_sample(&mut holder, 1).await,
        std::time::Duration::from_secs(5),
        "PostgreSQL lock waiter",
    )
    .await
    .unwrap();

    let reopen_pool = pool.clone();
    let reopen = tokio::spawn(async move {
        let mut tx = reopen_pool.begin().await.expect("begin reopen transaction");
        let result = crate::engine::ops::reopen_done_auto_queue_entries_on_pg_tx(
            &mut tx,
            "lock-order-card",
            "pmd_reopen",
        )
        .await;
        match result {
            Ok(()) => tx.commit().await.map_err(|error| error.to_string()),
            Err(error) => {
                let _ = tx.rollback().await;
                Err(error)
            }
        }
    });
    poll_within(
        async || advisory_waiters_sample(&mut holder, 2).await,
        std::time::Duration::from_secs(5),
        "PostgreSQL lock waiter",
    )
    .await
    .unwrap();

    sqlx::query("COMMIT")
        .execute(&mut *holder)
        .await
        .expect("release first created run token");
    let (pause_result, reopen_result) =
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            tokio::join!(pause, reopen)
        })
        .await
        .expect("cross-path lock acquisition completes without timeout");
    pause_result
        .expect("join force-pause task")
        .expect("force-pause completes without deadlock");
    reopen_result
        .expect("join reopen task")
        .expect("reopen completes without deadlock");

    drop(holder);
    pool.close().await;
    db.drop().await;
}
