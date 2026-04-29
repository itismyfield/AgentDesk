#[derive(Debug, Default)]
struct SlotCleanupResult {
    released_slots: usize,
    cleared_slot_sessions: usize,
    warnings: Vec<String>,
}

async fn load_run_ids_with_status_pg(
    pool: &sqlx::PgPool,
    statuses: &[&str],
) -> Result<Vec<String>, String> {
    if statuses.is_empty() {
        return Ok(Vec::new());
    }

    let mut query =
        QueryBuilder::<Postgres>::new("SELECT id FROM auto_queue_runs WHERE status IN (");
    let mut separated = query.separated(", ");
    for status in statuses {
        separated.push_bind(*status);
    }
    separated.push_unseparated(") ORDER BY created_at ASC, id ASC");
    query
        .build_query_scalar::<String>()
        .fetch_all(pool)
        .await
        .map_err(|error| format!("load postgres auto_queue_runs by status: {error}"))
}

async fn load_slot_bindings_for_runs_pg(
    pool: &sqlx::PgPool,
    run_ids: &[String],
) -> Result<Vec<(String, String, i64)>, String> {
    if run_ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut query = QueryBuilder::<Postgres>::new(
        "SELECT DISTINCT assigned_run_id, agent_id, slot_index
         FROM auto_queue_slots
         WHERE assigned_run_id IN (",
    );
    let mut separated = query.separated(", ");
    for run_id in run_ids {
        separated.push_bind(run_id);
    }
    separated.push_unseparated(") AND assigned_run_id IS NOT NULL");

    let rows = query
        .build()
        .fetch_all(pool)
        .await
        .map_err(|error| format!("load postgres slot bindings for runs: {error}"))?;

    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get::<String, _>("assigned_run_id")
                    .map_err(|error| format!("decode postgres assigned_run_id: {error}"))?,
                row.try_get::<String, _>("agent_id")
                    .map_err(|error| format!("decode postgres slot agent_id: {error}"))?,
                row.try_get::<i64, _>("slot_index")
                    .map_err(|error| format!("decode postgres slot_index: {error}"))?,
            ))
        })
        .collect()
}

async fn load_live_dispatch_ids_for_runs_pg(
    pool: &sqlx::PgPool,
    run_ids: &[String],
) -> Result<Vec<String>, String> {
    if run_ids.is_empty() {
        return Ok(Vec::new());
    }

    sqlx::query_scalar(
        "SELECT DISTINCT td.id
         FROM task_dispatches td
         WHERE td.status IN ('pending', 'dispatched')
           AND (
               EXISTS (
                   SELECT 1
                   FROM auto_queue_entries e
                   WHERE e.dispatch_id = td.id
                     AND e.run_id = ANY($1)
               )
               OR EXISTS (
                   SELECT 1
                   FROM auto_queue_phase_gates pg
                   WHERE pg.dispatch_id = td.id
                     AND pg.run_id = ANY($1)
               )
               OR (
                   CASE
                       WHEN td.context IS NULL OR BTRIM(td.context) = '' THEN NULL
                       ELSE (td.context::jsonb #>> '{phase_gate,run_id}')
                   END
               ) = ANY($1)
           )
         ORDER BY td.id",
    )
    .bind(run_ids)
    .fetch_all(pool)
    .await
    .map_err(|error| {
        format!(
            "load postgres live dispatch ids for runs {:?}: {error}",
            run_ids
        )
    })
}

async fn load_dispatched_card_ids_for_runs_pg(
    pool: &sqlx::PgPool,
    run_ids: &[String],
) -> Result<Vec<String>, String> {
    if run_ids.is_empty() {
        return Ok(Vec::new());
    }

    sqlx::query_scalar(
        "SELECT DISTINCT e.kanban_card_id
         FROM auto_queue_entries e
         WHERE e.run_id = ANY($1)
           AND e.status = 'dispatched'
           AND e.kanban_card_id IS NOT NULL
           AND BTRIM(e.kanban_card_id) <> ''
         ORDER BY e.kanban_card_id",
    )
    .bind(run_ids)
    .fetch_all(pool)
    .await
    .map_err(|error| {
        format!(
            "load postgres dispatched card ids for runs {:?}: {error}",
            run_ids
        )
    })
}

async fn delete_phase_gate_rows_for_runs_pg(
    pool: &sqlx::PgPool,
    run_ids: &[String],
) -> Result<usize, String> {
    if run_ids.is_empty() {
        return Ok(0);
    }

    let mut query =
        QueryBuilder::<Postgres>::new("DELETE FROM auto_queue_phase_gates WHERE run_id IN (");
    let mut separated = query.separated(", ");
    for run_id in run_ids {
        separated.push_bind(run_id);
    }
    separated.push_unseparated(")");

    query
        .build()
        .execute(pool)
        .await
        .map(|result| result.rows_affected() as usize)
        .map_err(|error| format!("delete postgres auto_queue_phase_gates: {error}"))
}

async fn count_live_dispatches_for_runs_pg(
    pool: &sqlx::PgPool,
    run_ids: &[String],
) -> Result<i64, String> {
    load_live_dispatch_ids_for_runs_pg(pool, run_ids)
        .await
        .map(|rows| rows.len() as i64)
}

async fn cancel_live_dispatches_for_runs_pg(
    pool: &sqlx::PgPool,
    run_ids: &[String],
    reason: &str,
) -> Result<usize, String> {
    let dispatch_ids = load_live_dispatch_ids_for_runs_pg(pool, run_ids).await?;
    let mut cancelled = 0usize;
    for dispatch_id in dispatch_ids {
        cancelled += crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg(
            pool,
            &dispatch_id,
            Some(reason),
        )
        .await?;
    }
    Ok(cancelled)
}

async fn clear_sessions_for_dispatches_pg(
    pool: &sqlx::PgPool,
    dispatch_ids: &[String],
) -> Result<usize, String> {
    let mut cleared_sessions = 0usize;
    for dispatch_id in dispatch_ids {
        let result = sqlx::query(
            "UPDATE sessions
             SET status = 'idle',
                 active_dispatch_id = NULL,
                 session_info = $1,
                 claude_session_id = NULL,
                 tokens = 0,
                 last_heartbeat = NOW()
             WHERE active_dispatch_id = $2
               AND status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working', 'idle')",
        )
        .bind("Dispatch cancelled")
        .bind(dispatch_id)
        .execute(pool)
        .await
        .map_err(|error| {
            format!("clear postgres sessions for cancelled dispatch {dispatch_id}: {error}")
        })?;
        cleared_sessions += result.rows_affected() as usize;
    }
    Ok(cleared_sessions)
}

async fn transition_entry_to_skipped_pg(
    pool: &sqlx::PgPool,
    entry_id: &str,
    trigger_source: &str,
) -> Result<bool, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres entry skip transaction {entry_id}: {error}"))?;

    let current_status = sqlx::query_scalar::<_, Option<String>>(
        "SELECT status
         FROM auto_queue_entries
         WHERE id = $1",
    )
    .bind(entry_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| format!("load postgres entry status {entry_id}: {error}"))?
    .flatten();
    let Some(current_status) = current_status else {
        tx.rollback()
            .await
            .map_err(|error| format!("rollback missing postgres entry {entry_id}: {error}"))?;
        return Ok(false);
    };
    if !matches!(current_status.as_str(), "pending" | "dispatched") {
        tx.rollback().await.map_err(|error| {
            format!("rollback non-skippable postgres entry {entry_id}: {error}")
        })?;
        return Ok(false);
    }

    let changed = sqlx::query(
        "UPDATE auto_queue_entries
         SET status = 'skipped',
             dispatch_id = NULL,
             dispatched_at = NULL,
             completed_at = NOW()
         WHERE id = $1
           AND status = $2",
    )
    .bind(entry_id)
    .bind(&current_status)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("skip postgres entry {entry_id}: {error}"))?
    .rows_affected() as usize;
    if changed == 0 {
        tx.rollback()
            .await
            .map_err(|error| format!("rollback unchanged postgres entry {entry_id}: {error}"))?;
        return Ok(false);
    }

    let _ = sqlx::query(
        "INSERT INTO auto_queue_entry_transitions (
            entry_id,
            from_status,
            to_status,
            trigger_source
        ) VALUES ($1, $2, 'skipped', $3)",
    )
    .bind(entry_id)
    .bind(&current_status)
    .bind(trigger_source)
    .execute(&mut *tx)
    .await;

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres entry skip {entry_id}: {error}"))?;
    Ok(true)
}

async fn clear_and_release_slots_for_runs_pg(
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &sqlx::PgPool,
    run_ids: &[String],
) -> SlotCleanupResult {
    let mut released_slots: HashSet<(String, i64)> = HashSet::new();
    let mut released_slot_count = 0usize;
    let mut cleared_sessions = 0usize;
    let mut warnings = Vec::new();
    for run_id in run_ids {
        match sqlx::query(
            "UPDATE auto_queue_slots
             SET assigned_run_id = NULL,
                 assigned_thread_group = NULL,
                 updated_at = NOW()
             WHERE assigned_run_id = $1
             RETURNING agent_id, slot_index",
        )
        .bind(run_id)
        .fetch_all(pool)
        .await
        {
            Ok(rows) => {
                released_slot_count += rows.len();
                for row in rows {
                    let agent_id = match row.try_get::<String, _>("agent_id") {
                        Ok(value) => value,
                        Err(error) => {
                            warnings.push(format!(
                                "failed to decode released slot agent for run {run_id}: {error}"
                            ));
                            continue;
                        }
                    };
                    let slot_index = match row.try_get::<i64, _>("slot_index") {
                        Ok(value) => value,
                        Err(error) => {
                            warnings.push(format!(
                                "failed to decode released slot index for run {run_id}: {error}"
                            ));
                            continue;
                        }
                    };
                    if released_slots.insert((agent_id.clone(), slot_index)) {
                        match crate::services::auto_queue::runtime::clear_slot_threads_for_slot_pg(
                            health_registry.clone(),
                            pool,
                            &agent_id,
                            slot_index,
                        )
                        .await
                        {
                            Ok(cleared) => cleared_sessions += cleared,
                            Err(error) => {
                                crate::auto_queue_log!(
                                    warn,
                                    "clear_slot_threads_pg_failed",
                                    AutoQueueLogContext::new().agent(&agent_id),
                                    "[auto-queue] failed to clear postgres slot thread sessions for {}:{}: {}",
                                    agent_id,
                                    slot_index,
                                    error
                                );
                                warnings.push(format!(
                                    "failed to clear slot thread sessions for {agent_id}:{slot_index}: {error}"
                                ));
                            }
                        }
                    }
                }
            }
            Err(error) => {
                crate::auto_queue_log!(
                    warn,
                    "clear_slot_release_pg_failed",
                    AutoQueueLogContext::new().run(run_id),
                    "[auto-queue] failed to release postgres slots while clearing run {}: {}",
                    run_id,
                    error
                );
                warnings.push(format!("failed to release slots for run {run_id}: {error}"));
            }
        }
    }

    SlotCleanupResult {
        released_slots: released_slot_count,
        cleared_slot_sessions: cleared_sessions,
        warnings,
    }
}

async fn cancel_selected_runs_with_pg(
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

async fn reset_scoped_with_pg(
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

async fn reset_global_with_pg(pool: &sqlx::PgPool) -> Result<serde_json::Value, String> {
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

fn parse_json_body<T: DeserializeOwned>(body: Bytes, label: &str) -> Result<T, String> {
    if body.is_empty() {
        serde_json::from_slice(b"{}").map_err(|error| format!("invalid {label} body: {error}"))
    } else {
        serde_json::from_slice(&body).map_err(|error| format!("invalid {label} body: {error}"))
    }
}

async fn update_run_with_pg(
    run_id: &str,
    body: &UpdateRunBody,
    pool: &sqlx::PgPool,
) -> Result<usize, String> {
    let mut changed = 0usize;

    if let Some(ref status) = body.status {
        let result = if status == "completed" {
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
        changed += result.rows_affected() as usize;
    }

    if let Some(ref deploy_phases) = body.deploy_phases {
        let json_str = serde_json::to_string(deploy_phases)
            .map_err(|error| format!("serialize deploy_phases for run {run_id}: {error}"))?;
        let result = sqlx::query(
            "UPDATE auto_queue_runs
             SET deploy_phases = $1::jsonb
             WHERE id = $2",
        )
        .bind(json_str)
        .bind(run_id)
        .execute(pool)
        .await
        .map_err(|error| {
            format!("update postgres auto_queue_runs deploy_phases for {run_id}: {error}")
        })?;
        changed += result.rows_affected() as usize;
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

    Ok(changed)
}

async fn reorder_with_pg(body: &ReorderBody, pool: &sqlx::PgPool) -> Result<(), String> {
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
    for (rank, id) in reordered_ids.iter().enumerate() {
        sqlx::query("UPDATE auto_queue_entries SET priority_rank = $1 WHERE id = $2")
            .bind(rank as i64)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                format!("update auto_queue_entries priority_rank for {id}: {error}")
            })?;
    }
    tx.commit()
        .await
        .map_err(|error| format!("commit reorder transaction: {error}"))?;

    Ok(())
}

async fn soft_pause_with_pg(pool: &sqlx::PgPool) -> Result<serde_json::Value, String> {
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

async fn force_pause_with_pg(
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &sqlx::PgPool,
) -> Result<serde_json::Value, String> {
    let active_run_ids =
        crate::services::auto_queue::cancel_run::load_run_ids_with_status_pg(pool, &["active"])
            .await?;
    let cleanup = crate::services::auto_queue::cancel_run::cancel_and_release_runs_with_pg(
        health_registry,
        pool,
        &active_run_ids,
        "auto_queue_pause",
        Some("run_pause_orphan_self_heal"),
    )
    .await?;
    let _deleted_phase_gates =
        crate::services::auto_queue::cancel_run::delete_phase_gate_rows_for_runs_pg(
            pool,
            &active_run_ids,
        )
        .await?;
    let _skipped_entries =
        crate::services::auto_queue::cancel_run::skip_dispatched_entries_for_runs_pg(
            pool,
            &active_run_ids,
            "run_pause",
        )
        .await?;
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

async fn cancel_with_pg(
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &sqlx::PgPool,
) -> Result<serde_json::Value, String> {
    crate::services::auto_queue::cancel_run::cancel_with_pg(health_registry, pool).await
}

