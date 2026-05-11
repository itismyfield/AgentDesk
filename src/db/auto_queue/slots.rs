use sqlx::PgPool;

// Give the provider bridge a short cleanup window after a terminal turn before
// reusing the same slot/thread. The auto-queue tick retries roughly every
// minute, so 45s avoids immediate same-thread delivery without adding another
// full tick of avoidable delay in the common case.
pub const SLOT_TERMINAL_DISPATCH_COOLDOWN_SECONDS: i64 = 45;

pub async fn run_slot_pool_size_pg(pool: &PgPool, run_id: &str) -> Result<i64, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, Option<i64>>(
        "SELECT COALESCE(max_concurrent_threads, 1)::BIGINT
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await?
    .flatten()
    .unwrap_or(1)
    .clamp(1, 10))
}

pub async fn ensure_agent_slot_pool_rows_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_pool_size: i64,
) -> Result<(), sqlx::Error> {
    for slot_index in 0..slot_pool_size.clamp(1, 32) {
        sqlx::query(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ($1, $2, '{}'::jsonb)
             ON CONFLICT (agent_id, slot_index) DO NOTHING",
        )
        .bind(agent_id)
        .bind(slot_index)
        .execute(pool)
        .await?;
    }
    Ok(())
}

pub async fn clear_inactive_slot_assignments_pg(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        // #2048 F14: include `restoring` so a run in mid-restore does not
        // have its slots yanked by a concurrent activate call. `restoring`
        // is a transient holding status (see fsm::apply_restore_state_changes_pg)
        // and must be treated as held-open for slot purposes.
        "UPDATE auto_queue_slots
         SET assigned_run_id = NULL,
             assigned_thread_group = NULL,
             updated_at = NOW()
         WHERE assigned_run_id IS NOT NULL
           AND assigned_run_id NOT IN (
               SELECT id FROM auto_queue_runs
               WHERE status IN ('active', 'paused', 'restoring')
           )",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

pub async fn release_run_slots_pg(pool: &PgPool, run_id: &str) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE auto_queue_slots
         SET assigned_run_id = NULL,
             assigned_thread_group = NULL,
             updated_at = NOW()
         WHERE assigned_run_id = $1",
    )
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

pub async fn slot_has_recent_terminal_auto_queue_dispatch_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
             SELECT 1
             FROM task_dispatches d
             WHERE d.to_agent_id = $1
               AND d.status IN ('completed', 'failed', 'cancelled')
               AND COALESCE(NULLIF((COALESCE(NULLIF(d.context, ''), '{}')::jsonb)->>'slot_index', '')::BIGINT, -1) = $2
               AND COALESCE(((COALESCE(NULLIF(d.context, ''), '{}')::jsonb)->>'auto_queue')::BOOLEAN, FALSE) = TRUE
               AND COALESCE(((COALESCE(NULLIF(d.context, ''), '{}')::jsonb)->>'sidecar_dispatch')::BOOLEAN, FALSE) = FALSE
               AND (COALESCE(NULLIF(d.context, ''), '{}')::jsonb)->'phase_gate' IS NULL
               AND COALESCE(d.completed_at, d.updated_at, d.created_at)
                   >= NOW() - make_interval(secs => $3::INT)
         )",
    )
    .bind(agent_id)
    .bind(slot_index)
    .bind(SLOT_TERMINAL_DISPATCH_COOLDOWN_SECONDS)
    .fetch_one(pool)
    .await
}

pub async fn release_slot_for_group_agent_pg(
    pool: &PgPool,
    run_id: &str,
    thread_group: i64,
    agent_id: &str,
    slot_index: i64,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE auto_queue_slots
         SET assigned_run_id = NULL,
             assigned_thread_group = NULL,
             updated_at = NOW()
         WHERE agent_id = $1
           AND slot_index = $2
           AND assigned_run_id = $3
           AND COALESCE(assigned_thread_group, 0) = $4",
    )
    .bind(agent_id)
    .bind(slot_index)
    .bind(run_id)
    .bind(thread_group)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

pub async fn slot_has_active_dispatch_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
) -> Result<bool, sqlx::Error> {
    slot_has_active_dispatch_excluding_pg(pool, agent_id, slot_index, None).await
}

pub async fn slot_has_active_dispatch_excluding_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
    exclude_dispatch_id: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let exclude_id = exclude_dispatch_id.unwrap_or("");
    // #2048 F5: paused/cancelled-run dispatched entries no longer block the
    // slot — their dispatches are being cancelled and the slot should be
    // reusable. Without the join, paused-run entries kept the slot
    // permanently "active" until `clear_inactive_slot_assignments_pg` ran.
    let auto_queue_active = sqlx::query_scalar::<_, bool>(
        "SELECT COUNT(*) > 0
         FROM auto_queue_entries e
         LEFT JOIN auto_queue_runs r ON r.id = e.run_id
         WHERE e.agent_id = $1
           AND e.slot_index = $2
           AND e.status = 'dispatched'
           AND COALESCE(e.dispatch_id, '') != $3
           AND COALESCE(r.status, 'active') NOT IN ('paused', 'cancelled')",
    )
    .bind(agent_id)
    .bind(slot_index)
    .bind(exclude_id)
    .fetch_one(pool)
    .await?;
    if auto_queue_active {
        return Ok(true);
    }

    sqlx::query_scalar::<_, bool>(
        "SELECT COUNT(*) > 0
         FROM task_dispatches
         WHERE to_agent_id = $1
           AND status IN ('pending', 'dispatched')
           AND COALESCE(NULLIF((COALESCE(NULLIF(context, ''), '{}')::jsonb)->>'slot_index', '')::BIGINT, -1) = $2
           AND COALESCE(((COALESCE(NULLIF(context, ''), '{}')::jsonb)->>'sidecar_dispatch')::BOOLEAN, FALSE) = FALSE
           AND (COALESCE(NULLIF(context, ''), '{}')::jsonb)->'phase_gate' IS NULL
           AND id != $3
           AND (
               COALESCE(dispatch_type, 'implementation') NOT IN ('review', 'review-decision', 'create-pr')
               OR EXISTS (
                   SELECT 1
                   FROM sessions s
                   WHERE s.active_dispatch_id = task_dispatches.id
                     AND COALESCE(s.status, '') NOT IN ('disconnected', 'completed', 'failed', 'cancelled')
               )
           )",
    )
    .bind(agent_id)
    .bind(slot_index)
    .bind(exclude_id)
    .fetch_one(pool)
    .await
}

pub(crate) async fn release_run_slots_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE auto_queue_slots
         SET assigned_run_id = NULL,
             assigned_thread_group = NULL,
             updated_at = NOW()
         WHERE assigned_run_id = $1",
    )
    .bind(run_id)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected())
}
