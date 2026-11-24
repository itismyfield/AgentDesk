use super::*;

impl EntryDispatchFailureResult {
    /// One dispatch failure on a caller-owned tx: retry decision and transition audit under the
    /// entry and run locks; a non-live run makes the entry terminal instead of blocking.
    pub(crate) async fn write_on_pg_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        entry_id: &str,
        max_retries: i64,
        trigger_source: &str,
    ) -> Result<Self, String> {
        let retry_limit = max_retries.max(1);
        let current = load_entry_status_row_pg_tx_for_dispatch_failure(tx, entry_id).await?;
        // An entry whose dispatch already started (unclassified, no-effect or withheld) keeps its
        // link, slot and retry count; requeueing it would run the same request again.
        let replay_held = match current.dispatch_id.as_deref() {
            Some(dispatch_id) => {
                crate::db::replay_disposition::dispatch_blocked_on_tx(tx, dispatch_id)
                    .await
                    .map_err(|error| {
                        format!("read replay hold of {dispatch_id} for entry {entry_id}: {error}")
                    })?
            }
            None => false,
        };
        if current.status != ENTRY_STATUS_DISPATCHED || replay_held {
            return Ok(Self {
                run_id: current.run_id,
                from_status: current.status.clone(),
                to_status: current.status,
                retry_count: current.retry_count,
                retry_limit,
                failure_transition_id: None,
                changed: false,
            });
        }

        let retry_count = current.retry_count.saturating_add(1);
        let run_status = sqlx::query_scalar::<_, String>(
            "SELECT status
             FROM auto_queue_runs
             WHERE id = $1
             FOR UPDATE",
        )
        .bind(&current.run_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|error| {
            format!(
                "lock auto-queue run {} for dispatch failure {entry_id}: {error}",
                current.run_id
            )
        })?
        .ok_or_else(|| format!("auto-queue run not found: {}", current.run_id))?;
        // Dispatch termination must never depend on whether the owning run can
        // accept another attempt. A live run receives the retry while a terminal
        // or otherwise non-live run converts the entry to terminal `failed`.
        // This also closes the real cancel window where the run is already
        // `cancelled` but its dispatched entry has not reached `skipped` yet.
        let target_status = if retry_count >= retry_limit
            || !crate::db::auto_queue::run_status::is_live_run_status(&run_status)
        {
            ENTRY_STATUS_FAILED
        } else {
            ENTRY_STATUS_PENDING
        };

        let rows_affected = sqlx::query(
            "UPDATE auto_queue_entries
             SET status = $1,
                 dispatch_id = NULL,
                 slot_index = NULL,
                 dispatched_at = NULL,
                 completed_at = CASE
                     WHEN $1 = 'failed' THEN NOW()
                     ELSE NULL
                 END,
                 retry_count = $2
             WHERE id = $3
               AND status = 'dispatched'
               AND retry_count = $4",
        )
        .bind(target_status)
        .bind(retry_count)
        .bind(entry_id)
        .bind(current.retry_count)
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            format!("update postgres auto-queue dispatch failure {entry_id}: {error}")
        })?
        .rows_affected();
        if rows_affected == 0 {
            return Ok(Self {
                run_id: current.run_id,
                from_status: current.status.clone(),
                to_status: current.status,
                retry_count: current.retry_count,
                retry_limit,
                failure_transition_id: None,
                changed: false,
            });
        }

        let failure_transition_id = record_entry_transition_on_pg(
            tx,
            entry_id,
            ENTRY_STATUS_DISPATCHED,
            target_status,
            trigger_source,
        )
        .await?;
        Ok(Self {
            run_id: current.run_id,
            from_status: ENTRY_STATUS_DISPATCHED.to_string(),
            to_status: target_status.to_string(),
            retry_count,
            retry_limit,
            failure_transition_id: Some(failure_transition_id),
            changed: true,
        })
    }

    /// Whether this write left an entry `failed` that owes a run-completion attempt.
    pub(crate) fn run_finalize_due(&self) -> bool {
        self.changed && self.to_status == ENTRY_STATUS_FAILED
    }
}

async fn load_entry_status_row_pg_tx_for_dispatch_failure(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    entry_id: &str,
) -> Result<EntryStatusRow, String> {
    let row = sqlx::query(
        "SELECT run_id,
                COALESCE(kanban_card_id, '') AS kanban_card_id,
                COALESCE(agent_id, '') AS agent_id,
                status,
                dispatch_id,
                COALESCE(retry_count, 0)::BIGINT AS retry_count,
                slot_index::BIGINT AS slot_index,
                COALESCE(thread_group, 0)::BIGINT AS thread_group,
                COALESCE(batch_phase, 0)::BIGINT AS batch_phase,
                completed_at::text AS completed_at
         FROM auto_queue_entries
         WHERE id = $1
         FOR UPDATE",
    )
    .bind(entry_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("lock postgres auto-queue entry {entry_id}: {error}"))?
    .ok_or_else(|| format!("auto-queue entry not found: {entry_id}"))?;

    Ok(EntryStatusRow {
        run_id: row
            .try_get("run_id")
            .map_err(|error| format!("decode auto-queue entry {entry_id} run_id: {error}"))?,
        card_id: row.try_get("kanban_card_id").map_err(|error| {
            format!("decode auto-queue entry {entry_id} kanban_card_id: {error}")
        })?,
        agent_id: row
            .try_get("agent_id")
            .map_err(|error| format!("decode auto-queue entry {entry_id} agent_id: {error}"))?,
        status: row
            .try_get("status")
            .map_err(|error| format!("decode auto-queue entry {entry_id} status: {error}"))?,
        dispatch_id: row
            .try_get("dispatch_id")
            .map_err(|error| format!("decode auto-queue entry {entry_id} dispatch_id: {error}"))?,
        retry_count: row
            .try_get("retry_count")
            .map_err(|error| format!("decode auto-queue entry {entry_id} retry_count: {error}"))?,
        slot_index: row
            .try_get("slot_index")
            .map_err(|error| format!("decode auto-queue entry {entry_id} slot_index: {error}"))?,
        thread_group: row
            .try_get("thread_group")
            .map_err(|error| format!("decode auto-queue entry {entry_id} thread_group: {error}"))?,
        batch_phase: row
            .try_get("batch_phase")
            .map_err(|error| format!("decode auto-queue entry {entry_id} batch_phase: {error}"))?,
        completed_at: row
            .try_get("completed_at")
            .map_err(|error| format!("decode auto-queue entry {entry_id} completed_at: {error}"))?,
    })
}
