//! `auto-queue add` for a card with no reusable run: a new pending run and its first entry.

use serde_json::{Value, json};

/// The card and placement for a new CLI run.
pub(crate) struct NewRunEntry<'a> {
    pub(crate) repo_id: Option<&'a str>,
    pub(crate) agent_id: &'a str,
    pub(crate) card_id: &'a str,
    pub(crate) card_status: &'a str,
    pub(crate) priority: Option<i64>,
    pub(crate) thread_group: Option<i64>,
    pub(crate) phase: Option<i64>,
}

/// Create a pending CLI run and queue the card as its first entry. Both commit together, so
/// no other connection sees the run while it is still empty and could complete it.
pub(crate) async fn add_card_to_new_run_pg(
    pool: &sqlx::PgPool,
    entry: &NewRunEntry<'_>,
) -> Result<Value, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| format!("begin postgres auto-queue add: {e}"))?;
    let run_id = {
        let created_run_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO auto_queue_runs (
                 id, repo, agent_id, status, ai_model, ai_rationale, max_concurrent_threads, thread_group_count
             )
             VALUES ($1, $2, $3, 'pending', 'manual-cli', 'agentdesk auto-queue add', 1, 1)",
        )
        .bind(&created_run_id)
        .bind(entry.repo_id)
        .bind(entry.agent_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("create postgres auto-queue run: {e}"))?;
        created_run_id
    };

    let already_queued = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
             SELECT 1 FROM auto_queue_entries
             WHERE run_id = $1 AND kanban_card_id = $2 AND status IN ('pending', 'dispatched')
         )",
    )
    .bind(&run_id)
    .bind(entry.card_id)
    .fetch_one(&mut *tx)
    .await
    .unwrap_or(false);
    if already_queued {
        tx.commit()
            .await
            .map_err(|e| format!("commit postgres auto-queue add: {e}"))?;
        return Ok(json!({
            "ok": true,
            "already_queued": true,
            "run_id": run_id,
            "card_id": entry.card_id,
        }));
    }

    let has_active_dispatch = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
             SELECT 1 FROM task_dispatches
             WHERE kanban_card_id = $1 AND status IN ('pending', 'dispatched')
         )",
    )
    .bind(entry.card_id)
    .fetch_one(&mut *tx)
    .await
    .unwrap_or(false);
    if has_active_dispatch {
        return Err(format!(
            "card {} already has an active dispatch and cannot be queued again",
            entry.card_id
        ));
    }

    let effective_thread_group = entry.thread_group.unwrap_or(0);
    let next_rank = if let Some(priority) = entry.priority {
        priority
    } else {
        sqlx::query_scalar::<_, i64>(
            "SELECT (COALESCE(MAX(priority_rank), -1) + 1)::BIGINT
             FROM auto_queue_entries
             WHERE run_id = $1
               AND COALESCE(thread_group, 0) = $2",
        )
        .bind(&run_id)
        .bind(effective_thread_group)
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(0)
    };
    let entry_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
             id, run_id, kanban_card_id, agent_id, priority_rank, thread_group, batch_phase, reason
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(&entry_id)
    .bind(&run_id)
    .bind(entry.card_id)
    .bind(entry.agent_id)
    .bind(next_rank)
    .bind(effective_thread_group)
    .bind(entry.phase.unwrap_or(0))
    .bind(format!("manual CLI add from status {}", entry.card_status))
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("insert postgres auto-queue entry: {e}"))?;
    tx.commit()
        .await
        .map_err(|e| format!("commit postgres auto-queue add: {e}"))?;

    Ok(json!({
        "ok": true,
        "run_id": run_id,
        "entry_id": entry_id,
        "card_id": entry.card_id,
        "priority_rank": next_rank,
        "thread_group": effective_thread_group,
        "batch_phase": entry.phase.unwrap_or(0),
    }))
}
