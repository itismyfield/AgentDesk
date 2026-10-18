//! Serializes live-dispatch owner publication with run cancellation on `aq_run:<run_id>`:
//! a publisher takes the owner run token first, re-reads the run and refuses a terminal one.

mod writes;

use sqlx::{Postgres, Row as SqlxRow, Transaction};

use super::phase_gates::PhaseGateSaveResult;
use super::run_status::is_live_run_status;

/// Runs owning dispatch `td`. The context arm uses the migration `json_extract(text, text)`
/// helper, which yields NULL for malformed JSON on PostgreSQL 15 and later.
pub(crate) const RUN_OWNERS_FOR_DISPATCH: &str = r#"
    SELECT e.run_id
    FROM auto_queue_entries e
    WHERE e.dispatch_id = td.id
      AND e.status IN ('pending', 'dispatched', 'user_cancelled')
    UNION ALL
    SELECT pg.run_id
    FROM auto_queue_phase_gates pg
    WHERE pg.dispatch_id = td.id
    UNION ALL
    SELECT owner_ctx.run_id
    FROM (SELECT json_extract(td.context, '$.phase_gate.run_id') AS run_id) owner_ctx
    WHERE owner_ctx.run_id IS NOT NULL
"#;

/// A guarded write failed either on the owner guard or on the database statement.
#[derive(Debug)]
pub(crate) enum OwnerWriteError {
    Guard(String),
    Db(sqlx::Error),
}

impl OwnerWriteError {
    /// Split a guard refusal (`Err`) from a database error the caller may classify.
    pub(crate) fn into_db(self) -> Result<sqlx::Error, String> {
        match self {
            Self::Guard(refusal) => Err(refusal),
            Self::Db(error) => Ok(error),
        }
    }
}

impl std::fmt::Display for OwnerWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Guard(refusal) => f.write_str(refusal),
            Self::Db(error) => write!(f, "{error}"),
        }
    }
}

impl From<String> for OwnerWriteError {
    fn from(message: String) -> Self {
        Self::Guard(message)
    }
}

/// Columns of a freshly created `pending` dispatch.
pub(crate) struct NewDispatchRow<'a> {
    pub(crate) id: &'a str,
    pub(crate) kanban_card_id: &'a str,
    pub(crate) to_agent_id: &'a str,
    pub(crate) dispatch_type: &'a str,
    pub(crate) title: &'a str,
    pub(crate) context: &'a str,
    pub(crate) parent_dispatch_id: Option<&'a str>,
    pub(crate) chain_depth: i64,
    pub(crate) required_capabilities: Option<&'a serde_json::Value>,
}

/// Columns of a force-kill retry dispatch, which copies the origin context.
pub(crate) struct NewRetryDispatchRow<'a> {
    pub(crate) id: &'a str,
    pub(crate) kanban_card_id: &'a str,
    pub(crate) to_agent_id: Option<&'a str>,
    pub(crate) dispatch_type: &'a str,
    pub(crate) title: &'a str,
    pub(crate) context: &'a str,
    pub(crate) retry_count: i64,
}

/// Phase-gate state after the caller trimmed and defaulted every field.
pub(crate) struct NormalizedPhaseGateState {
    pub(crate) status: String,
    pub(crate) verdict: Option<String>,
    pub(crate) pass_verdict: String,
    pub(crate) next_phase: Option<i64>,
    pub(crate) final_phase: bool,
    pub(crate) anchor_card_id: Option<String>,
    pub(crate) failure_reason: Option<String>,
    pub(crate) created_at: Option<String>,
}

/// Lock order: `aq_retry` < sorted `aq_run` < dispatch rows < two-argument phase key < gate
/// rows. Raw owner SQL stays in `writes`; no caller waits here while holding a row lock.
pub(crate) async fn lock_owner_runs_on_pg_tx(
    tx: &mut Transaction<'_, Postgres>,
    run_ids: &[String],
) -> Result<Vec<String>, String> {
    super::runs::acquire_run_advisory_xact_locks_on_pg_tx(tx, run_ids).await
}

/// Refuse when a locked run exists but no longer owns live work. A missing run row
/// has no cancellation to serialize with, so it passes.
async fn require_live_owner_runs_on_pg_tx(
    tx: &mut Transaction<'_, Postgres>,
    locked: &[String],
) -> Result<(), String> {
    if locked.is_empty() {
        return Ok(());
    }
    let rows = sqlx::query("SELECT id, status FROM auto_queue_runs WHERE id = ANY($1) ORDER BY id")
        .bind(locked)
        .fetch_all(&mut **tx)
        .await
        .map_err(|error| format!("reload auto-queue owner runs: {error}"))?;
    for row in rows {
        let id: String = row
            .try_get("id")
            .map_err(|error| format!("decode auto-queue owner run id: {error}"))?;
        let status: String = row
            .try_get("status")
            .map_err(|error| format!("decode auto-queue owner run {id} status: {error}"))?;
        if !is_live_run_status(&status) {
            return Err(format!(
                "auto-queue run {id} is {status}: refusing to publish a live dispatch owner"
            ));
        }
    }
    Ok(())
}

async fn context_owner_run_id_on_pg_tx(
    tx: &mut Transaction<'_, Postgres>,
    context: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(context) = context else {
        return Ok(None);
    };
    sqlx::query_scalar::<_, Option<String>>("SELECT json_extract($1::text, '$.phase_gate.run_id')")
        .bind(context)
        .fetch_one(&mut **tx)
        .await
        .map_err(|error| format!("read phase-gate owner from dispatch context: {error}"))
}

/// Guard a new dispatch whose context names a `phase_gate.run_id` owner. A caller holding an
/// entry owner's token passes it, and a different context run is refused, not locked late.
pub(crate) async fn guard_context_owner_publication_on_pg_tx(
    tx: &mut Transaction<'_, Postgres>,
    context: Option<&str>,
    already_locked_run: Option<&str>,
) -> Result<(), String> {
    let Some(context_run) = context_owner_run_id_on_pg_tx(tx, context).await? else {
        return Ok(());
    };
    match already_locked_run {
        Some(locked) if locked == context_run => Ok(()),
        Some(locked) => Err(format!(
            "ambiguous auto-queue ownership for retry: context run {context_run} differs from entry run {locked}"
        )),
        None => {
            let locked = lock_owner_runs_on_pg_tx(tx, &[context_run]).await?;
            require_live_owner_runs_on_pg_tx(tx, &locked).await
        }
    }
}

/// Insert a new `pending` dispatch after guarding its context owner.
pub(crate) async fn insert_dispatch_row_on_pg_tx(
    tx: &mut Transaction<'_, Postgres>,
    row: &NewDispatchRow<'_>,
) -> Result<(), OwnerWriteError> {
    guard_context_owner_publication_on_pg_tx(tx, Some(row.context), None).await?;
    writes::insert_dispatch_row(tx, row)
        .await
        .map_err(OwnerWriteError::Db)
}

/// Insert a retry dispatch after guarding the copied context owner against the entry
/// owner token the caller already holds.
pub(crate) async fn insert_retry_dispatch_row_on_pg_tx(
    tx: &mut Transaction<'_, Postgres>,
    row: &NewRetryDispatchRow<'_>,
    already_locked_run: Option<&str>,
) -> Result<(), OwnerWriteError> {
    guard_context_owner_publication_on_pg_tx(tx, Some(row.context), already_locked_run).await?;
    writes::insert_retry_dispatch_row(tx, row)
        .await
        .map_err(OwnerWriteError::Db)
}

async fn phase_gate_source_runs_on_pg_tx(
    tx: &mut Transaction<'_, Postgres>,
    run_id: &str,
    phase: i64,
    dispatch_ids: &[String],
) -> Result<Vec<(String, String)>, String> {
    sqlx::query_as::<_, (String, String)>(
        "SELECT DISTINCT dispatch_id, run_id
         FROM auto_queue_phase_gates
         WHERE dispatch_id = ANY($1)
           AND NOT (run_id = $2 AND phase = $3)
         ORDER BY dispatch_id, run_id",
    )
    .bind(dispatch_ids)
    .bind(run_id)
    .bind(phase)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| format!("load phase-gate owners for run {run_id} phase {phase}: {error}"))
}

/// Save one (run, phase) gate state. With dispatches it locks the target and current gate-owner
/// runs, then the dispatch rows, and re-reads the owners so a save that waited sees them.
pub(crate) async fn publish_phase_gate_state_on_pg_tx(
    tx: &mut Transaction<'_, Postgres>,
    run_id: &str,
    phase: i64,
    state: &NormalizedPhaseGateState,
    dispatch_ids: &[String],
) -> Result<PhaseGateSaveResult, String> {
    let mut persisted = Vec::new();
    if !dispatch_ids.is_empty() {
        let sources = phase_gate_source_runs_on_pg_tx(tx, run_id, phase, dispatch_ids).await?;
        let mut owner_runs = vec![run_id.to_string()];
        owner_runs.extend(sources.into_iter().map(|(_, source)| source));
        owner_runs.sort();
        owner_runs.dedup();
        let locked = lock_owner_runs_on_pg_tx(tx, &owner_runs).await?;

        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT id, status FROM task_dispatches WHERE id = ANY($1) ORDER BY id FOR UPDATE",
        )
        .bind(dispatch_ids)
        .fetch_all(&mut **tx)
        .await
        .map_err(|error| format!("lock postgres phase-gate dispatch rows: {error}"))?;
        let existing: std::collections::HashMap<String, String> = rows.into_iter().collect();
        persisted = dispatch_ids
            .iter()
            .filter(|dispatch_id| existing.contains_key(dispatch_id.as_str()))
            .cloned()
            .collect();

        for (dispatch_id, source) in
            phase_gate_source_runs_on_pg_tx(tx, run_id, phase, dispatch_ids).await?
        {
            if !locked.contains(&source) {
                return Err(format!(
                    "phase-gate dispatch {dispatch_id} gained owner run {source} outside the locked set; retry"
                ));
            }
        }

        let target_live =
            sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
                .bind(run_id)
                .fetch_optional(&mut **tx)
                .await
                .map_err(|error| format!("reload phase-gate target run {run_id}: {error}"))?
                .is_none_or(|status| is_live_run_status(&status));
        if !target_live
            && let Some(dispatch_id) = persisted.iter().find(|dispatch_id| {
                existing
                    .get(dispatch_id.as_str())
                    .is_some_and(|status| matches!(status.as_str(), "pending" | "dispatched"))
            })
        {
            return Err(format!(
                "auto-queue run {run_id} is not live: refusing to attach live dispatch {dispatch_id} to its phase gate"
            ));
        }
    }

    super::phase_gates::lock_phase_gate_state_on_pg_tx(tx, run_id, phase).await?;
    let removed_stale_rows =
        writes::delete_stale_phase_gate_rows(tx, run_id, phase, &persisted).await?;
    if persisted.is_empty() {
        writes::upsert_unattached_phase_gate_row(tx, run_id, phase, state).await?;
    } else {
        for dispatch_id in &persisted {
            writes::move_phase_gate_dispatch_row(tx, run_id, phase, state, dispatch_id).await?;
        }
    }
    Ok(PhaseGateSaveResult {
        persisted_dispatch_ids: persisted,
        removed_stale_rows,
    })
}
