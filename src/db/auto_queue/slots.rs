use sqlx::{PgPool, Row as SqlxRow};

use super::phase_gates::batch_phase_is_eligible;

const SLOT_ALLOCATION_MAX_RETRIES: usize = 16;
// Give the provider bridge a short cleanup window after a terminal turn before
// reusing the same slot/thread. The auto-queue tick retries roughly every
// minute, so 45s avoids immediate same-thread delivery without adding another
// full tick of avoidable delay in the common case.
pub const SLOT_TERMINAL_DISPATCH_COOLDOWN_SECONDS: i64 = 45;

pub async fn rebind_slot_for_group_agent_pg(
    pool: &PgPool,
    run_id: &str,
    thread_group: i64,
    agent_id: &str,
    slot_index: i64,
) -> Result<u64, String> {
    let slot_pool_size = run_slot_pool_size_pg(pool, run_id)
        .await
        .map_err(|error| format!("load postgres slot pool size for {run_id}: {error}"))?;
    ensure_agent_slot_pool_rows_pg(pool, agent_id, slot_pool_size)
        .await
        .map_err(|error| {
            format!("prepare postgres slot rows for run {run_id} agent {agent_id}: {error}")
        })?;

    let slot_updated = sqlx::query(
        "UPDATE auto_queue_slots
         SET assigned_run_id = $1,
             assigned_thread_group = $2,
             updated_at = NOW()
         WHERE agent_id = $3
           AND slot_index = $4
           AND (assigned_run_id IS NULL OR assigned_run_id = $1)",
    )
    .bind(run_id)
    .bind(thread_group)
    .bind(agent_id)
    .bind(slot_index)
    .execute(pool)
    .await
    .map_err(|error| {
        format!(
            "rebind postgres slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
        )
    })?
    .rows_affected();
    if slot_updated == 0 {
        return Ok(0);
    }

    bind_slot_index_for_group_entries_pg(pool, run_id, agent_id, thread_group, slot_index)
        .await
        .map_err(|error| {
            format!(
                "bind rebound postgres slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
            )
        })
}

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
        "UPDATE auto_queue_slots
         SET assigned_run_id = NULL,
             assigned_thread_group = NULL,
             updated_at = NOW()
         WHERE assigned_run_id IS NOT NULL
           AND assigned_run_id NOT IN (
               SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
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

pub async fn assigned_groups_with_pending_entries_pg(
    pool: &PgPool,
    run_id: &str,
    current_phase: Option<i64>,
) -> Result<Vec<i64>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT s.assigned_thread_group, COALESCE(e.batch_phase, 0)::BIGINT AS batch_phase
         FROM auto_queue_slots s
         JOIN auto_queue_entries e
           ON e.run_id = $1
          AND e.agent_id = s.agent_id
          AND COALESCE(e.thread_group, 0) = COALESCE(s.assigned_thread_group, 0)
         WHERE s.assigned_run_id = $1
           AND s.assigned_thread_group IS NOT NULL
           AND EXISTS (
               SELECT 1
               FROM auto_queue_entries e
               WHERE e.run_id = $1
                 AND e.agent_id = s.agent_id
                 AND COALESCE(e.thread_group, 0) = COALESCE(s.assigned_thread_group, 0)
                 AND e.status = 'pending'
           )
           AND NOT EXISTS (
               SELECT 1
               FROM auto_queue_entries e
               WHERE e.run_id = $1
                 AND e.agent_id = s.agent_id
                 AND COALESCE(e.thread_group, 0) = COALESCE(s.assigned_thread_group, 0)
                 AND e.status = 'dispatched'
           )
         ORDER BY s.assigned_thread_group ASC, s.slot_index ASC, COALESCE(e.batch_phase, 0) ASC",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?;

    let mut seen = std::collections::HashSet::new();
    let mut groups = Vec::new();
    for row in rows {
        let thread_group = row.try_get::<i64, _>("assigned_thread_group")?;
        let batch_phase = row.try_get::<i64, _>("batch_phase")?;
        if batch_phase_is_eligible(batch_phase, current_phase) && seen.insert(thread_group) {
            groups.push(thread_group);
        }
    }
    Ok(groups)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotAllocation {
    pub slot_index: i64,
    pub newly_assigned: bool,
    pub reassigned_from_other_group: bool,
}

async fn bind_slot_index_for_group_entries_pg(
    pool: &PgPool,
    run_id: &str,
    agent_id: &str,
    thread_group: i64,
    slot_index: i64,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE auto_queue_entries
         SET slot_index = $1
         WHERE run_id = $2
           AND agent_id = $3
           AND COALESCE(thread_group, 0) = $4
           AND status IN ('pending', 'dispatched')
           AND (slot_index IS NULL OR slot_index != $1)",
    )
    .bind(slot_index)
    .bind(run_id)
    .bind(agent_id)
    .bind(thread_group)
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

fn active_dispatch_slot_guard_sql(agent_expr: &str, slot_expr: &str) -> String {
    format!(
        "NOT EXISTS (
             SELECT 1
             FROM task_dispatches d
             WHERE d.to_agent_id = {agent_expr}
               AND d.status IN ('pending', 'dispatched')
               AND COALESCE(NULLIF((COALESCE(NULLIF(d.context, ''), '{{}}')::jsonb)->>'slot_index', '')::BIGINT, -1) = {slot_expr}
               AND COALESCE(((COALESCE(NULLIF(d.context, ''), '{{}}')::jsonb)->>'sidecar_dispatch')::BOOLEAN, FALSE) = FALSE
               AND (COALESCE(NULLIF(d.context, ''), '{{}}')::jsonb)->'phase_gate' IS NULL
               AND (
                   COALESCE(d.dispatch_type, 'implementation') NOT IN ('review', 'review-decision', 'create-pr')
                   OR EXISTS (
                       SELECT 1
                       FROM sessions s
                       WHERE s.active_dispatch_id = d.id
                         AND COALESCE(s.status, '') NOT IN ('disconnected', 'completed', 'failed', 'cancelled')
                   )
               )
         )"
    )
}

fn active_dispatch_slot_exists_sql(agent_expr: &str, slot_expr: &str) -> String {
    format!(
        "EXISTS (
             SELECT 1
             FROM task_dispatches d
             WHERE d.to_agent_id = {agent_expr}
               AND d.status IN ('pending', 'dispatched')
               AND COALESCE(NULLIF((COALESCE(NULLIF(d.context, ''), '{{}}')::jsonb)->>'slot_index', '')::BIGINT, -1) = {slot_expr}
               AND COALESCE(((COALESCE(NULLIF(d.context, ''), '{{}}')::jsonb)->>'sidecar_dispatch')::BOOLEAN, FALSE) = FALSE
               AND (COALESCE(NULLIF(d.context, ''), '{{}}')::jsonb)->'phase_gate' IS NULL
               AND (
                   COALESCE(d.dispatch_type, 'implementation') NOT IN ('review', 'review-decision', 'create-pr')
                   OR EXISTS (
                       SELECT 1
                       FROM sessions s
                       WHERE s.active_dispatch_id = d.id
                         AND COALESCE(s.status, '') NOT IN ('disconnected', 'completed', 'failed', 'cancelled')
                   )
               )
         )"
    )
}

async fn first_free_slot_blocked_by_active_dispatch_pg(
    pool: &PgPool,
    agent_id: &str,
) -> Result<Option<i64>, sqlx::Error> {
    let active_dispatch_exists =
        active_dispatch_slot_exists_sql("auto_queue_slots.agent_id", "auto_queue_slots.slot_index");
    let query = format!(
        "SELECT slot_index::BIGINT
         FROM auto_queue_slots
         WHERE agent_id = $1
           AND assigned_run_id IS NULL
           AND {active_dispatch_exists}
         ORDER BY slot_index ASC
         LIMIT 1"
    );

    sqlx::query_scalar::<_, i64>(&query)
        .bind(agent_id)
        .fetch_optional(pool)
        .await
}

pub async fn allocate_slot_for_group_agent_pg(
    pool: &PgPool,
    run_id: &str,
    thread_group: i64,
    agent_id: &str,
) -> Result<Option<SlotAllocation>, String> {
    let slot_pool_size = run_slot_pool_size_pg(pool, run_id)
        .await
        .map_err(|error| format!("load postgres slot pool size for {run_id}: {error}"))?;
    ensure_agent_slot_pool_rows_pg(pool, agent_id, slot_pool_size)
        .await
        .map_err(|error| {
            format!("prepare postgres slot rows for run {run_id} agent {agent_id}: {error}")
        })?;

    for attempt in 1..=SLOT_ALLOCATION_MAX_RETRIES {
        let existing = sqlx::query_scalar::<_, i64>(
            "SELECT slot_index::BIGINT
             FROM auto_queue_slots
             WHERE agent_id = $1
               AND assigned_run_id = $2
               AND COALESCE(assigned_thread_group, 0) = $3
             LIMIT 1",
        )
        .bind(agent_id)
        .bind(run_id)
        .bind(thread_group)
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            format!(
                "inspect existing postgres slot for run {run_id} agent {agent_id} group {thread_group}: {error}"
            )
        })?;
        if let Some(slot_index) = existing {
            let slot_busy = slot_has_active_dispatch_pg(pool, agent_id, slot_index)
                .await
                .map_err(|error| {
                    format!(
                        "inspect existing postgres slot {slot_index} active dispatch for run {run_id} agent {agent_id} group {thread_group}: {error}"
                    )
                })?;
            if slot_busy {
                return Ok(None);
            }

            bind_slot_index_for_group_entries_pg(pool, run_id, agent_id, thread_group, slot_index)
                .await
                .map_err(|error| {
                    format!(
                        "bind existing postgres slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
                    )
                })?;
            return Ok(Some(SlotAllocation {
                slot_index,
                newly_assigned: false,
                reassigned_from_other_group: false,
            }));
        }

        let reusable_slot_guard = active_dispatch_slot_guard_sql("s.agent_id", "s.slot_index");
        let reusable_slot_query = format!(
            "SELECT s.slot_index::BIGINT,
                    s.assigned_thread_group::BIGINT
             FROM auto_queue_slots s
             WHERE s.agent_id = $1
               AND s.assigned_run_id = $2
               AND COALESCE(s.assigned_thread_group, -1) != $3
               AND NOT EXISTS (
                   SELECT 1
                   FROM auto_queue_entries e
                   WHERE e.run_id = $2
                     AND e.agent_id = s.agent_id
                     AND COALESCE(e.thread_group, 0) = COALESCE(s.assigned_thread_group, 0)
                     AND e.status IN ('pending', 'dispatched')
               )
               AND {reusable_slot_guard}
             ORDER BY s.slot_index ASC
             LIMIT 1"
        );
        let reusable_slot = sqlx::query(&reusable_slot_query)
            .bind(agent_id)
            .bind(run_id)
            .bind(thread_group)
            .fetch_optional(pool)
            .await
            .map_err(|error| {
                format!(
                    "inspect reusable postgres slot for run {run_id} agent {agent_id} group {thread_group}: {error}"
                )
            })?;
        if let Some(reusable_slot) = reusable_slot {
            let slot_index = reusable_slot.try_get::<i64, _>("slot_index").map_err(|error| {
                format!(
                    "decode reusable postgres slot index for run {run_id} agent {agent_id} group {thread_group}: {error}"
                )
            })?;
            let previous_thread_group = reusable_slot
                .try_get::<Option<i64>, _>("assigned_thread_group")
                .map_err(|error| {
                    format!(
                        "decode reusable postgres slot previous group for run {run_id} agent {agent_id} group {thread_group}: {error}"
                    )
                })?;
            let rebound_slot_guard = active_dispatch_slot_guard_sql(
                "auto_queue_slots.agent_id",
                "auto_queue_slots.slot_index",
            );
            let rebound_query = format!(
                "UPDATE auto_queue_slots
                 SET assigned_thread_group = $1,
                     updated_at = NOW()
                 WHERE agent_id = $2
                   AND slot_index = $3
                   AND assigned_run_id = $4
                   AND COALESCE(assigned_thread_group, -1) != $1
                   AND NOT EXISTS (
                       SELECT 1
                       FROM auto_queue_entries e
                       WHERE e.run_id = $4
                         AND e.agent_id = auto_queue_slots.agent_id
                         AND COALESCE(e.thread_group, 0) = COALESCE(auto_queue_slots.assigned_thread_group, 0)
                         AND e.status IN ('pending', 'dispatched')
                   )
                   AND {rebound_slot_guard}"
            );
            let rebound = sqlx::query(&rebound_query)
            .bind(thread_group)
            .bind(agent_id)
            .bind(slot_index)
            .bind(run_id)
            .execute(pool)
            .await
            .map_err(|error| {
                format!(
                    "rebind postgres slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
                )
            })?
            .rows_affected();
            if rebound == 0 {
                if attempt == SLOT_ALLOCATION_MAX_RETRIES {
                    return Err(format!(
                        "slot allocation retry limit exceeded for run {run_id} agent {agent_id} group {thread_group} after {attempt} attempts"
                    ));
                }
                continue;
            }

            let slot_busy = slot_has_active_dispatch_pg(pool, agent_id, slot_index)
                .await
                .map_err(|error| {
                    format!(
                        "inspect rebound postgres slot {slot_index} active dispatch for run {run_id} agent {agent_id} group {thread_group}: {error}"
                    )
                })?;
            if slot_busy {
                tracing::warn!(
                    run_id,
                    agent_id,
                    thread_group,
                    slot_index,
                    "[auto-queue] rebound slot raced with active dispatch; restoring previous group"
                );
                let restored = sqlx::query(
                    "UPDATE auto_queue_slots
                     SET assigned_thread_group = $1,
                         updated_at = NOW()
                     WHERE agent_id = $2
                       AND slot_index = $3
                       AND assigned_run_id = $4
                       AND COALESCE(assigned_thread_group, -1) = $5",
                )
                .bind(previous_thread_group)
                .bind(agent_id)
                .bind(slot_index)
                .bind(run_id)
                .bind(thread_group)
                .execute(pool)
                .await
                .map_err(|error| {
                    format!(
                        "restore raced rebound postgres slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
                    )
                })?
                .rows_affected();
                if restored == 0 {
                    tracing::warn!(
                        run_id,
                        agent_id,
                        thread_group,
                        slot_index,
                        "[auto-queue] failed to restore raced rebound slot"
                    );
                }
                continue;
            }

            bind_slot_index_for_group_entries_pg(pool, run_id, agent_id, thread_group, slot_index)
                .await
                .map_err(|error| {
                    format!(
                        "bind rebound postgres slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
                    )
                })?;
            return Ok(Some(SlotAllocation {
                slot_index,
                newly_assigned: false,
                reassigned_from_other_group: true,
            }));
        }

        let free_slot_guard = active_dispatch_slot_guard_sql(
            "auto_queue_slots.agent_id",
            "auto_queue_slots.slot_index",
        );
        let free_slot_query = format!(
            "SELECT slot_index::BIGINT
             FROM auto_queue_slots
             WHERE agent_id = $1
               AND assigned_run_id IS NULL
               AND {free_slot_guard}
             ORDER BY slot_index ASC
             LIMIT 1"
        );
        let free_slot = sqlx::query_scalar::<_, i64>(&free_slot_query)
        .bind(agent_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            format!(
                "inspect free postgres slot for run {run_id} agent {agent_id} group {thread_group}: {error}"
            )
        })?;
        let Some(slot_index) = free_slot else {
            match first_free_slot_blocked_by_active_dispatch_pg(pool, agent_id).await {
                Ok(Some(blocked_slot_index)) => tracing::warn!(
                    run_id,
                    agent_id,
                    thread_group,
                    slot_index = blocked_slot_index,
                    "[auto-queue] free-slot fallback refused slot with active dispatch"
                ),
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    run_id,
                    agent_id,
                    thread_group,
                    error = %error,
                    "[auto-queue] failed to inspect active-dispatch-blocked free slots"
                ),
            }
            return Ok(None);
        };

        let claim_slot_guard = active_dispatch_slot_guard_sql(
            "auto_queue_slots.agent_id",
            "auto_queue_slots.slot_index",
        );
        let claim_query = format!(
            "UPDATE auto_queue_slots
             SET assigned_run_id = $1,
                 assigned_thread_group = $2,
                 updated_at = NOW()
             WHERE agent_id = $3
               AND slot_index = $4
               AND assigned_run_id IS NULL
               AND {claim_slot_guard}"
        );
        let claimed = sqlx::query(&claim_query)
        .bind(run_id)
        .bind(thread_group)
        .bind(agent_id)
        .bind(slot_index)
        .execute(pool)
        .await
        .map_err(|error| {
            format!(
                "claim postgres slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
            )
        })?
        .rows_affected();
        if claimed == 0 {
            match slot_has_active_dispatch_pg(pool, agent_id, slot_index).await {
                Ok(true) => tracing::warn!(
                    run_id,
                    agent_id,
                    thread_group,
                    slot_index,
                    "[auto-queue] free-slot claim refused slot with active dispatch"
                ),
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    run_id,
                    agent_id,
                    thread_group,
                    slot_index,
                    error = %error,
                    "[auto-queue] failed to inspect active dispatch after free-slot claim refusal"
                ),
            }
            if attempt == SLOT_ALLOCATION_MAX_RETRIES {
                return Err(format!(
                    "slot allocation retry limit exceeded for run {run_id} agent {agent_id} group {thread_group} after {attempt} attempts"
                ));
            }
            continue;
        }

        let slot_busy = slot_has_active_dispatch_pg(pool, agent_id, slot_index)
            .await
            .map_err(|error| {
                format!(
                    "inspect claimed postgres slot {slot_index} active dispatch for run {run_id} agent {agent_id} group {thread_group}: {error}"
                )
            })?;
        if slot_busy {
            tracing::warn!(
                run_id,
                agent_id,
                thread_group,
                slot_index,
                "[auto-queue] claimed free slot raced with active dispatch; releasing claim"
            );
            let released = sqlx::query(
                "UPDATE auto_queue_slots
                 SET assigned_run_id = NULL,
                     assigned_thread_group = NULL,
                     updated_at = NOW()
                 WHERE agent_id = $1
                   AND slot_index = $2
                   AND assigned_run_id = $3
                   AND COALESCE(assigned_thread_group, -1) = $4",
            )
            .bind(agent_id)
            .bind(slot_index)
            .bind(run_id)
            .bind(thread_group)
            .execute(pool)
            .await
            .map_err(|error| {
                format!(
                    "release raced claimed postgres slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
                )
            })?
            .rows_affected();
            if released == 0 {
                tracing::warn!(
                    run_id,
                    agent_id,
                    thread_group,
                    slot_index,
                    "[auto-queue] failed to release raced claimed free slot"
                );
            }
            continue;
        }

        bind_slot_index_for_group_entries_pg(pool, run_id, agent_id, thread_group, slot_index)
            .await
            .map_err(|error| {
                format!(
                    "bind claimed postgres slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
                )
            })?;
        return Ok(Some(SlotAllocation {
            slot_index,
            newly_assigned: true,
            reassigned_from_other_group: false,
        }));
    }

    unreachable!("slot allocation loop must return within bounded retries");
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
    let auto_queue_active = sqlx::query_scalar::<_, bool>(
        "SELECT COUNT(*) > 0
         FROM auto_queue_entries
         WHERE agent_id = $1
           AND slot_index = $2
           AND status = 'dispatched'
           AND COALESCE(dispatch_id, '') != $3",
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
