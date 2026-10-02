use super::*;

#[derive(Debug)]
pub(super) struct AddedRunEntry {
    pub(super) entry_id: String,
    pub(super) thread_group: i64,
    pub(super) priority_rank: i64,
}

pub(super) async fn sync_run_group_metadata_with_pg_tx(
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
    .map_err(|err| format!("count thread groups for run {run_id}: {err}"))?;

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
    .map_err(|err| format!("sync run group metadata for {run_id}: {err}"))?;
    Ok(())
}

pub(super) async fn enqueue_entries_into_existing_run_with_pg(
    pool: &sqlx::PgPool,
    run_id: &str,
    requested_entries: &[GenerateEntryBody],
    cards_by_issue: &HashMap<i64, ResolvedDispatchCard>,
) -> Result<Vec<AddedRunEntry>, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|err| format!("begin enqueue transaction: {err}"))?;

    let existing_live_cards: HashSet<String> = sqlx::query_scalar::<_, String>(
        "SELECT kanban_card_id
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(run_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(|err| format!("query existing queued cards: {err}"))?
    .into_iter()
    .collect();

    let mut next_rank_by_group = HashMap::new();
    for row in sqlx::query(
        "SELECT COALESCE(thread_group, 0)::BIGINT AS thread_group,
                (COALESCE(MAX(priority_rank), -1) + 1)::BIGINT AS next_priority_rank
         FROM auto_queue_entries
         WHERE run_id = $1
         GROUP BY COALESCE(thread_group, 0)",
    )
    .bind(run_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(|err| format!("query group ranks: {err}"))?
    {
        let thread_group: i64 = row
            .try_get("thread_group")
            .map_err(|err| format!("decode thread_group: {err}"))?;
        let next_priority_rank: i64 = row
            .try_get("next_priority_rank")
            .map_err(|err| format!("decode next_priority_rank: {err}"))?;
        next_rank_by_group.insert(thread_group, next_priority_rank);
    }

    let mut next_auto_group = sqlx::query_scalar::<_, i64>(
        "SELECT (COALESCE(MAX(COALESCE(thread_group, 0)), -1) + 1)::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1",
    )
    .bind(run_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|err| format!("query next thread group: {err}"))?;

    let mut existing_live_cards = existing_live_cards;
    let mut inserted = Vec::new();

    for entry in requested_entries {
        let Some(card) = cards_by_issue.get(&entry.issue_number) else {
            continue;
        };
        if existing_live_cards.contains(&card.card_id) {
            return Err(format!(
                "issue #{} is already queued in run {run_id}",
                entry.issue_number
            ));
        }

        let has_active_dispatch = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                 SELECT 1
                 FROM task_dispatches
                 WHERE kanban_card_id = $1
                   AND status IN ('pending', 'dispatched')
             )",
        )
        .bind(&card.card_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|err| format!("query active dispatches for {}: {err}", card.card_id))?;
        if has_active_dispatch {
            return Err(format!(
                "issue #{} already has an active dispatch and cannot be queued again",
                entry.issue_number
            ));
        }

        let thread_group = entry.thread_group.unwrap_or_else(|| {
            let chosen = next_auto_group;
            next_auto_group += 1;
            chosen
        });
        let priority_rank = *next_rank_by_group.entry(thread_group).or_insert(0);
        next_rank_by_group.insert(thread_group, priority_rank + 1);
        let entry_id = uuid::Uuid::new_v4().to_string();

        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, priority_rank, thread_group, batch_phase, reason
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8
             )",
        )
        .bind(&entry_id)
        .bind(run_id)
        .bind(&card.card_id)
        .bind(card.assigned_agent_id.as_deref().unwrap_or(""))
        .bind(priority_rank)
        .bind(thread_group)
        .bind(entry.batch_phase.unwrap_or(0))
        .bind(format!(
            "manual run entry add for issue #{}",
            entry.issue_number
        ))
        .execute(&mut *tx)
        .await
        .map_err(|err| format!("insert auto-queue entry: {err}"))?;

        existing_live_cards.insert(card.card_id.clone());
        inserted.push(AddedRunEntry {
            entry_id,
            thread_group,
            priority_rank,
        });
    }

    if !inserted.is_empty() {
        sync_run_group_metadata_with_pg_tx(&mut tx, run_id).await?;
    }

    tx.commit()
        .await
        .map_err(|err| format!("commit enqueue transaction: {err}"))?;
    Ok(inserted)
}

pub(super) fn existing_live_run_conflict_response(
    run_id: &str,
    status: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": format!(
                "live auto-queue run already exists: run_id={run_id}, status={status}; pass force=true to cancel it before creating a new run"
            ),
            "existing_run_id": run_id,
            "existing_run_status": status,
        })),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct QueueEntryOrder {
    pub(super) id: String,
    pub(super) status: String,
    pub(super) agent_id: String,
}

pub(super) fn reorder_entry_ids(
    entries: &[QueueEntryOrder],
    ordered_ids: &[String],
    agent_id: Option<&str>,
) -> Result<Vec<String>, String> {
    if ordered_ids.is_empty() {
        return Err("ordered_ids cannot be empty".to_string());
    }

    let scope_ids: Vec<&str> = entries
        .iter()
        .filter(|entry| {
            entry.status == "pending"
                && agent_id
                    .map(|target| entry.agent_id == target)
                    .unwrap_or(true)
        })
        .map(|entry| entry.id.as_str())
        .collect();
    if scope_ids.is_empty() {
        return Err("no pending entries found for reorder scope".to_string());
    }

    let scope_set: HashSet<&str> = scope_ids.iter().copied().collect();
    let mut seen = HashSet::new();
    let mut replacement_ids = Vec::new();
    for id in ordered_ids {
        let id_str = id.as_str();
        if scope_set.contains(id_str) && seen.insert(id_str) {
            replacement_ids.push(id_str);
        }
    }
    if replacement_ids.is_empty() {
        return Err("ordered_ids do not match any pending entries in scope".to_string());
    }

    for id in &scope_ids {
        if !seen.contains(*id) {
            replacement_ids.push(*id);
        }
    }

    let mut replacement_iter = replacement_ids.into_iter();
    let mut reordered = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.status == "pending"
            && agent_id
                .map(|target| entry.agent_id == target)
                .unwrap_or(true)
        {
            let next_id = replacement_iter
                .next()
                .ok_or_else(|| "replacement sequence exhausted".to_string())?;
            reordered.push(next_id.to_string());
        } else {
            reordered.push(entry.id.clone());
        }
    }

    if replacement_iter.next().is_some() {
        return Err("replacement sequence was not fully consumed".to_string());
    }

    Ok(reordered)
}

// ── Endpoints ────────────────────────────────────────────────────────────────
