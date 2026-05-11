use sqlx::{PgPool, Row as SqlxRow};

use super::phase_gates::batch_phase_is_eligible;
use super::slots::{
    ensure_agent_slot_pool_rows_pg, run_slot_pool_size_pg, slot_has_active_dispatch_pg,
};

const SLOT_ALLOCATION_MAX_RETRIES: usize = 16;

pub async fn group_has_pending_entries_pg(
    pool: &PgPool,
    run_id: &str,
    thread_group: i64,
    current_phase: Option<i64>,
) -> Result<bool, sqlx::Error> {
    let rows = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(batch_phase, 0)::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1
           AND COALESCE(thread_group, 0) = $2
           AND status = 'pending'
         ORDER BY priority_rank ASC",
    )
    .bind(run_id)
    .bind(thread_group)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .any(|batch_phase| batch_phase_is_eligible(batch_phase, current_phase)))
}

pub async fn first_pending_entry_for_group_pg(
    pool: &PgPool,
    run_id: &str,
    thread_group: i64,
    current_phase: Option<i64>,
) -> Result<Option<(String, String, String, i64, i64)>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT e.id,
                COALESCE(e.kanban_card_id, '') AS kanban_card_id,
                e.agent_id,
                COALESCE(e.batch_phase, 0)::BIGINT AS batch_phase,
                COALESCE(e.retry_count, 0)::BIGINT AS retry_count
         FROM auto_queue_entries e
         WHERE e.run_id = $1
           AND COALESCE(e.thread_group, 0) = $2
           AND e.status = 'pending'
         ORDER BY e.priority_rank ASC",
    )
    .bind(run_id)
    .bind(thread_group)
    .fetch_all(pool)
    .await?;

    for row in rows {
        let batch_phase = row.try_get::<i64, _>("batch_phase")?;
        if batch_phase_is_eligible(batch_phase, current_phase) {
            return Ok(Some((
                row.try_get("id")?,
                row.try_get("kanban_card_id")?,
                row.try_get("agent_id")?,
                batch_phase,
                row.try_get("retry_count")?,
            )));
        }
    }

    Ok(None)
}

#[allow(dead_code)]
pub async fn sync_run_group_metadata_pg(pool: &PgPool, run_id: &str) -> Result<(), String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres sync run group metadata {run_id}: {error}"))?;
    sync_run_group_metadata_pg_tx(&mut tx, run_id).await?;
    tx.commit()
        .await
        .map_err(|error| format!("commit postgres sync run group metadata {run_id}: {error}"))?;
    Ok(())
}

#[allow(dead_code)]
pub async fn sync_run_group_metadata_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<(), String> {
    let thread_group_count = sqlx::query_scalar::<_, i64>(
        "SELECT GREATEST(
                COALESCE(COUNT(DISTINCT COALESCE(thread_group, 0)), 0),
                1
            )::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1",
    )
    .bind(run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("count postgres thread groups for run {run_id}: {error}"))?;

    sqlx::query(
        "UPDATE auto_queue_runs
         SET thread_group_count = $1,
             max_concurrent_threads = $1
         WHERE id = $2",
    )
    .bind(thread_group_count)
    .bind(run_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("update postgres run group metadata for {run_id}: {error}"))?;

    Ok(())
}

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
    // #2048 F10: only bind PENDING entries. Dispatched entries already
    // have their slot_index pinned to the live dispatch row's context
    // (`context.slot_index`); rewriting it here would desync the entry
    // from its dispatch and cause observability/cleanup to point at the
    // wrong slot. New entries that share the group still pick up the
    // newly chosen slot via this query, since they enter as 'pending'.
    let result = sqlx::query(
        "UPDATE auto_queue_entries
         SET slot_index = $1
         WHERE run_id = $2
           AND agent_id = $3
           AND COALESCE(thread_group, 0) = $4
           AND status = 'pending'
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

    // #2048 F3: serialize the whole allocation per-agent against concurrent
    // calls. The compare-and-set pattern across "existing → reusable → free"
    // probe + UPDATE + busy-recheck is racy — two calls for the same agent
    // could both observe the same slot, claim it, and bind the same
    // `slot_index` on different entries before either's post-claim busy
    // probe fires. An agent-scoped session advisory lock pinned to a
    // dedicated connection orders the probe+claim without changing
    // wire-level UPDATE semantics for any single step.
    let mut conn = pool.acquire().await.map_err(|error| {
        format!("acquire slot allocation connection for agent {agent_id}: {error}")
    })?;
    sqlx::query("SELECT pg_advisory_lock(hashtext($1), hashtext($2))")
        .bind("aq_slot_alloc")
        .bind(agent_id)
        .execute(&mut *conn)
        .await
        .map_err(|error| {
            format!("acquire slot allocation advisory lock for agent {agent_id}: {error}")
        })?;
    let result =
        allocate_slot_for_group_agent_pg_inner(pool, run_id, thread_group, agent_id).await;
    if let Err(unlock_error) =
        sqlx::query("SELECT pg_advisory_unlock(hashtext($1), hashtext($2))")
            .bind("aq_slot_alloc")
            .bind(agent_id)
            .execute(&mut *conn)
            .await
    {
        tracing::warn!(
            agent_id,
            error = %unlock_error,
            "[auto-queue] failed to release slot allocation advisory lock"
        );
    }
    drop(conn);
    result
}

async fn allocate_slot_for_group_agent_pg_inner(
    pool: &PgPool,
    run_id: &str,
    thread_group: i64,
    agent_id: &str,
) -> Result<Option<SlotAllocation>, String> {
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
