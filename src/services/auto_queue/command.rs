use super::*;

pub(super) async fn cancel_selected_runs_with_pg(
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &sqlx::PgPool,
    target_run_ids: &[String],
    reason: &str,
) -> Result<serde_json::Value, String> {
    crate::services::auto_queue::cancel_run::cancel_selected_runs_with_pg(
        health_registry,
        pool,
        target_run_ids,
        reason,
    )
    .await
}

pub(super) async fn reset_scoped_with_pg(
    agent_id: &str,
    pool: &sqlx::PgPool,
) -> Result<serde_json::Value, String> {
    let deleted_entries = sqlx::query("DELETE FROM auto_queue_entries WHERE agent_id = $1")
        .bind(agent_id)
        .execute(pool)
        .await
        .map_err(|error| format!("delete auto_queue_entries for agent {agent_id}: {error}"))?
        .rows_affected() as usize;
    let completed_runs = sqlx::query(
        "UPDATE auto_queue_runs
             SET status = 'completed',
                 completed_at = NOW()
             WHERE status IN ('generated', 'pending', 'active', 'paused')
               AND agent_id = $1",
    )
    .bind(agent_id)
    .execute(pool)
    .await
    .map_err(|error| format!("complete auto_queue_runs for agent {agent_id}: {error}"))?
    .rows_affected() as usize;
    Ok(json!({
        "ok": true,
        "deleted_entries": deleted_entries,
        "completed_runs": completed_runs,
        "protected_active_runs": 0usize,
    }))
}

pub(super) async fn reset_global_with_pg(pool: &sqlx::PgPool) -> Result<serde_json::Value, String> {
    let protected_active_runs = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM auto_queue_runs WHERE status = 'active'",
    )
    .fetch_one(pool)
    .await
    .map_err(|error| format!("count active auto_queue_runs: {error}"))?;
    if protected_active_runs > 0 {
        crate::auto_queue_log!(
            warn,
            "reset_global_preserved_active_runs",
            AutoQueueLogContext::new(),
            "[auto-queue] Global PG reset requested without agent_id; preserving {protected_active_runs} active run(s)"
        );
    } else {
        crate::auto_queue_log!(
            warn,
            "reset_global_unscoped",
            AutoQueueLogContext::new(),
            "[auto-queue] Global PG reset requested without agent_id; applying unscoped reset"
        );
    }

    let deleted_entries = if protected_active_runs > 0 {
        sqlx::query(
            "DELETE FROM auto_queue_entries
                 WHERE run_id IS NULL
                    OR run_id NOT IN (
                        SELECT id FROM auto_queue_runs WHERE status = 'active'
                    )",
        )
        .execute(pool)
        .await
        .map_err(|error| format!("delete inactive auto_queue_entries: {error}"))?
        .rows_affected() as usize
    } else {
        sqlx::query("DELETE FROM auto_queue_entries")
            .execute(pool)
            .await
            .map_err(|error| format!("delete all auto_queue_entries: {error}"))?
            .rows_affected() as usize
    };
    let completed_runs = if protected_active_runs > 0 {
        sqlx::query(
            "UPDATE auto_queue_runs
                 SET status = 'completed',
                     completed_at = NOW()
                 WHERE status IN ('generated', 'pending', 'paused')",
        )
        .execute(pool)
        .await
        .map_err(|error| format!("complete inactive auto_queue_runs: {error}"))?
        .rows_affected() as usize
    } else {
        sqlx::query(
            "UPDATE auto_queue_runs
                 SET status = 'completed',
                     completed_at = NOW()
                 WHERE status IN ('generated', 'pending', 'active', 'paused')",
        )
        .execute(pool)
        .await
        .map_err(|error| format!("complete all auto_queue_runs: {error}"))?
        .rows_affected() as usize
    };
    let warning = (protected_active_runs > 0).then(|| {
        format!(
            "global reset preserved {protected_active_runs} active run(s); use agent_id to reset a specific queue"
        )
    });

    let mut response = json!({
        "ok": true,
        "deleted_entries": deleted_entries,
        "completed_runs": completed_runs,
        "protected_active_runs": protected_active_runs,
    });
    if let Some(warning) = warning {
        response["warning"] = json!(warning);
    }
    Ok(response)
}

pub(super) fn parse_json_body<T: DeserializeOwned>(body: Bytes, label: &str) -> Result<T, String> {
    if body.is_empty() {
        serde_json::from_slice(b"{}").map_err(|error| format!("invalid {label} body: {error}"))
    } else {
        serde_json::from_slice(&body).map_err(|error| format!("invalid {label} body: {error}"))
    }
}

pub(super) async fn update_run_with_pg(
    run_id: &str,
    body: &UpdateRunBody,
    pool: &sqlx::PgPool,
) -> Result<(usize, Option<bool>), String> {
    let mut changed = 0usize;
    let mut status_changed = None;

    if let Some(ref status) = body.status {
        let result = if status == "active" {
            sqlx::query(
                "UPDATE auto_queue_runs
                 SET status = 'active',
                     completed_at = NULL
                 WHERE id = $1
                   AND status = 'pending'",
            )
            .bind(run_id)
            .execute(pool)
            .await
            .map_err(|error| format!("start pending postgres auto_queue_run {run_id}: {error}"))?
        } else if status == "completed" {
            sqlx::query(
                "UPDATE auto_queue_runs
                 SET status = $1,
                     completed_at = NOW()
                 WHERE id = $2",
            )
            .bind(status)
            .bind(run_id)
            .execute(pool)
            .await
            .map_err(|error| {
                format!("update postgres auto_queue_runs status for {run_id}: {error}")
            })?
        } else {
            sqlx::query(
                "UPDATE auto_queue_runs
                 SET status = $1,
                     completed_at = NULL
                 WHERE id = $2",
            )
            .bind(status)
            .bind(run_id)
            .execute(pool)
            .await
            .map_err(|error| {
                format!("update postgres auto_queue_runs status for {run_id}: {error}")
            })?
        };
        let status_rows = result.rows_affected() as usize;
        status_changed = Some(status_rows > 0);
        changed += status_rows;
        if status == "active" && status_rows == 0 {
            return Ok((changed, status_changed));
        }
    }

    if let Some(max_concurrent_threads) = body.max_concurrent_threads {
        let result = sqlx::query(
            "UPDATE auto_queue_runs
             SET max_concurrent_threads = $1
             WHERE id = $2",
        )
        .bind(max_concurrent_threads)
        .bind(run_id)
        .execute(pool)
        .await
        .map_err(|error| {
            format!("update postgres auto_queue_runs max_concurrent_threads for {run_id}: {error}")
        })?;
        changed += result.rows_affected() as usize;
    }

    Ok((changed, status_changed))
}

pub(super) async fn reorder_with_pg(body: &ReorderBody, pool: &sqlx::PgPool) -> Result<(), String> {
    let mut run_id = None;
    for id in &body.ordered_ids {
        let found = sqlx::query_scalar::<_, String>(
            "SELECT run_id
             FROM auto_queue_entries
             WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load auto_queue_entries run_id for {id}: {error}"))?;
        if found.is_some() {
            run_id = found;
            break;
        }
    }

    let Some(run_id) = run_id else {
        return Err("not_found:no matching queue entries found".to_string());
    };

    let current_entries: Vec<QueueEntryOrder> = sqlx::query(
        "SELECT id,
                COALESCE(status, 'pending') AS status,
                COALESCE(agent_id, '') AS agent_id
         FROM auto_queue_entries
         WHERE run_id = $1
         ORDER BY priority_rank ASC, created_at ASC, id ASC",
    )
    .bind(&run_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load auto_queue_entries for reorder run {run_id}: {error}"))?
    .into_iter()
    .map(|row| {
        Ok(QueueEntryOrder {
            id: row
                .try_get("id")
                .map_err(|error| format!("decode reorder entry id: {error}"))?,
            status: row
                .try_get("status")
                .map_err(|error| format!("decode reorder entry status: {error}"))?,
            agent_id: row
                .try_get("agent_id")
                .map_err(|error| format!("decode reorder entry agent_id: {error}"))?,
        })
    })
    .collect::<Result<Vec<_>, String>>()?;

    let reordered_ids = reorder_entry_ids(
        &current_entries,
        &body.ordered_ids,
        body.agent_id.as_deref(),
    )?;

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin reorder transaction: {error}"))?;
    // #2048 F19: when a `body.agent_id` is provided we must NOT renumber
    // entries belonging to other agents in the same run. `reorder_entry_ids`
    // already filters its output to the scoped set, but the UPDATE itself
    // needs the same scope guard so a stale set member (e.g. an entry that
    // changed agent between load and update) cannot leak the renumber into
    // another agent's queue. Without agent_id scope, the global-run reorder
    // path remains.
    for (rank, id) in reordered_ids.iter().enumerate() {
        if let Some(agent_id) = body.agent_id.as_deref() {
            sqlx::query(
                "UPDATE auto_queue_entries
                 SET priority_rank = $1
                 WHERE id = $2
                   AND agent_id = $3",
            )
            .bind(rank as i64)
            .bind(id)
            .bind(agent_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                format!("update auto_queue_entries priority_rank for {id}: {error}")
            })?;
        } else {
            sqlx::query("UPDATE auto_queue_entries SET priority_rank = $1 WHERE id = $2")
                .bind(rank as i64)
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    format!("update auto_queue_entries priority_rank for {id}: {error}")
                })?;
        }
    }
    tx.commit()
        .await
        .map_err(|error| format!("commit reorder transaction: {error}"))?;

    Ok(())
}

pub(super) async fn soft_pause_with_pg(pool: &sqlx::PgPool) -> Result<serde_json::Value, String> {
    let paused = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'paused',
             completed_at = NULL
         WHERE status = 'active'",
    )
    .execute(pool)
    .await
    .map_err(|error| format!("pause postgres auto_queue_runs: {error}"))?
    .rows_affected() as usize;

    Ok(json!({
        "ok": true,
        "paused_runs": paused,
        "cancelled_dispatches": 0usize,
        "released_slots": 0usize,
        "cleared_slot_sessions": 0usize,
    }))
}

pub(super) async fn force_pause_with_pg(
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &sqlx::PgPool,
) -> Result<serde_json::Value, String> {
    // #2048 F2: capture the run set AND flip them to `paused` in a single
    // transaction, taking the same per-run advisory lock that
    // `create_activate_dispatch_pg_inner` (F1) uses for its paused-run
    // re-check. This closes two races:
    //   1. runs that became `active` AFTER our snapshot were previously
    //      also flipped to `paused` by a broad `WHERE status='active'`
    //      UPDATE without being included in cleanup, leaving zombie live
    //      dispatches on a paused run.
    //   2. follow-up dispatches created concurrently on these runs went
    //      uncancelled because cleanup operated on a stale run set.
    // After this, cleanup runs on exactly the run set we paused, and any
    // concurrent dispatch-create path observes `paused` under the lock and
    // refuses to insert.
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin force_pause transaction: {error}"))?;
    let pause_candidates = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM auto_queue_runs
         WHERE status = 'active'",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(|error| format!("load active runs for force_pause: {error}"))?;
    let locked_run_ids =
        crate::db::auto_queue::acquire_run_advisory_xact_locks_on_pg_tx(&mut tx, &pause_candidates)
            .await?;
    let pause_target_ids = sqlx::query_scalar::<_, String>(
        "UPDATE auto_queue_runs
         SET status = 'paused',
             completed_at = NULL
         WHERE id = ANY($1)
           AND status = 'active'
         RETURNING id",
    )
    .bind(&locked_run_ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(|error| format!("flip active runs to paused for force_pause: {error}"))?;
    tx.commit()
        .await
        .map_err(|error| format!("commit force_pause flip transaction: {error}"))?;

    let paused = pause_target_ids.len();

    let cleanup = crate::services::auto_queue::cancel_run::cancel_and_release_runs_with_pg(
        health_registry,
        pool,
        &pause_target_ids,
        "auto_queue_pause",
        Some("run_pause_orphan_self_heal"),
    )
    .await?;
    let _deleted_phase_gates =
        crate::services::auto_queue::cancel_run::delete_phase_gate_rows_for_runs_pg(
            pool,
            &pause_target_ids,
        )
        .await?;
    let _skipped_entries =
        crate::services::auto_queue::cancel_run::skip_dispatched_entries_for_runs_pg(
            pool,
            &pause_target_ids,
            "run_pause",
        )
        .await?;

    let mut response = json!({
        "ok": true,
        "paused_runs": paused,
        "cancelled_dispatches": cleanup.cancelled_dispatches,
        "released_slots": cleanup.slot_cleanup.released_slots,
        "cleared_slot_sessions": cleanup.slot_cleanup.cleared_slot_sessions,
    });
    if let Some(warning) = crate::services::auto_queue::cancel_run::slot_cleanup_warning(
        &cleanup.slot_cleanup.warnings,
    ) {
        response["warning"] = json!(warning);
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn wait_for_advisory_waiters(
        conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>,
        expected: i64,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
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
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "expected {expected} advisory-lock waiters, observed {waiting}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
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
        let pause = tokio::spawn(async move { force_pause_with_pg(None, &pause_pool).await });
        wait_for_advisory_waiters(&mut holder, 1).await;

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
        wait_for_advisory_waiters(&mut holder, 2).await;

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
}
