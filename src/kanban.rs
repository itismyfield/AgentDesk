//! Central kanban state machine.
//!
//! ALL card status transitions MUST go through the Postgres transition helpers.
//! This ensures hooks fire, auto-queue syncs, and notifications are sent.
//!
//! ## Pipeline-Driven Transitions (#106 P5)
//!
//! All transition rules, gates, hooks, clocks, and timeouts are defined in
//! `policies/default-pipeline.yaml`. No hardcoded state names exist in this module.
//! See the YAML file for the complete state machine specification.
//!
//! Custom pipelines can override the default via repo or agent-level overrides
//! (3-level inheritance: default → repo → agent).

use crate::db::Db;
use crate::engine::PolicyEngine;
use anyhow::Result;
use serde_json::json;
use sqlx::Row as SqlxRow;

#[cfg(test)]
fn clear_escalation_alert_state_on_conn(
    conn: &rusqlite::Connection,
    card_id: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM kv_meta WHERE key IN (?1, ?2)",
        rusqlite::params![
            format!("pm_pending:{card_id}"),
            format!("pm_decision_sent:{card_id}")
        ],
    )?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn cleanup_force_transition_revert_fields_on_conn(
    conn: &rusqlite::Connection,
    card_id: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE kanban_cards \
         SET latest_dispatch_id = NULL, review_status = NULL, \
             review_round = 0, review_notes = NULL, suggestion_pending_at = NULL, \
             review_entered_at = NULL, awaiting_dod_at = NULL, blocked_reason = NULL, \
             updated_at = datetime('now') \
         WHERE id = ?1",
        [card_id],
    )?;
    conn.execute(
        "INSERT INTO card_review_state (
            card_id, review_round, state, pending_dispatch_id, last_verdict, last_decision,
            decided_by, decided_at, approach_change_round, session_reset_round, review_entered_at, updated_at
         ) VALUES (
            ?1, 0, 'idle', NULL, NULL, NULL,
            NULL, NULL, NULL, NULL, NULL, datetime('now')
         )
         ON CONFLICT(card_id) DO UPDATE SET
            review_round = 0,
            state = 'idle',
            pending_dispatch_id = NULL,
            last_verdict = NULL,
            last_decision = NULL,
            decided_by = NULL,
            decided_at = NULL,
            approach_change_round = NULL,
            session_reset_round = NULL,
            review_entered_at = NULL,
            updated_at = datetime('now')",
        [card_id],
    )?;
    clear_escalation_alert_state_on_conn(conn, card_id)?;
    strip_stale_worktree_metadata_from_dispatches_on_conn(conn, card_id)?;
    Ok(())
}

/// #800: Strip recorded worktree metadata from every `task_dispatches` row that
/// belongs to the given card.
///
/// `reset_full=true` reopens (`POST /api/kanban-cards/:id/reopen`) advertise a
/// "full reset" but historically only cleared `card_review_state` and a handful
/// of `kanban_cards` columns. The persisted dispatch JSON kept its old
/// `worktree_path` / `worktree_branch` / `completed_*` fields, so the very next
/// `latest_completed_work_dispatch_target()` call would silently re-inject the
/// stale path into the new dispatch context — defeating the reset and steering
/// the agent back into the orphaned worktree.
///
/// This helper rewrites the `context` and `result` JSON columns to drop the
/// worktree-locating keys (`worktree_path`, `worktree_branch`,
/// `completed_worktree_path`, `completed_branch`). Other fields on those JSON
/// blobs (titles, prompts, completion evidence like `completed_commit`) are
/// preserved so audit history remains intact. Rows whose JSON is malformed or
/// already lacks the keys are left untouched.
#[cfg(test)]
fn strip_stale_worktree_metadata_from_dispatches_on_conn(
    conn: &rusqlite::Connection,
    card_id: &str,
) -> anyhow::Result<()> {
    const STALE_KEYS: &[&str] = &[
        "worktree_path",
        "worktree_branch",
        "completed_worktree_path",
        "completed_branch",
    ];

    let mut stmt =
        conn.prepare("SELECT id, context, result FROM task_dispatches WHERE kanban_card_id = ?1")?;
    let rows: Vec<(String, Option<String>, Option<String>)> = stmt
        .query_map([card_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .filter_map(|row| row.ok())
        .collect();
    drop(stmt);

    for (dispatch_id, context_raw, result_raw) in rows {
        let new_context = scrub_worktree_keys_from_json(context_raw.as_deref(), STALE_KEYS);
        let new_result = scrub_worktree_keys_from_json(result_raw.as_deref(), STALE_KEYS);

        if new_context.is_none() && new_result.is_none() {
            continue;
        }

        let context_value: Option<String> = new_context.or(context_raw);
        let result_value: Option<String> = new_result.or(result_raw);

        conn.execute(
            "UPDATE task_dispatches SET context = ?1, result = ?2, updated_at = datetime('now') WHERE id = ?3",
            rusqlite::params![context_value, result_value, dispatch_id],
        )?;
    }
    Ok(())
}

/// Returns `Some(serialized)` when at least one of `keys` was present in the
/// parsed JSON object, with those keys removed; otherwise returns `None` to
/// signal "no rewrite needed". `None` input or non-object payloads are passed
/// through as `None` so the caller leaves the column untouched.
fn scrub_worktree_keys_from_json(raw: Option<&str>, keys: &[&str]) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let mut value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let obj = value.as_object_mut()?;
    let mut changed = false;
    for key in keys {
        if obj.remove(*key).is_some() {
            changed = true;
        }
    }
    if !changed {
        return None;
    }
    serde_json::to_string(&value).ok()
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PgTransitionCleanupCounts {
    pub cancelled_dispatches: usize,
    pub skipped_auto_queue_entries: usize,
}

async fn clear_escalation_alert_state_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM kv_meta WHERE key = ANY($1)")
        .bind(vec![
            format!("pm_pending:{card_id}"),
            format!("pm_decision_sent:{card_id}"),
        ])
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!("clear postgres escalation state for {card_id}: {error}")
        })?;
    Ok(())
}

async fn strip_stale_worktree_metadata_from_dispatches_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> anyhow::Result<()> {
    const STALE_KEYS: &[&str] = &[
        "worktree_path",
        "worktree_branch",
        "completed_worktree_path",
        "completed_branch",
    ];

    let rows = sqlx::query(
        "SELECT id, context::text AS context, result::text AS result
         FROM task_dispatches
         WHERE kanban_card_id = $1",
    )
    .bind(card_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!("load postgres dispatch cleanup rows for {card_id}: {error}")
    })?;

    for row in rows {
        let dispatch_id: String = row.try_get("id").map_err(|error| {
            anyhow::anyhow!("decode postgres dispatch id for {card_id}: {error}")
        })?;
        let context_raw: Option<String> = row.try_get("context").map_err(|error| {
            anyhow::anyhow!("decode postgres dispatch context for {dispatch_id}: {error}")
        })?;
        let result_raw: Option<String> = row.try_get("result").map_err(|error| {
            anyhow::anyhow!("decode postgres dispatch result for {dispatch_id}: {error}")
        })?;

        let new_context = scrub_worktree_keys_from_json(context_raw.as_deref(), STALE_KEYS);
        let new_result = scrub_worktree_keys_from_json(result_raw.as_deref(), STALE_KEYS);

        if new_context.is_none() && new_result.is_none() {
            continue;
        }

        let context_value: Option<String> = new_context.or(context_raw);
        let result_value: Option<String> = new_result.or(result_raw);

        sqlx::query(
            "UPDATE task_dispatches
             SET context = $1::jsonb,
                 result = $2::jsonb,
                 updated_at = NOW()
             WHERE id = $3",
        )
        .bind(context_value)
        .bind(result_value)
        .bind(&dispatch_id)
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!("save postgres dispatch cleanup row {dispatch_id}: {error}")
        })?;
    }

    Ok(())
}

async fn skip_live_auto_queue_entries_for_card_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> anyhow::Result<usize> {
    let rows = sqlx::query(
        "SELECT id
         FROM auto_queue_entries
         WHERE kanban_card_id = $1
           AND status IN ('pending', 'dispatched')
           AND run_id IN (
               SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
           )",
    )
    .bind(card_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| anyhow::anyhow!("load postgres auto-queue entries for {card_id}: {error}"))?;

    let mut changed = 0usize;
    for row in rows {
        let entry_id: String = row.try_get("id").map_err(|error| {
            anyhow::anyhow!("decode postgres auto-queue entry for {card_id}: {error}")
        })?;
        let result = crate::db::auto_queue::update_entry_status_on_pg_tx(
            tx,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
            "force_transition_cleanup",
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        )
        .await
        .map_err(|error| anyhow::anyhow!("skip postgres auto-queue entry {entry_id}: {error}"))?;
        if result.changed {
            changed += 1;
        }
    }

    Ok(changed)
}

async fn count_live_auto_queue_entries_for_card_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> anyhow::Result<usize> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM auto_queue_entries
         WHERE kanban_card_id = $1
           AND status IN ('pending', 'dispatched')
           AND run_id IN (
               SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
           )",
    )
    .bind(card_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!("count postgres live auto-queue entries for {card_id}: {error}")
    })?;
    Ok(count.max(0) as usize)
}

async fn clear_force_transition_terminalized_links_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE auto_queue_entries
         SET dispatch_id = NULL,
             slot_index = NULL,
             dispatched_at = NULL,
             completed_at = COALESCE(completed_at, NOW())
         WHERE kanban_card_id = $1
           AND status = 'skipped'
           AND dispatch_id IS NOT NULL
           AND run_id IN (
               SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
           )",
    )
    .bind(card_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!(
            "clear postgres force-transition terminalized auto-queue links for {card_id}: {error}"
        )
    })?;
    Ok(())
}

async fn count_live_dispatches_for_card_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> anyhow::Result<usize> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| anyhow::anyhow!("count postgres live dispatches for {card_id}: {error}"))?;
    Ok(count.max(0) as usize)
}

async fn cancel_active_dispatches_for_card_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
    reason: Option<&str>,
) -> anyhow::Result<PgTransitionCleanupCounts> {
    let rows = sqlx::query(
        "SELECT id
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| anyhow::anyhow!("load postgres live dispatches for {card_id}: {error}"))?;
    let dispatch_ids: Vec<String> = rows
        .into_iter()
        .map(|row| row.try_get("id"))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            anyhow::anyhow!("decode postgres live dispatch id for {card_id}: {error}")
        })?;

    if dispatch_ids.is_empty() {
        return Ok(PgTransitionCleanupCounts::default());
    }

    sqlx::query(
        "UPDATE sessions
         SET status = CASE WHEN status = 'working' THEN 'idle' ELSE status END,
             active_dispatch_id = NULL
         WHERE active_dispatch_id = ANY($1)",
    )
    .bind(&dispatch_ids)
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!("clear postgres live session dispatches for {card_id}: {error}")
    })?;

    let cancel_payload = reason
        .map(|value| json!({ "reason": value, "completion_source": "force_transition" }))
        .unwrap_or_else(|| json!({ "completion_source": "force_transition" }));
    let mut counts = PgTransitionCleanupCounts::default();
    for dispatch_id in dispatch_ids {
        let rows_affected = sqlx::query(
            "UPDATE task_dispatches
             SET status = 'cancelled',
                 updated_at = NOW(),
                 completed_at = COALESCE(completed_at, NOW()),
                 result = COALESCE(result, CAST($2 AS jsonb)::text)
             WHERE id = $1
               AND status IN ('pending', 'dispatched')",
        )
        .bind(&dispatch_id)
        .bind(cancel_payload.to_string())
        .execute(&mut **tx)
        .await
        .map_err(|error| anyhow::anyhow!("cancel postgres dispatch {dispatch_id}: {error}"))?
        .rows_affected();
        counts.cancelled_dispatches += rows_affected as usize;

        // Route the force-skip through the shared entry transition helper so
        // PG bookkeeping mirrors SQLite: transition rows are recorded and
        // single-entry runs can finalize. Preserve the dispatch link afterward
        // for abandoned-dispatch lineage.
        counts.skipped_auto_queue_entries += crate::db::auto_queue::sync_dispatch_terminal_entries_on_pg_tx(
            tx,
            &dispatch_id,
            crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
            "force_transition_cleanup",
            true,
        )
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "mark postgres live auto-queue entry skipped during force-transition cancel {dispatch_id}: {error}"
            )
        })?;
    }

    Ok(counts)
}

async fn cleanup_force_transition_revert_fields_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> anyhow::Result<()> {
    use crate::engine::transition::TransitionIntent;

    crate::engine::transition_executor_pg::execute_pg_transition_intent(
        tx,
        &TransitionIntent::SetLatestDispatchId {
            card_id: card_id.to_string(),
            dispatch_id: None,
        },
    )
    .await
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    crate::engine::transition_executor_pg::execute_pg_transition_intent(
        tx,
        &TransitionIntent::SetReviewStatus {
            card_id: card_id.to_string(),
            review_status: None,
        },
    )
    .await
    .map_err(|error| anyhow::anyhow!("{error}"))?;

    sqlx::query(
        "UPDATE kanban_cards
         SET review_round = 0,
             review_notes = NULL,
             suggestion_pending_at = NULL,
             review_entered_at = NULL,
             awaiting_dod_at = NULL,
             blocked_reason = NULL,
             updated_at = NOW()
         WHERE id = $1",
    )
    .bind(card_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!("reset postgres kanban cleanup fields for {card_id}: {error}")
    })?;

    sqlx::query(
        "INSERT INTO card_review_state (
            card_id, review_round, state, pending_dispatch_id, last_verdict, last_decision,
            decided_by, decided_at, approach_change_round, session_reset_round, review_entered_at, updated_at
         ) VALUES (
            $1, 0, 'idle', NULL, NULL, NULL,
            NULL, NULL, NULL, NULL, NULL, NOW()
         )
         ON CONFLICT(card_id) DO UPDATE SET
            review_round = 0,
            state = 'idle',
            pending_dispatch_id = NULL,
            last_verdict = NULL,
            last_decision = NULL,
            decided_by = NULL,
            decided_at = NULL,
            approach_change_round = NULL,
            session_reset_round = NULL,
            review_entered_at = NULL,
            updated_at = NOW()",
    )
    .bind(card_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| anyhow::anyhow!("reset postgres review state for {card_id}: {error}"))?;

    clear_escalation_alert_state_on_pg_tx(tx, card_id).await?;
    strip_stale_worktree_metadata_from_dispatches_on_pg_tx(tx, card_id).await?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn log_audit_on_conn(
    conn: &rusqlite::Connection,
    card_id: &str,
    from: &str,
    to: &str,
    source: &str,
    result: &str,
) {
    log_audit(conn, card_id, from, to, source, result);
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowedOnConnMutation {
    ForceTransitionRevertCleanup,
    ForceTransitionTerminalCleanup,
    TestOnlyRollbackGuard,
    TestOnlyManualInterventionCleanup,
}

impl AllowedOnConnMutation {
    fn audit_value(self) -> &'static str {
        match self {
            Self::ForceTransitionRevertCleanup => "force_transition_revert_cleanup",
            Self::ForceTransitionTerminalCleanup => "force_transition_terminal_cleanup",
            Self::TestOnlyRollbackGuard => "test_only_rollback_guard",
            Self::TestOnlyManualInterventionCleanup => "test_only_manual_intervention_cleanup",
        }
    }

    fn rationale(self) -> &'static str {
        match self {
            Self::ForceTransitionRevertCleanup => {
                "same transaction required to clear review and dispatch residue while rewinding status"
            }
            Self::ForceTransitionTerminalCleanup => {
                "same transaction required to cancel stale dispatches before terminal status commits"
            }
            Self::TestOnlyRollbackGuard => {
                "test-only rollback probe for transition + cleanup atomicity"
            }
            Self::TestOnlyManualInterventionCleanup => {
                "test-only cleanup for escalation-cooldown clearing assertions"
            }
        }
    }
}

async fn execute_allowed_cleanup_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
    new_status: &str,
    on_pg_policy: AllowedOnConnMutation,
) -> Result<PgTransitionCleanupCounts> {
    let mut counts = PgTransitionCleanupCounts::default();

    match on_pg_policy {
        AllowedOnConnMutation::ForceTransitionRevertCleanup => {
            let reason = format!("force-transition to {new_status}");
            // Model 2: generic cancel keeps the dispatch pointer for
            // provenance. Force-transition cleanup is the explicit terminal
            // cleanup path, so it preserves the detailed cancel bookkeeping
            // and then clears any skipped links that cancel's side-effect left.
            let cancelled_counts =
                cancel_active_dispatches_for_card_on_pg_tx(tx, card_id, Some(&reason)).await?;
            counts.cancelled_dispatches = cancelled_counts.cancelled_dispatches;
            counts.skipped_auto_queue_entries = cancelled_counts.skipped_auto_queue_entries;
            counts.skipped_auto_queue_entries +=
                skip_live_auto_queue_entries_for_card_on_pg_tx(tx, card_id).await?;
            clear_force_transition_terminalized_links_on_pg_tx(tx, card_id).await?;
            cleanup_force_transition_revert_fields_on_pg_tx(tx, card_id).await?;
        }
        AllowedOnConnMutation::ForceTransitionTerminalCleanup => {
            counts.cancelled_dispatches =
                count_live_dispatches_for_card_on_pg_tx(tx, card_id).await?;
            crate::engine::transition_executor_pg::cancel_live_dispatches_for_terminal_card_pg(
                tx, card_id,
            )
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        }
        AllowedOnConnMutation::TestOnlyRollbackGuard => {
            return Err(anyhow::anyhow!("cleanup failed"));
        }
        AllowedOnConnMutation::TestOnlyManualInterventionCleanup => {
            clear_escalation_alert_state_on_pg_tx(tx, card_id).await?;
        }
    }

    Ok(counts)
}

async fn transition_status_with_opts_pg_inner(
    db: Option<&Db>,
    pg_pool: &sqlx::PgPool,
    engine: &PolicyEngine,
    card_id: &str,
    new_status: &str,
    source: &str,
    force_intent: crate::engine::transition::ForceIntent,
    on_pg_policy: Option<AllowedOnConnMutation>,
) -> Result<(TransitionResult, PgTransitionCleanupCounts)> {
    use crate::engine::transition::{
        self, CardState, GateSnapshot, TransitionContext, TransitionOutcome,
    };

    let row = sqlx::query(
        "SELECT
            status,
            review_status,
            latest_dispatch_id,
            repo_id,
            assigned_agent_id,
            review_entered_at::text AS review_entered_at,
            blocked_reason
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pg_pool)
    .await
    .map_err(|error| anyhow::anyhow!("load postgres card {card_id}: {error}"))?
    .ok_or_else(|| anyhow::anyhow!("card not found: {card_id}"))?;

    let old_status: String = row
        .try_get("status")
        .map_err(|error| anyhow::anyhow!("decode status for {card_id}: {error}"))?;
    let review_status: Option<String> = row
        .try_get("review_status")
        .map_err(|error| anyhow::anyhow!("decode review_status for {card_id}: {error}"))?;
    let latest_dispatch_id: Option<String> = row
        .try_get("latest_dispatch_id")
        .map_err(|error| anyhow::anyhow!("decode latest_dispatch_id for {card_id}: {error}"))?;
    let card_repo_id: Option<String> = row
        .try_get("repo_id")
        .map_err(|error| anyhow::anyhow!("decode repo_id for {card_id}: {error}"))?;
    let card_agent_id: Option<String> = row
        .try_get("assigned_agent_id")
        .map_err(|error| anyhow::anyhow!("decode assigned_agent_id for {card_id}: {error}"))?;
    let review_entered_at: Option<String> = row
        .try_get("review_entered_at")
        .map_err(|error| anyhow::anyhow!("decode review_entered_at for {card_id}: {error}"))?;
    let blocked_reason: Option<String> = row
        .try_get("blocked_reason")
        .map_err(|error| anyhow::anyhow!("decode blocked_reason for {card_id}: {error}"))?;

    if old_status == new_status {
        return Ok((
            TransitionResult {
                changed: false,
                from: old_status,
                to: new_status.to_string(),
            },
            PgTransitionCleanupCounts::default(),
        ));
    }

    crate::pipeline::ensure_loaded();
    let effective =
        resolve_pipeline_with_pg(pg_pool, card_repo_id.as_deref(), card_agent_id.as_deref())
            .await?;

    let has_active_dispatch = sqlx::query_scalar::<_, bool>(
        "SELECT COUNT(*) > 0
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .fetch_one(pg_pool)
    .await
    .map_err(|error| anyhow::anyhow!("load active dispatch gate for {card_id}: {error}"))?;

    let latest_review_verdict = sqlx::query_scalar::<_, Option<String>>(
        "SELECT result::jsonb ->> 'verdict'
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type = 'review'
           AND status = 'completed'
           AND ($2::timestamptz IS NULL OR COALESCE(completed_at, updated_at) >= $2::timestamptz)
         ORDER BY COALESCE(completed_at, updated_at) DESC, id DESC
         LIMIT 1",
    )
    .bind(card_id)
    .bind(review_entered_at.as_deref())
    .fetch_optional(pg_pool)
    .await
    .map_err(|error| anyhow::anyhow!("load latest review verdict for {card_id}: {error}"))?
    .flatten();

    let ctx = TransitionContext {
        card: CardState {
            id: card_id.to_string(),
            status: old_status.clone(),
            review_status: review_status.clone(),
            latest_dispatch_id: latest_dispatch_id.clone(),
        },
        pipeline: effective.clone(),
        gates: GateSnapshot {
            has_active_dispatch,
            review_verdict_pass: matches!(
                latest_review_verdict.as_deref(),
                Some("pass") | Some("approved")
            ),
            review_verdict_rework: matches!(
                latest_review_verdict.as_deref(),
                Some("rework") | Some("improve") | Some("reject")
            ),
        },
    };

    let decision = transition::decide_status_transition_with_caller(
        &ctx,
        new_status,
        source,
        force_intent,
        "kanban::transition_status_with_opts_pg",
    );

    if let TransitionOutcome::Blocked(ref reason) = decision.outcome {
        let mut tx = pg_pool
            .begin()
            .await
            .map_err(|error| anyhow::anyhow!("begin blocked postgres transition tx: {error}"))?;
        for intent in &decision.intents {
            crate::engine::transition_executor_pg::execute_pg_transition_intent(&mut tx, intent)
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
        }
        tx.commit()
            .await
            .map_err(|error| anyhow::anyhow!("commit blocked postgres transition tx: {error}"))?;
        tracing::warn!(
            "[kanban] Blocked postgres transition {} → {} for card {} (source: {}): {}",
            old_status,
            new_status,
            card_id,
            source,
            reason
        );
        return Err(anyhow::anyhow!("{}", reason));
    }

    if decision.outcome == TransitionOutcome::NoOp {
        return Ok((
            TransitionResult {
                changed: false,
                from: old_status,
                to: new_status.to_string(),
            },
            PgTransitionCleanupCounts::default(),
        ));
    }

    let old_manual_intervention = crate::manual_intervention::requires_manual_intervention(
        review_status.as_deref(),
        blocked_reason.as_deref(),
    );

    let mut tx = pg_pool
        .begin()
        .await
        .map_err(|error| anyhow::anyhow!("begin postgres transition tx: {error}"))?;

    for intent in &decision.intents {
        crate::engine::transition_executor_pg::execute_pg_transition_intent(&mut tx, intent)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
    }

    let cleanup_counts = if let Some(policy) = on_pg_policy {
        tracing::debug!(
            card_id,
            source,
            on_pg_policy = policy.audit_value(),
            rationale = policy.rationale(),
            "[kanban] executing allowlisted postgres cleanup after transition intents"
        );
        execute_allowed_cleanup_on_pg_tx(&mut tx, card_id, new_status, policy).await?
    } else {
        let mut counts = PgTransitionCleanupCounts::default();
        if effective.is_terminal(new_status) {
            counts.cancelled_dispatches =
                count_live_dispatches_for_card_on_pg_tx(&mut tx, card_id).await?;
            crate::engine::transition_executor_pg::cancel_live_dispatches_for_terminal_card_pg(
                &mut tx, card_id,
            )
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        }
        counts
    };

    let new_state_row = sqlx::query(
        "SELECT review_status, blocked_reason
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| anyhow::anyhow!("reload postgres card state for {card_id}: {error}"))?;
    let new_review_status: Option<String> = new_state_row
        .try_get("review_status")
        .map_err(|error| anyhow::anyhow!("decode new review_status for {card_id}: {error}"))?;
    let new_blocked_reason: Option<String> = new_state_row
        .try_get("blocked_reason")
        .map_err(|error| anyhow::anyhow!("decode new blocked_reason for {card_id}: {error}"))?;

    let new_manual_intervention = crate::manual_intervention::requires_manual_intervention(
        new_review_status.as_deref(),
        new_blocked_reason.as_deref(),
    );
    if old_manual_intervention && !new_manual_intervention {
        clear_escalation_alert_state_on_pg_tx(&mut tx, card_id).await?;
    }

    tx.commit()
        .await
        .map_err(|error| anyhow::anyhow!("commit postgres transition tx: {error}"))?;

    github_sync_on_transition_pg(pg_pool, &effective, card_id, new_status).await;
    fire_dynamic_hooks(
        engine,
        &effective,
        card_id,
        &old_status,
        new_status,
        Some(source),
    );

    if effective.is_terminal(new_status)
        && record_true_negative_if_pass_with_backends(db, Some(pg_pool), card_id)
    {
        crate::server::routes::review_verdict::spawn_aggregate_if_needed_with_pg(Some(
            pg_pool.clone(),
        ));
    }

    Ok((
        TransitionResult {
            changed: true,
            from: old_status,
            to: new_status.to_string(),
        },
        cleanup_counts,
    ))
}

pub async fn transition_status_with_opts_pg_only(
    pg_pool: &sqlx::PgPool,
    engine: &PolicyEngine,
    card_id: &str,
    new_status: &str,
    source: &str,
    force_intent: crate::engine::transition::ForceIntent,
) -> Result<TransitionResult> {
    transition_status_with_opts_pg_inner(
        None,
        pg_pool,
        engine,
        card_id,
        new_status,
        source,
        force_intent,
        None,
    )
    .await
    .map(|(result, _)| result)
}

pub async fn transition_status_with_opts_pg(
    db: Option<&Db>,
    pg_pool: &sqlx::PgPool,
    engine: &PolicyEngine,
    card_id: &str,
    new_status: &str,
    source: &str,
    force_intent: crate::engine::transition::ForceIntent,
) -> Result<TransitionResult> {
    transition_status_with_opts_pg_inner(
        db,
        pg_pool,
        engine,
        card_id,
        new_status,
        source,
        force_intent,
        None,
    )
    .await
    .map(|(result, _)| result)
}

pub async fn transition_status_with_opts_and_allowed_cleanup_pg_only(
    pg_pool: &sqlx::PgPool,
    engine: &PolicyEngine,
    card_id: &str,
    new_status: &str,
    source: &str,
    force_intent: crate::engine::transition::ForceIntent,
    on_pg_policy: AllowedOnConnMutation,
) -> Result<(TransitionResult, PgTransitionCleanupCounts)> {
    transition_status_with_opts_pg_inner(
        None,
        pg_pool,
        engine,
        card_id,
        new_status,
        source,
        force_intent,
        Some(on_pg_policy),
    )
    .await
}

pub async fn transition_status_with_opts_and_allowed_cleanup_pg(
    db: Option<&Db>,
    pg_pool: &sqlx::PgPool,
    engine: &PolicyEngine,
    card_id: &str,
    new_status: &str,
    source: &str,
    force_intent: crate::engine::transition::ForceIntent,
    on_pg_policy: AllowedOnConnMutation,
) -> Result<(TransitionResult, PgTransitionCleanupCounts)> {
    transition_status_with_opts_pg_inner(
        db,
        pg_pool,
        engine,
        card_id,
        new_status,
        source,
        force_intent,
        Some(on_pg_policy),
    )
    .await
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct TransitionResult {
    pub changed: bool,
    pub from: String,
    pub to: String,
}

/// Fire hooks dynamically based on the effective pipeline's hooks section (#106 P5).
///
/// All hook bindings come from the YAML pipeline definition.
/// States without hook bindings simply fire no hooks.
fn fire_dynamic_hooks(
    engine: &PolicyEngine,
    pipeline: &crate::pipeline::PipelineConfig,
    card_id: &str,
    old_status: &str,
    new_status: &str,
    source: Option<&str>,
) {
    let mut payload = json!({
        "card_id": card_id,
        "from": old_status,
        "to": new_status,
        "status": new_status,
    });
    if let Some(source) = source {
        payload["source"] = json!(source);
    }

    // Fire on_exit hooks for the state being LEFT
    if let Some(bindings) = pipeline.hooks_for_state(old_status) {
        for hook_name in &bindings.on_exit {
            let _ = engine.try_fire_hook_by_name(hook_name, payload.clone());
        }
    }
    // Fire on_enter hooks for the state being ENTERED
    if let Some(bindings) = pipeline.hooks_for_state(new_status) {
        for hook_name in &bindings.on_enter {
            let _ = engine.try_fire_hook_by_name(hook_name, payload.clone());
        }
    }
    // No fallback — YAML is the sole source of truth for hook bindings.
}

pub(crate) async fn resolve_pipeline_with_pg(
    pg_pool: &sqlx::PgPool,
    repo_id: Option<&str>,
    agent_id: Option<&str>,
) -> Result<crate::pipeline::PipelineConfig> {
    let repo_override = if let Some(repo_id) = repo_id {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT pipeline_config::text AS pipeline_config
             FROM github_repos
             WHERE id = $1",
        )
        .bind(repo_id)
        .fetch_optional(pg_pool)
        .await
        .map_err(|error| anyhow::anyhow!("load repo pipeline override for {repo_id}: {error}"))?
        .flatten()
        .map(|json| crate::pipeline::parse_override(&json))
        .transpose()
        .map_err(|error| anyhow::anyhow!("parse repo pipeline override for {repo_id}: {error}"))?
        .flatten()
    } else {
        None
    };

    let agent_override = if let Some(agent_id) = agent_id {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT pipeline_config::text AS pipeline_config
             FROM agents
             WHERE id = $1",
        )
        .bind(agent_id)
        .fetch_optional(pg_pool)
        .await
        .map_err(|error| anyhow::anyhow!("load agent pipeline override for {agent_id}: {error}"))?
        .flatten()
        .map(|json| crate::pipeline::parse_override(&json))
        .transpose()
        .map_err(|error| anyhow::anyhow!("parse agent pipeline override for {agent_id}: {error}"))?
        .flatten()
    } else {
        None
    };

    Ok(crate::pipeline::resolve(
        repo_override.as_ref(),
        agent_override.as_ref(),
    ))
}

async fn github_sync_on_transition_pg(
    pg_pool: &sqlx::PgPool,
    pipeline: &crate::pipeline::PipelineConfig,
    card_id: &str,
    new_status: &str,
) {
    let is_terminal = pipeline.is_terminal(new_status);
    let is_review_enter = pipeline
        .hooks_for_state(new_status)
        .is_some_and(|hooks| hooks.on_enter.iter().any(|name| name == "OnReviewEnter"));

    if !is_terminal && !is_review_enter {
        return;
    }

    let Some((repo_id, issue_number)) = github_sync_target_for_card_pg(pg_pool, card_id).await
    else {
        return;
    };

    if is_terminal {
        if let Err(error) = crate::github::close_issue(&repo_id, issue_number) {
            tracing::warn!(
                "[kanban] failed to close issue {repo_id}#{issue_number} for terminal card {card_id}: {error}"
            );
        }
    } else if is_review_enter {
        let comment = "🔍 칸반 상태: **review** (카운터모델 리뷰 진행 중)";
        let _ = crate::github::comment_issue(&repo_id, issue_number, comment);
    }
}

async fn github_sync_target_for_card_pg(
    pg_pool: &sqlx::PgPool,
    card_id: &str,
) -> Option<(String, i64)> {
    let row = sqlx::query(
        "SELECT
            COALESCE(repo_id, '') AS repo_id,
            COALESCE(github_issue_url, '') AS github_issue_url,
            github_issue_number::BIGINT AS github_issue_number
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pg_pool)
    .await
    .ok()??;

    let repo_id: String = row.try_get("repo_id").ok()?;
    let issue_url: String = row.try_get("github_issue_url").ok()?;
    let issue_number: Option<i64> = row.try_get("github_issue_number").ok()?;
    if repo_id.is_empty() || issue_url.is_empty() {
        return None;
    }

    let issue_repo = issue_url
        .strip_prefix("https://github.com/")
        .and_then(|value| value.find("/issues/").map(|index| &value[..index]))?;
    if issue_repo != repo_id {
        tracing::warn!(
            "[kanban] skip GitHub sync for card {card_id}: issue URL repo {issue_repo} does not match card repo_id {repo_id}"
        );
        return None;
    }

    let repo_registered = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
            SELECT 1
            FROM github_repos
            WHERE id = $1
              AND COALESCE(sync_enabled, TRUE) = TRUE
         )",
    )
    .bind(&repo_id)
    .fetch_one(pg_pool)
    .await
    .ok()
    .unwrap_or(false);
    if !repo_registered {
        tracing::warn!(
            "[kanban] skip GitHub sync for card {card_id}: repo_id {repo_id} is not a registered sync-enabled repo"
        );
        return None;
    }

    issue_number.map(|number| (repo_id, number))
}

const TERMINAL_DISPATCH_CLEANUP_REASON: &str = "auto_cancelled_on_terminal_card";

fn sync_terminal_card_state(db: &Db, card_id: &str) {
    sync_terminal_card_state_with_scope(db, card_id, true);
}

fn sync_terminal_transition_followups(db: &Db, card_id: &str) {
    sync_terminal_card_state_with_scope(db, card_id, false);
}

fn sync_terminal_card_state_with_scope(db: &Db, card_id: &str, cancel_implementation: bool) {
    #[cfg(not(test))]
    {
        let _ = (db, card_id, cancel_implementation);
        return;
    }

    #[cfg(test)]
    {
        let Ok(conn) = db.lock() else {
            return;
        };

        let dispatch_types = if cancel_implementation {
            "'implementation', 'review-decision', 'rework'"
        } else {
            "'review-decision', 'rework'"
        };

        let pending_followups: Vec<String> = conn
            .prepare(&format!(
                "SELECT id FROM task_dispatches \
             WHERE kanban_card_id = ?1 AND dispatch_type IN ({dispatch_types}) \
             AND status IN ('pending', 'dispatched')"
            ))
            .ok()
            .and_then(|mut stmt| {
                stmt.query_map([card_id], |row| row.get::<_, String>(0))
                    .ok()
                    .map(|rows| rows.filter_map(|row| row.ok()).collect())
            })
            .unwrap_or_default();

        let mut cancelled = 0usize;
        for dispatch_id in pending_followups {
            cancelled += crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(
                &conn,
                &dispatch_id,
                Some(TERMINAL_DISPATCH_CLEANUP_REASON),
            )
            .unwrap_or(0);
        }

        if cancelled > 0 {
            tracing::info!(
                "[kanban] Cancelled {} pending terminal follow-up dispatch(es) for card {}",
                cancelled,
                card_id
            );
        }
    }
}

/// Drain deferred side-effects produced while hooks were executing.
///
/// Hooks cannot re-enter the engine, so transition requests and dispatch
/// creations are accumulated for post-hook replay.
pub fn drain_hook_side_effects(db: &Db, engine: &PolicyEngine) {
    drain_hook_side_effects_with_backends(Some(db), engine);
}

pub fn drain_hook_side_effects_with_backends(db: Option<&Db>, engine: &PolicyEngine) {
    loop {
        let intent_result = engine.drain_pending_intents();
        let mut transitions = intent_result.transitions;
        transitions.extend(engine.drain_pending_transitions());

        if transitions.is_empty() {
            break;
        }

        for (card_id, old_status, new_status) in &transitions {
            fire_transition_hooks_with_backends(
                db,
                engine.pg_pool(),
                engine,
                card_id,
                old_status,
                new_status,
            );
        }
    }
}

/// Fire pipeline-defined event hooks for a lifecycle event (#134).
///
/// Looks up the `events` section of the effective pipeline and fires each
/// hook name via `try_fire_hook_by_name`. Falls back to firing the default
/// hook name if no pipeline config or no event binding is found.
pub fn fire_event_hooks(
    db: &Db,
    engine: &PolicyEngine,
    event: &str,
    default_hook: &str,
    payload: serde_json::Value,
) {
    fire_event_hooks_with_backends(Some(db), engine, event, default_hook, payload);
}

pub fn fire_event_hooks_with_backends(
    db: Option<&Db>,
    engine: &PolicyEngine,
    event: &str,
    default_hook: &str,
    payload: serde_json::Value,
) {
    crate::pipeline::ensure_loaded();
    let hooks: Vec<String> = crate::pipeline::try_get()
        .and_then(|p| p.event_hooks(event).cloned())
        .unwrap_or_else(|| vec![default_hook.to_string()]);
    for hook_name in &hooks {
        let _ = engine.try_fire_hook_by_name(hook_name, payload.clone());
    }
    // Event hook callers already own transition draining; only materialize
    // deferred dispatch intents here so follow-up notification queries can see them.
    let _ = db;
    let _ = engine.drain_pending_intents();
}

/// Fire only the pipeline-defined on_enter/on_exit hooks for a transition.
///
/// Unlike `fire_transition_hooks`, this does NOT perform side-effects
/// (audit log, GitHub sync, terminal-state sync, dispatch notifications).
/// Use this when callers already handle those concerns separately
/// (e.g. dispatch creation, route handlers).
fn resolve_effective_pipeline_for_hooks(
    db: Option<&Db>,
    pg_pool: Option<&sqlx::PgPool>,
    card_id: &str,
) -> Option<crate::pipeline::PipelineConfig> {
    crate::pipeline::ensure_loaded();

    if let Some(pg_pool) = pg_pool {
        let card_id_owned = card_id.to_string();
        return match crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |bridge_pool| async move {
                let row = sqlx::query(
                    "SELECT repo_id, assigned_agent_id
                     FROM kanban_cards
                     WHERE id = $1",
                )
                .bind(&card_id_owned)
                .fetch_optional(&bridge_pool)
                .await
                .map_err(|error| {
                    format!("load postgres hook card context {card_id_owned}: {error}")
                })?;

                let (repo_id, agent_id) = if let Some(row) = row {
                    (
                        row.try_get::<Option<String>, _>("repo_id")
                            .map_err(|error| {
                                format!("decode postgres repo_id for {card_id_owned}: {error}")
                            })?,
                        row.try_get::<Option<String>, _>("assigned_agent_id")
                            .map_err(|error| {
                                format!(
                                    "decode postgres assigned_agent_id for {card_id_owned}: {error}"
                                )
                            })?,
                    )
                } else {
                    (None, None)
                };

                Ok(Some(
                    crate::pipeline::resolve_for_card_pg(
                        &bridge_pool,
                        repo_id.as_deref(),
                        agent_id.as_deref(),
                    )
                    .await,
                ))
            },
            |error| error,
        ) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!("failed to resolve postgres hook pipeline for {card_id}: {error}");
                None
            }
        };
    }

    #[cfg(test)]
    {
        let Some(db) = db else {
            return None;
        };

        db.lock().ok().map(|conn| {
            let repo_id: Option<String> = conn
                .query_row(
                    "SELECT repo_id FROM kanban_cards WHERE id = ?1",
                    [card_id],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            let agent_id: Option<String> = conn
                .query_row(
                    "SELECT assigned_agent_id FROM kanban_cards WHERE id = ?1",
                    [card_id],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), agent_id.as_deref())
        })
    }
    #[cfg(not(test))]
    {
        let _ = db;
        None
    }
}

pub fn fire_state_hooks(db: &Db, engine: &PolicyEngine, card_id: &str, from: &str, to: &str) {
    fire_state_hooks_with_backends(Some(db), engine, card_id, from, to);
}

pub fn fire_state_hooks_with_backends(
    db: Option<&Db>,
    engine: &PolicyEngine,
    card_id: &str,
    from: &str,
    to: &str,
) {
    if from == to {
        return;
    }
    let effective = resolve_effective_pipeline_for_hooks(db, engine.pg_pool(), card_id);
    if let Some(ref pipeline) = effective {
        fire_dynamic_hooks(engine, pipeline, card_id, from, to, None);
    }
    drain_hook_side_effects_with_backends(db, engine);
}

/// Fire only the on_enter hooks for a specific state, without requiring a transition.
///
/// Used when re-entering the same state (e.g., restarting review from awaiting_dod)
/// where `fire_state_hooks` would no-op because from == to.
pub fn fire_enter_hooks(db: &Db, engine: &PolicyEngine, card_id: &str, state: &str) {
    fire_enter_hooks_with_backends(Some(db), engine, card_id, state);
}

pub fn fire_enter_hooks_with_backends(
    db: Option<&Db>,
    engine: &PolicyEngine,
    card_id: &str,
    state: &str,
) {
    let effective = resolve_effective_pipeline_for_hooks(db, engine.pg_pool(), card_id);
    if let Some(ref pipeline) = effective {
        if let Some(bindings) = pipeline.hooks_for_state(state) {
            let payload = json!({
                "card_id": card_id,
                "from": state,
                "to": state,
                "status": state,
            });
            for hook_name in &bindings.on_enter {
                let _ = engine.try_fire_hook_by_name(hook_name, payload.clone());
            }
        }
    }
    drain_hook_side_effects_with_backends(db, engine);
}

/// Fire hooks for a status transition that already happened in the DB.
/// Use this when the DB UPDATE was done elsewhere (e.g., update_card with mixed fields).
pub fn fire_transition_hooks(db: &Db, engine: &PolicyEngine, card_id: &str, from: &str, to: &str) {
    fire_transition_hooks_with_backends(Some(db), engine.pg_pool(), engine, card_id, from, to);
}

pub fn fire_transition_hooks_with_backends(
    db: Option<&Db>,
    pg_pool: Option<&sqlx::PgPool>,
    engine: &PolicyEngine,
    card_id: &str,
    from: &str,
    to: &str,
) {
    if from == to {
        return;
    }

    if let Some(pg_pool) = pg_pool {
        fire_transition_hooks_pg(db, pg_pool, engine, card_id, from, to);
        return;
    }

    #[cfg(not(test))]
    {
        let _ = (db, engine, card_id, from, to);
        return;
    }

    #[cfg(test)]
    {
        let Some(db) = db else {
            return;
        };

        // Audit log
        if let Ok(conn) = db.lock() {
            log_audit(&conn, card_id, from, to, "hook", "OK");
        }

        // Resolve effective pipeline for this card (#135)
        crate::pipeline::ensure_loaded();
        let effective = db.lock().ok().map(|conn| {
            let repo_id: Option<String> = conn
                .query_row(
                    "SELECT repo_id FROM kanban_cards WHERE id = ?1",
                    [card_id],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            let agent_id: Option<String> = conn
                .query_row(
                    "SELECT assigned_agent_id FROM kanban_cards WHERE id = ?1",
                    [card_id],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), agent_id.as_deref())
        });

        if let Some(ref pipeline) = effective {
            // Sync auto_queue_entries + GitHub on terminal status
            if pipeline.is_terminal(to) {
                sync_terminal_transition_followups(db, card_id);
            }

            github_sync_on_transition(db, pipeline, card_id, to);
            fire_dynamic_hooks(engine, pipeline, card_id, from, to, Some("hook"));

            // #119: Record true_negative for cards that passed review and reached terminal state
            if pipeline.is_terminal(to)
                && record_true_negative_if_pass(db, engine.pg_pool(), card_id)
            {
                crate::server::routes::review_verdict::spawn_aggregate_if_needed_with_pg(
                    engine.pg_pool().cloned(),
                );
            }
        }

        drain_hook_side_effects(db, engine);
    }

    fn fire_transition_hooks_pg(
        db: Option<&Db>,
        pg_pool: &sqlx::PgPool,
        engine: &PolicyEngine,
        card_id: &str,
        from: &str,
        to: &str,
    ) {
        let card_id_owned = card_id.to_string();
        let from_owned = from.to_string();
        let to_owned = to.to_string();
        let effective = match crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |bridge_pool| async move {
                sqlx::query(
                "INSERT INTO kanban_audit_logs (card_id, from_status, to_status, source, result)
                 VALUES ($1, $2, $3, 'hook', 'OK')",
            )
            .bind(&card_id_owned)
            .bind(&from_owned)
            .bind(&to_owned)
            .execute(&bridge_pool)
            .await
            .map_err(|error| {
                format!("insert postgres kanban audit for {card_id_owned}: {error}")
            })?;
                sqlx::query(
                    "INSERT INTO audit_logs (entity_type, entity_id, action, actor)
                 VALUES ('kanban_card', $1, $2, 'hook')",
                )
                .bind(&card_id_owned)
                .bind(format!("{from_owned}->{to_owned} (OK)"))
                .execute(&bridge_pool)
                .await
                .map_err(|error| {
                    format!("insert postgres audit log for {card_id_owned}: {error}")
                })?;

                crate::pipeline::ensure_loaded();
                let row = sqlx::query(
                    "SELECT repo_id, assigned_agent_id
                 FROM kanban_cards
                 WHERE id = $1",
                )
                .bind(&card_id_owned)
                .fetch_optional(&bridge_pool)
                .await
                .map_err(|error| {
                    format!("load postgres card transition context {card_id_owned}: {error}")
                })?;
                let (repo_id, agent_id) = if let Some(row) = row {
                    (
                        row.try_get::<Option<String>, _>("repo_id")
                            .map_err(|error| {
                                format!("decode postgres repo_id for {card_id_owned}: {error}")
                            })?,
                        row.try_get::<Option<String>, _>("assigned_agent_id")
                            .map_err(|error| {
                                format!(
                                    "decode postgres assigned_agent_id for {card_id_owned}: {error}"
                                )
                            })?,
                    )
                } else {
                    (None, None)
                };
                Ok(Some(
                    crate::pipeline::resolve_for_card_pg(
                        &bridge_pool,
                        repo_id.as_deref(),
                        agent_id.as_deref(),
                    )
                    .await,
                ))
            },
            |error| error,
        ) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!("failed to fire postgres transition hooks for {card_id}: {error}");
                None
            }
        };

        if let Some(ref pipeline) = effective {
            if pipeline.is_terminal(to) {
                let card_id_owned = card_id.to_string();
                let terminal_followup = crate::utils::async_bridge::block_on_pg_result(
                    pg_pool,
                    move |bridge_pool| async move {
                        let mut tx = bridge_pool.begin().await.map_err(|error| {
                            format!("begin postgres terminal follow-up tx: {error}")
                        })?;
                        crate::github::sync::sync_auto_queue_terminal_on_pg(
                            &mut tx,
                            &card_id_owned,
                        )
                        .await
                        .map_err(|error| format!("{error}"))?;
                        let dispatch_ids = sqlx::query_scalar::<_, String>(
                        "SELECT id
                         FROM task_dispatches
                         WHERE kanban_card_id = $1
                           AND dispatch_type IN ('review-decision', 'rework')
                           AND status IN ('pending', 'dispatched')",
                    )
                    .bind(&card_id_owned)
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(|error| {
                        format!(
                            "load postgres terminal follow-up dispatches {card_id_owned}: {error}"
                        )
                    })?;
                        for dispatch_id in dispatch_ids {
                            crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg_tx(
                                &mut tx,
                                &dispatch_id,
                                Some(TERMINAL_DISPATCH_CLEANUP_REASON),
                            )
                            .await
                            .map_err(|error| format!("{error}"))?;
                        }
                        tx.commit().await.map_err(|error| {
                            format!("commit postgres terminal follow-up tx: {error}")
                        })?;
                        Ok(())
                    },
                    |error| error,
                );
                if let Err(error) = terminal_followup {
                    tracing::warn!(
                        "[kanban] failed postgres terminal follow-up sync for {}: {}",
                        card_id,
                        error
                    );
                }
            }

            let pg_pool_owned = pg_pool.clone();
            let pipeline_owned = pipeline.clone();
            let card_id_owned = card_id.to_string();
            let to_owned = to.to_string();
            let _ = crate::utils::async_bridge::block_on_pg_result(
                pg_pool,
                move |_bridge_pool| async move {
                    github_sync_on_transition_pg(
                        &pg_pool_owned,
                        &pipeline_owned,
                        &card_id_owned,
                        &to_owned,
                    )
                    .await;
                    Ok(())
                },
                |_error| (),
            );
            fire_dynamic_hooks(engine, pipeline, card_id, from, to, Some("hook"));

            if pipeline.is_terminal(to)
                && record_true_negative_if_pass_with_backends(db, Some(pg_pool), card_id)
            {
                crate::server::routes::review_verdict::spawn_aggregate_if_needed_with_pg(Some(
                    pg_pool.clone(),
                ));
            }
        }
    }
}

/// Sync GitHub issue state when kanban card transitions (pipeline-driven).
/// Terminal states → close issue. States with OnReviewEnter hook → comment.
#[cfg(test)]
fn github_sync_on_transition(
    db: &Db,
    pipeline: &crate::pipeline::PipelineConfig,
    card_id: &str,
    new_status: &str,
) {
    let is_terminal = pipeline.is_terminal(new_status);
    let is_review_enter = pipeline
        .hooks_for_state(new_status)
        .map_or(false, |h| h.on_enter.iter().any(|n| n == "OnReviewEnter"));

    if !is_terminal && !is_review_enter {
        return;
    }

    let Some((repo_id, num)) = github_sync_target_for_card(db, card_id) else {
        return;
    };

    if is_terminal {
        if let Err(error) = crate::github::close_issue(&repo_id, num) {
            tracing::warn!(
                "[kanban] failed to close issue {repo_id}#{num} for terminal card {card_id}: {error}"
            );
        }
    } else if is_review_enter {
        let comment = "🔍 칸반 상태: **review** (카운터모델 리뷰 진행 중)";
        let _ = crate::github::comment_issue(&repo_id, num, comment);
    }
}

#[cfg(test)]
fn github_sync_target_for_card(db: &Db, card_id: &str) -> Option<(String, i64)> {
    let info: Option<(String, String, Option<i64>)> = db
        .lock()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT COALESCE(repo_id, ''), COALESCE(github_issue_url, ''), github_issue_number FROM kanban_cards WHERE id = ?1",
                [card_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok()
        });

    let Some((repo_id, issue_url, issue_number)) = info else {
        return None;
    };
    if repo_id.is_empty() || issue_url.is_empty() {
        return None;
    }

    let issue_repo = match issue_url
        .strip_prefix("https://github.com/")
        .and_then(|s| s.find("/issues/").map(|i| &s[..i]))
    {
        Some(r) => r,
        None => return None,
    };
    if issue_repo != repo_id {
        tracing::warn!(
            "[kanban] skip GitHub sync for card {card_id}: issue URL repo {issue_repo} does not match card repo_id {repo_id}"
        );
        return None;
    }

    let repo_registered = db
        .lock()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM github_repos WHERE id = ?1 AND COALESCE(sync_enabled, 1) = 1)",
                [&repo_id],
                |row| row.get::<_, bool>(0),
            )
            .ok()
        })
        .unwrap_or(false);
    if !repo_registered {
        tracing::warn!(
            "[kanban] skip GitHub sync for card {card_id}: repo_id {repo_id} is not a registered sync-enabled repo"
        );
        return None;
    }

    issue_number.map(|num| (repo_id, num))
}

/// Log a kanban state transition to audit_logs table.
#[cfg(test)]
fn log_audit(
    conn: &rusqlite::Connection,
    card_id: &str,
    from: &str,
    to: &str,
    source: &str,
    result: &str,
) {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS kanban_audit_logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            card_id TEXT,
            from_status TEXT,
            to_status TEXT,
            source TEXT,
            result TEXT,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .ok();
    conn.execute(
        "INSERT INTO kanban_audit_logs (card_id, from_status, to_status, source, result) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![card_id, from, to, source, result],
    )
    .ok();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS audit_logs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            entity_type TEXT,
            entity_id   TEXT,
            action      TEXT,
            timestamp   DATETIME DEFAULT CURRENT_TIMESTAMP,
            actor       TEXT
        )",
    )
    .ok();
    conn.execute(
        "INSERT INTO audit_logs (entity_type, entity_id, action, actor)
         VALUES ('kanban_card', ?1, ?2, ?3)",
        rusqlite::params![card_id, format!("{from}->{to} ({result})"), source],
    )
    .ok();
}

/// #119: When a card reaches done after a review pass verdict, record a true_negative
/// tuning outcome. This confirms the review was correct in not finding issues.
/// Returns true if a TN was actually inserted.
fn record_true_negative_if_pass(db: &Db, pg_pool: Option<&sqlx::PgPool>, card_id: &str) -> bool {
    record_true_negative_if_pass_with_backends(Some(db), pg_pool, card_id)
}

fn record_true_negative_if_pass_with_backends(
    db: Option<&Db>,
    pg_pool: Option<&sqlx::PgPool>,
    card_id: &str,
) -> bool {
    if let Some(pool) = pg_pool {
        let card_id = card_id.to_string();
        return crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |pool| async move {
                let last_verdict = sqlx::query_scalar::<_, Option<String>>(
                    "SELECT last_verdict
                     FROM card_review_state
                     WHERE card_id = $1",
                )
                .bind(&card_id)
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load postgres review verdict for {card_id}: {error}"))?
                .flatten();

                let Some(last_verdict) = last_verdict else {
                    return Ok(false);
                };
                if !matches!(last_verdict.as_str(), "pass" | "approved") {
                    return Ok(false);
                }

                // `card_review_state.review_round` is BIGINT (0008_int4_to_bigint_audit.sql).
                // Decoding as `i32` raises `ColumnDecode: mismatched types`, which silently
                // aborted this whole true_negative recording path.
                let review_round = sqlx::query_scalar::<_, Option<i64>>(
                    "SELECT review_round
                     FROM card_review_state
                     WHERE card_id = $1",
                )
                .bind(&card_id)
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load postgres review round for {card_id}: {error}"))?
                .flatten();
                // `review_tuning_outcomes.review_round` is still INTEGER (not in the
                // 0008 bigint audit). Downcast is safe — review rounds are bounded small.
                let review_round_i32 = review_round.map(|v| v as i32);

                let review_results = sqlx::query(
                    "SELECT result
                     FROM task_dispatches
                     WHERE kanban_card_id = $1
                       AND dispatch_type = 'review'
                       AND status = 'completed'
                     ORDER BY COALESCE(completed_at, updated_at, created_at) DESC, id DESC",
                )
                .bind(&card_id)
                .fetch_all(&pool)
                .await
                .map_err(|error| format!("load postgres review dispatches for {card_id}: {error}"))?;

                let finding_cats = review_results.into_iter().find_map(|row| {
                    row.try_get::<Option<String>, _>("result")
                        .ok()
                        .flatten()
                        .and_then(|result_str| serde_json::from_str::<serde_json::Value>(&result_str).ok())
                        .and_then(|value| {
                            value["items"].as_array().and_then(|items| {
                                let cats: Vec<String> = items
                                    .iter()
                                    .filter_map(|item| item["category"].as_str().map(str::to_string))
                                    .collect();
                                if cats.is_empty() {
                                    None
                                } else {
                                    serde_json::to_string(&cats).ok()
                                }
                            })
                        })
                });

                let inserted = sqlx::query(
                    "INSERT INTO review_tuning_outcomes (
                        card_id, dispatch_id, review_round, verdict, decision, outcome, finding_categories
                     )
                     VALUES ($1, NULL, $2, $3, 'done', 'true_negative', $4)",
                )
                .bind(&card_id)
                .bind(review_round_i32)
                .bind(&last_verdict)
                .bind(finding_cats)
                .execute(&pool)
                .await
                .map(|result| result.rows_affected() > 0)
                .map_err(|error| {
                    format!("insert postgres true_negative review tuning for {card_id}: {error}")
                })?;

                if inserted {
                    tracing::info!(
                        "[review-tuning] #119 recorded true_negative: card={card_id} (pass → done)"
                    );
                }
                Ok(inserted)
            },
            |error| error,
        )
        .unwrap_or(false);
    }

    #[cfg(test)]
    {
        if let Some(db) = db
            && let Ok(conn) = db.lock()
        {
            // Check if the card's last review verdict was "pass" or "approved"
            let last_verdict: Option<String> = conn
                .query_row(
                    "SELECT last_verdict FROM card_review_state WHERE card_id = ?1",
                    [card_id],
                    |row| row.get(0),
                )
                .ok()
                .flatten();

            match last_verdict.as_deref() {
                Some("pass") | Some("approved") => {
                    let review_round: Option<i64> = conn
                        .query_row(
                            "SELECT review_round FROM card_review_state WHERE card_id = ?1",
                            [card_id],
                            |row| row.get(0),
                        )
                        .ok();

                    // Carry forward finding_categories from the review dispatch that found issues.
                    // The most recent review dispatch is typically the pass/approved one with
                    // empty items, so we walk backwards to find one with actual findings.
                    // This ensures that if TN is later corrected to FN on reopen, categories
                    // are already present.
                    let finding_cats: Option<String> = conn
                        .prepare(
                            "SELECT td.result FROM task_dispatches td \
                         WHERE td.kanban_card_id = ?1 AND td.dispatch_type = 'review' \
                         AND td.status = 'completed' ORDER BY td.rowid DESC",
                        )
                        .ok()
                        .and_then(|mut stmt| {
                            let rows = stmt
                                .query_map([card_id], |row| row.get::<_, Option<String>>(0))
                                .ok()?;
                            for row_result in rows {
                                if let Ok(Some(result_str)) = row_result {
                                    if let Ok(v) =
                                        serde_json::from_str::<serde_json::Value>(&result_str)
                                    {
                                        if let Some(items) = v["items"].as_array() {
                                            let cats: Vec<String> = items
                                                .iter()
                                                .filter_map(|it| {
                                                    it["category"].as_str().map(|s| s.to_string())
                                                })
                                                .collect();
                                            if !cats.is_empty() {
                                                return serde_json::to_string(&cats).ok();
                                            }
                                        }
                                    }
                                }
                            }
                            None
                        });

                    let inserted = conn.execute(
                    "INSERT INTO review_tuning_outcomes \
                     (card_id, dispatch_id, review_round, verdict, decision, outcome, finding_categories) \
                     VALUES (?1, NULL, ?2, ?3, 'done', 'true_negative', ?4)",
                    rusqlite::params![card_id, review_round, last_verdict.as_deref().unwrap_or("pass"), finding_cats],
                )
                .map(|n| n > 0)
                .unwrap_or(false);
                    if inserted {
                        tracing::info!(
                            "[review-tuning] #119 recorded true_negative: card={card_id} (pass → done)"
                        );
                    }
                    return inserted;
                }
                _ => {} // No review or non-pass verdict — nothing to record
            }
        }
    }
    false
}

/// #119: When a card is reopened after reaching done with a pass verdict,
/// correct any true_negative outcomes to false_negative — the review missed a real bug.
///
/// Also backfills finding_categories if the TN record had empty categories.
/// TN is typically recorded using categories from the last completed review dispatch,
/// which is the pass/approved dispatch with empty items. On reopen we look for the
/// most recent review dispatch that actually reported findings (non-empty items array)
/// to carry those categories forward into the FN record.
pub fn correct_tn_to_fn_on_reopen(db: Option<&Db>, pg_pool: Option<&sqlx::PgPool>, card_id: &str) {
    if let Some(pool) = pg_pool {
        let card_id = card_id.to_string();
        let log_card_id = card_id.clone();
        let updated = crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |pool| async move {
                let updated = sqlx::query(
                    "UPDATE review_tuning_outcomes
                     SET outcome = 'false_negative'
                     WHERE card_id = $1
                       AND outcome = 'true_negative'
                       AND review_round = (
                           SELECT MAX(review_round)
                           FROM review_tuning_outcomes
                           WHERE card_id = $1
                             AND outcome = 'true_negative'
                       )",
                )
                .bind(&card_id)
                .execute(&pool)
                .await
                .map_err(|error| format!("correct postgres TN->FN for {card_id}: {error}"))?
                .rows_affected();
                if updated == 0 {
                    return Ok(0_u64);
                }

                let needs_backfill = sqlx::query_scalar::<_, bool>(
                    "SELECT COALESCE(
                         finding_categories IS NULL
                         OR finding_categories = ''
                         OR finding_categories = '[]',
                         false
                     )
                     FROM review_tuning_outcomes
                     WHERE card_id = $1
                       AND outcome = 'false_negative'
                     ORDER BY id DESC
                     LIMIT 1",
                )
                .bind(&card_id)
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load postgres FN backfill flag for {card_id}: {error}"))?
                .unwrap_or(false);

                if needs_backfill {
                    let review_results = sqlx::query(
                        "SELECT result
                         FROM task_dispatches
                         WHERE kanban_card_id = $1
                           AND dispatch_type = 'review'
                           AND status = 'completed'
                         ORDER BY COALESCE(completed_at, updated_at, created_at) DESC, id DESC",
                    )
                    .bind(&card_id)
                    .fetch_all(&pool)
                    .await
                    .map_err(|error| format!("load postgres review dispatches for {card_id}: {error}"))?;

                    let finding_cats = review_results.into_iter().find_map(|row| {
                        row.try_get::<Option<String>, _>("result")
                            .ok()
                            .flatten()
                            .and_then(|result_str| serde_json::from_str::<serde_json::Value>(&result_str).ok())
                            .and_then(|value| {
                                value["items"].as_array().and_then(|items| {
                                    if items.is_empty() {
                                        return None;
                                    }
                                    let cats: Vec<String> = items
                                        .iter()
                                        .filter_map(|item| item["category"].as_str().map(str::to_string))
                                        .collect();
                                    if cats.is_empty() {
                                        None
                                    } else {
                                        serde_json::to_string(&cats).ok()
                                    }
                                })
                            })
                    });

                    if let Some(cats) = finding_cats {
                        let backfilled = sqlx::query(
                            "UPDATE review_tuning_outcomes
                             SET finding_categories = $1
                             WHERE card_id = $2
                               AND outcome = 'false_negative'
                               AND (finding_categories IS NULL OR finding_categories = '' OR finding_categories = '[]')",
                        )
                        .bind(&cats)
                        .bind(&card_id)
                        .execute(&pool)
                        .await
                        .map_err(|error| {
                            format!("backfill postgres FN finding_categories for {card_id}: {error}")
                        })?
                        .rows_affected();
                        if backfilled > 0 {
                            tracing::info!(
                                "[review-tuning] #119 backfilled {backfilled} FN finding_categories: card={card_id} categories={cats}"
                            );
                        }
                    }
                }

                Ok(updated)
            },
            |error| error,
        )
        .unwrap_or(0);
        if updated > 0 {
            tracing::info!(
                "[review-tuning] #119 corrected {updated} true_negative → false_negative: card={log_card_id} (reopen, latest round only)"
            );
        }
        return;
    }

    #[cfg(test)]
    {
        let Some(db) = db else {
            return;
        };

        if let Ok(conn) = db.lock() {
            // Only correct the most recent TN (latest review_round) to avoid
            // corrupting historical TN records from earlier rounds
            let updated = conn
            .execute(
                "UPDATE review_tuning_outcomes SET outcome = 'false_negative' \
                 WHERE card_id = ?1 AND outcome = 'true_negative' \
                 AND review_round = (SELECT MAX(review_round) FROM review_tuning_outcomes WHERE card_id = ?1 AND outcome = 'true_negative')",
                [card_id],
            )
            .unwrap_or(0);
            if updated > 0 {
                tracing::info!(
                    "[review-tuning] #119 corrected {updated} true_negative → false_negative: card={card_id} (reopen, latest round only)"
                );

                // Backfill finding_categories if empty. The TN was recorded using the
                // last review dispatch (the pass/approved one with empty items). Look
                // for an earlier review dispatch that actually found issues.
                let needs_backfill: bool = conn
                .query_row(
                    "SELECT finding_categories IS NULL OR finding_categories = '' OR finding_categories = '[]' \
                     FROM review_tuning_outcomes \
                     WHERE card_id = ?1 AND outcome = 'false_negative' \
                     ORDER BY rowid DESC LIMIT 1",
                    [card_id],
                    |row| row.get(0),
                )
                .unwrap_or(false);

                if needs_backfill {
                    // Walk through review dispatches (most recent first) to find
                    // one with a non-empty items array containing categories
                    let finding_cats: Option<String> = conn
                        .prepare(
                            "SELECT td.result FROM task_dispatches td \
                         WHERE td.kanban_card_id = ?1 AND td.dispatch_type = 'review' \
                         AND td.status = 'completed' \
                         ORDER BY td.rowid DESC",
                        )
                        .ok()
                        .and_then(|mut stmt| {
                            let rows = stmt
                                .query_map([card_id], |row| row.get::<_, Option<String>>(0))
                                .ok()?;
                            for row_result in rows {
                                if let Ok(Some(result_str)) = row_result {
                                    if let Ok(v) =
                                        serde_json::from_str::<serde_json::Value>(&result_str)
                                    {
                                        if let Some(items) = v["items"].as_array() {
                                            if !items.is_empty() {
                                                let cats: Vec<String> = items
                                                    .iter()
                                                    .filter_map(|it| {
                                                        it["category"]
                                                            .as_str()
                                                            .map(|s| s.to_string())
                                                    })
                                                    .collect();
                                                if !cats.is_empty() {
                                                    return serde_json::to_string(&cats).ok();
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            None
                        });

                    if let Some(ref cats) = finding_cats {
                        let backfilled = conn
                        .execute(
                            "UPDATE review_tuning_outcomes SET finding_categories = ?1 \
                             WHERE card_id = ?2 AND outcome = 'false_negative' \
                             AND (finding_categories IS NULL OR finding_categories = '' OR finding_categories = '[]')",
                            rusqlite::params![cats, card_id],
                        )
                        .unwrap_or(0);
                        if backfilled > 0 {
                            tracing::info!(
                                "[review-tuning] #119 backfilled {backfilled} FN finding_categories: card={card_id} categories={cats}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
        config.policies.hot_reload = false;
        PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap()
    }

    fn test_engine_with_dir(db: &Db, dir: &std::path::Path) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = dir.to_path_buf();
        config.policies.hot_reload = false;
        PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap()
    }

    fn test_engine_with_pg(pg_pool: sqlx::PgPool) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
        config.policies.hot_reload = false;
        PolicyEngine::new_with_pg(&config, Some(pg_pool)).unwrap()
    }

    fn test_engine_with_pg_and_dir(pg_pool: sqlx::PgPool, dir: &std::path::Path) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = dir.to_path_buf();
        config.policies.hot_reload = false;
        PolicyEngine::new_with_pg(&config, Some(pg_pool)).unwrap()
    }

    struct KanbanPgDatabase {
        _lock: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
        cleanup_armed: bool,
    }

    impl KanbanPgDatabase {
        async fn create() -> Self {
            let lock = crate::db::postgres::lock_test_lifecycle();
            let admin_url = postgres_admin_database_url();
            let database_name = format!("agentdesk_kanban_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(&admin_url, &database_name, "kanban tests")
                .await
                .expect("create kanban postgres test db");

            Self {
                _lock: lock,
                admin_url,
                database_name,
                database_url,
                cleanup_armed: true,
            }
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(&self.database_url, "kanban tests")
                .await
                .expect("connect + migrate kanban postgres test db")
        }

        async fn drop(mut self) {
            let drop_result = crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "kanban tests",
            )
            .await;
            if drop_result.is_ok() {
                self.cleanup_armed = false;
            }
            drop_result.expect("drop kanban postgres test db");
        }
    }

    impl Drop for KanbanPgDatabase {
        fn drop(&mut self) {
            if !self.cleanup_armed {
                return;
            }
            cleanup_test_postgres_db_from_drop(self.admin_url.clone(), self.database_name.clone());
        }
    }

    fn cleanup_test_postgres_db_from_drop(admin_url: String, database_name: String) {
        let cleanup_database_name = database_name.clone();
        let thread_name = format!("kanban tests cleanup {cleanup_database_name}");
        let spawn_result = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        eprintln!(
                            "kanban tests cleanup runtime failed for {database_name}: {error}"
                        );
                        return;
                    }
                };
                if let Err(error) = runtime.block_on(crate::db::postgres::drop_test_database(
                    &admin_url,
                    &database_name,
                    "kanban tests",
                )) {
                    eprintln!("kanban tests cleanup failed for {database_name}: {error}");
                }
            });

        match spawn_result {
            Ok(handle) => {
                if handle.join().is_err() {
                    eprintln!("kanban tests cleanup thread panicked for {cleanup_database_name}");
                }
            }
            Err(error) => {
                eprintln!(
                    "kanban tests cleanup thread spawn failed for {cleanup_database_name}: {error}"
                );
            }
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    struct EnvVarGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) };
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[cfg(unix)]
    fn write_executable_script(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, contents).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    fn seed_card(db: &Db, card_id: &str, status: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        ).ok(); // ignore if already exists
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at)
             VALUES (?1, 'Test Card', ?2, 'agent-1', datetime('now'), datetime('now'))",
            rusqlite::params![card_id, status],
        ).unwrap();
    }

    fn seed_dispatch(db: &Db, card_id: &str, dispatch_status: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES (?1, ?2, 'agent-1', 'implementation', ?3, 'Test Dispatch', datetime('now'), datetime('now'))",
            rusqlite::params![format!("dispatch-{}-{}", card_id, dispatch_status), card_id, dispatch_status],
        ).unwrap();
    }

    fn seed_dispatch_with_type(
        db: &Db,
        dispatch_id: &str,
        card_id: &str,
        dispatch_type: &str,
        dispatch_status: &str,
    ) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES (?1, ?2, 'agent-1', ?3, ?4, 'Typed Dispatch', datetime('now'), datetime('now'))",
            rusqlite::params![dispatch_id, card_id, dispatch_type, dispatch_status],
        )
        .unwrap();
    }

    async fn seed_card_pg(pool: &sqlx::PgPool, card_id: &str, status: &str) {
        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
             VALUES ('agent-1', 'Agent 1', '123', '456')
             ON CONFLICT (id) DO NOTHING",
        )
        .execute(pool)
        .await
        .expect("seed postgres agent");
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at)
             VALUES ($1, 'Test Card', $2, 'agent-1', NOW(), NOW())",
        )
        .bind(card_id)
        .bind(status)
        .execute(pool)
        .await
        .expect("seed postgres card");
    }

    async fn seed_card_with_repo_pg(
        pool: &sqlx::PgPool,
        card_id: &str,
        status: &str,
        repo_id: &str,
    ) {
        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
             VALUES ('agent-1', 'Agent 1', '123', '456')
             ON CONFLICT (id) DO NOTHING",
        )
        .execute(pool)
        .await
        .expect("seed postgres agent");
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, repo_id, created_at, updated_at
             )
             VALUES ($1, 'Test Card', $2, 'agent-1', $3, NOW(), NOW())",
        )
        .bind(card_id)
        .bind(status)
        .bind(repo_id)
        .execute(pool)
        .await
        .expect("seed postgres card with repo");
    }

    async fn seed_dispatch_pg(pool: &sqlx::PgPool, card_id: &str, dispatch_status: &str) {
        seed_dispatch_with_type_pg(
            pool,
            &format!("dispatch-{}-{}", card_id, dispatch_status),
            card_id,
            "implementation",
            dispatch_status,
        )
        .await;
    }

    async fn seed_dispatch_with_type_pg(
        pool: &sqlx::PgPool,
        dispatch_id: &str,
        card_id: &str,
        dispatch_type: &str,
        dispatch_status: &str,
    ) {
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
             )
             VALUES ($1, $2, 'agent-1', $3, $4, 'Typed Dispatch', NOW(), NOW())",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .bind(dispatch_type)
        .bind(dispatch_status)
        .execute(pool)
        .await
        .expect("seed postgres dispatch");
    }

    #[tokio::test]
    async fn completed_dispatch_only_does_not_authorize_transition() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-completed", "requested").await;
        seed_dispatch_pg(&pool, "card-completed", "completed").await;

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-completed",
            "in_progress",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(
            result.is_err(),
            "completed dispatch should NOT authorize transition"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("active dispatch"),
            "error should mention active dispatch"
        );
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn pending_dispatch_authorizes_transition() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-pending", "requested").await;
        seed_dispatch_pg(&pool, "card-pending", "pending").await;

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-pending",
            "in_progress",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(
            result.is_ok(),
            "pending dispatch should authorize transition"
        );
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn dispatched_status_authorizes_transition() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-dispatched", "requested").await;
        seed_dispatch_pg(&pool, "card-dispatched", "dispatched").await;

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-dispatched",
            "in_progress",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(
            result.is_ok(),
            "dispatched status should authorize transition"
        );
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn no_dispatch_blocks_non_free_transition() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-none", "requested").await;
        // No dispatch at all

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-none",
            "in_progress",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(result.is_err(), "no dispatch should block transition");
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn free_transition_works_without_dispatch() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-free", "backlog").await;

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-free",
            "ready",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(
            result.is_ok(),
            "backlog → ready should work without dispatch"
        );
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn force_overrides_dispatch_check() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-force", "requested").await;
        // No dispatch, but force=true

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-force",
            "in_progress",
            "pmd",
            crate::engine::transition::ForceIntent::OperatorOverride,
        )
        .await;
        assert!(result.is_ok(), "force=true should bypass dispatch check");
        pg_db.drop().await;
    }

    #[test]
    fn sync_terminal_card_state_cancels_pending_implementation_dispatch() {
        let db = test_db();
        ensure_auto_queue_tables(&db);
        seed_card(&db, "card-terminal-sync", "done");
        seed_dispatch_with_type(
            &db,
            "dispatch-card-terminal-sync-pending",
            "card-terminal-sync",
            "implementation",
            "pending",
        );

        sync_terminal_card_state(&db, "card-terminal-sync");

        let status: String = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'dispatch-card-terminal-sync-pending'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "cancelled");
    }

    #[tokio::test]
    async fn stale_completed_review_verdict_does_not_open_current_done_gate() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-stale-review-pass", "review").await;

        sqlx::query(
            "UPDATE kanban_cards
             SET review_entered_at = NOW()
             WHERE id = 'card-stale-review-pass'",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, result,
                created_at, updated_at, completed_at
             ) VALUES (
                'review-stale-pass', 'card-stale-review-pass', 'agent-1', 'review', 'completed',
                'stale pass', $1::jsonb,
                NOW() - INTERVAL '30 minutes', NOW() - INTERVAL '30 minutes', NOW() - INTERVAL '30 minutes'
             )",
        )
        .bind(json!({"verdict": "pass"}).to_string())
        .execute(&pool)
        .await
        .unwrap();

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-stale-review-pass",
            "done",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(
            result.is_err(),
            "completed review verdicts from older rounds must not satisfy the current review_passed gate"
        );

        let status: String = sqlx::query_scalar(
            "SELECT status FROM kanban_cards WHERE id = 'card-stale-review-pass'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            status, "review",
            "stale review verdict must leave the card in review"
        );
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn legacy_review_without_review_entered_at_keeps_latest_pass_behavior() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-legacy-review-pass", "review").await;

        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, result,
                created_at, updated_at, completed_at
             ) VALUES (
                'review-legacy-pass', 'card-legacy-review-pass', 'agent-1', 'review', 'completed',
                'legacy pass', $1::jsonb,
                NOW() - INTERVAL '10 minutes', NOW() - INTERVAL '5 minutes', NOW() - INTERVAL '5 minutes'
             )",
        )
        .bind(json!({"verdict": "pass"}).to_string())
        .execute(&pool)
        .await
        .unwrap();

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-legacy-review-pass",
            "done",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(
            result.is_ok(),
            "cards without review_entered_at must preserve the legacy pass verdict behavior"
        );
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn transition_status_with_on_conn_rolls_back_on_cleanup_error() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-force-rollback", "requested").await;
        seed_dispatch_pg(&pool, "card-force-rollback", "pending").await;

        let result = transition_status_with_opts_and_allowed_cleanup_pg_only(
            &pool,
            &engine,
            "card-force-rollback",
            "in_progress",
            "pmd",
            crate::engine::transition::ForceIntent::OperatorOverride,
            AllowedOnConnMutation::TestOnlyRollbackGuard,
        )
        .await;
        assert!(result.is_err(), "cleanup failure must abort the transition");

        let status: String =
            sqlx::query_scalar("SELECT status FROM kanban_cards WHERE id = 'card-force-rollback'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            status, "requested",
            "cleanup failure must roll back the card status change"
        );
        pg_db.drop().await;
    }

    #[test]
    fn drain_hook_side_effects_materializes_tick_dispatch_intents() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("tick-dispatch.js"),
            r#"
            var policy = {
                name: "tick-dispatch",
                priority: 1,
                onTick30s: function() {
                    agentdesk.dispatch.create(
                        "card-tick",
                        "agent-1",
                        "rework",
                        "Tick Rework"
                    );
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let db = test_db();
        let engine = test_engine_with_dir(&db, dir.path());
        seed_card(&db, "card-tick", "requested");

        engine
            .try_fire_hook_by_name("onTick30s", json!({}))
            .unwrap();
        drain_hook_side_effects(&db, &engine);

        let conn = db.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-tick' AND dispatch_type = 'rework'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "tick hook dispatch intent should be persisted");
    }

    /// Regression test for #274: status transitions fire custom state hooks
    /// through try_fire_hook_by_name(), and dispatch.create() in that path must
    /// return with the dispatch row + notify outbox already materialized.
    #[tokio::test]
    async fn transition_status_custom_on_enter_hook_materializes_dispatch_outbox() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("ready-enter-hook.js"),
            r#"
            var policy = {
                name: "ready-enter-hook",
                priority: 1,
                onCustomReadyEnter: function(payload) {
                    agentdesk.dispatch.create(
                        payload.card_id,
                        "agent-1",
                        "implementation",
                        "Ready Hook Dispatch"
                    );
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg_and_dir(pool.clone(), dir.path());
        seed_card_pg(&pool, "card-ready-hook", "backlog").await;

        sqlx::query("UPDATE agents SET pipeline_config = $1::jsonb WHERE id = 'agent-1'")
            .bind(
                json!({
                    "hooks": {
                        "ready": {
                            "on_enter": ["onCustomReadyEnter"],
                            "on_exit": []
                        }
                    }
                })
                .to_string(),
            )
            .execute(&pool)
            .await
            .unwrap();

        transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-ready-hook",
            "ready",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await
        .unwrap();

        let (dispatch_id, title): (String, String) = sqlx::query_as(
            "SELECT id, title FROM task_dispatches WHERE kanban_card_id = 'card-ready-hook'",
        )
        .fetch_one(&pool)
        .await
        .expect("custom ready on_enter hook should create a dispatch");
        assert_eq!(title, "Ready Hook Dispatch");

        let notify_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM dispatch_outbox WHERE dispatch_id = $1 AND action = 'notify'",
        )
        .bind(&dispatch_id)
        .fetch_one(&pool)
        .await
        .expect("dispatch outbox query should succeed");
        assert_eq!(
            notify_count, 1,
            "custom transition hook dispatch must enqueue exactly one notify outbox row"
        );

        let (card_status, latest_dispatch_id): (String, String) = sqlx::query_as(
            "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-ready-hook'",
        )
        .fetch_one(&pool)
        .await
        .expect("card should be updated by dispatch.create()");
        assert_eq!(card_status, "in_progress");
        assert_eq!(latest_dispatch_id, dispatch_id);
        pg_db.drop().await;
    }

    /// Regression guard for the known-hook path: try_fire_hook_by_name() must
    /// return with dispatch.create() side-effects already visible, even without
    /// an extra drain_hook_side_effects() call at the caller.
    #[test]
    fn try_fire_hook_drains_dispatch_intents_without_explicit_drain() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("tick-intent.js"),
            r#"
            var policy = {
                name: "tick-intent",
                priority: 1,
                onTick1min: function() {
                    agentdesk.dispatch.create(
                        "card-intent-test",
                        "agent-1",
                        "implementation",
                        "Intent Drain Test"
                    );
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let db = test_db();
        let engine = test_engine_with_dir(&db, dir.path());
        seed_card(&db, "card-intent-test", "requested");

        // Fire tick hook — do NOT call drain_hook_side_effects afterwards.
        // The intent should still be drained by try_fire_hook's internal drain.
        engine
            .try_fire_hook_by_name("OnTick1min", json!({}))
            .unwrap();

        let conn = db.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-intent-test' AND dispatch_type = 'implementation'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "#202: tick hook dispatch intent must be persisted by try_fire_hook's internal drain"
        );
    }

    #[test]
    fn fire_transition_hooks_terminal_cleanup_cancels_review_followups_with_reason() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-terminal-cleanup", "review");
        seed_dispatch_with_type(
            &db,
            "dispatch-rd-cleanup",
            "card-terminal-cleanup",
            "review-decision",
            "pending",
        );
        seed_dispatch_with_type(
            &db,
            "dispatch-rw-cleanup",
            "card-terminal-cleanup",
            "rework",
            "dispatched",
        );
        seed_dispatch_with_type(
            &db,
            "dispatch-review-keep",
            "card-terminal-cleanup",
            "review",
            "pending",
        );

        fire_transition_hooks(&db, &engine, "card-terminal-cleanup", "review", "done");

        let conn = db.lock().unwrap();
        let (rd_status, rd_reason): (String, Option<String>) = conn
            .query_row(
                "SELECT status, json_extract(result, '$.reason') FROM task_dispatches WHERE id = 'dispatch-rd-cleanup'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let (rw_status, rw_reason): (String, Option<String>) = conn
            .query_row(
                "SELECT status, json_extract(result, '$.reason') FROM task_dispatches WHERE id = 'dispatch-rw-cleanup'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let review_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'dispatch-review-keep'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(rd_status, "cancelled");
        assert_eq!(rd_reason.as_deref(), Some(TERMINAL_DISPATCH_CLEANUP_REASON));
        assert_eq!(rw_status, "cancelled");
        assert_eq!(rw_reason.as_deref(), Some(TERMINAL_DISPATCH_CLEANUP_REASON));
        assert_eq!(
            review_status, "pending",
            "terminal cleanup must not cancel pending review dispatches"
        );
    }

    // ── Pipeline / auto-queue regression tests (#110) ──────────────

    /// Ensure auto_queue tables exist (created lazily by auto_queue routes, not main migration)
    fn ensure_auto_queue_tables(db: &Db) {
        let conn = db.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS auto_queue_runs (
                id          TEXT PRIMARY KEY,
                repo        TEXT,
                agent_id    TEXT,
                status      TEXT DEFAULT 'active',
                ai_model    TEXT,
                ai_rationale TEXT,
                timeout_minutes INTEGER DEFAULT 120,
                unified_thread INTEGER DEFAULT 0,
                unified_thread_id TEXT,
                unified_thread_channel_id TEXT,
                max_concurrent_threads INTEGER DEFAULT 1,
                thread_group_count INTEGER DEFAULT 1,
                created_at  DATETIME DEFAULT CURRENT_TIMESTAMP,
                completed_at DATETIME
            );
            CREATE TABLE IF NOT EXISTS auto_queue_entries (
                id              TEXT PRIMARY KEY,
                run_id          TEXT REFERENCES auto_queue_runs(id),
                kanban_card_id  TEXT REFERENCES kanban_cards(id),
                agent_id        TEXT,
                priority_rank   INTEGER DEFAULT 0,
                reason          TEXT,
                status          TEXT DEFAULT 'pending',
                dispatch_id     TEXT,
                thread_group    INTEGER DEFAULT 0,
                created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
                dispatched_at   DATETIME,
                completed_at    DATETIME
            );",
        )
        .unwrap();
    }

    fn seed_card_with_repo(db: &Db, card_id: &str, status: &str, repo_id: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        ).ok();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, repo_id, created_at, updated_at)
             VALUES (?1, 'Test Card', ?2, 'agent-1', ?3, datetime('now'), datetime('now'))",
            rusqlite::params![card_id, status, repo_id],
        ).unwrap();
    }

    /// Insert 2 pipeline stages (INTEGER AUTOINCREMENT id) and return their ids.
    fn seed_pipeline_stages(db: &Db, repo_id: &str) -> (i64, i64) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO pipeline_stages (repo_id, stage_name, stage_order, trigger_after)
             VALUES (?1, 'Build', 1, 'ready')",
            [repo_id],
        )
        .unwrap();
        let stage1 = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO pipeline_stages (repo_id, stage_name, stage_order, trigger_after)
             VALUES (?1, 'Deploy', 2, 'review_pass')",
            [repo_id],
        )
        .unwrap();
        let stage2 = conn.last_insert_rowid();
        (stage1, stage2)
    }

    fn seed_auto_queue_run(db: &Db, agent_id: &str) -> (String, String, String) {
        ensure_auto_queue_tables(db);
        let conn = db.lock().unwrap();
        let run_id = "run-1";
        let entry_a = "entry-a";
        let entry_b = "entry-b";
        conn.execute(
            "INSERT INTO auto_queue_runs (id, status, agent_id, created_at) VALUES (?1, 'active', ?2, datetime('now'))",
            rusqlite::params![run_id, agent_id],
        ).unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank)
             VALUES (?1, ?2, 'card-q1', ?3, 'dispatched', 1)",
            rusqlite::params![entry_a, run_id, agent_id],
        ).unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank)
             VALUES (?1, ?2, 'card-q2', ?3, 'pending', 2)",
            rusqlite::params![entry_b, run_id, agent_id],
        ).unwrap();
        (run_id.to_string(), entry_a.to_string(), entry_b.to_string())
    }

    async fn seed_auto_queue_run_pg(
        pool: &sqlx::PgPool,
        agent_id: &str,
    ) -> (String, String, String) {
        let run_id = "run-1";
        let entry_a = "entry-a";
        let entry_b = "entry-b";
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, status, agent_id, created_at)
             VALUES ($1, 'active', $2, NOW())",
        )
        .bind(run_id)
        .bind(agent_id)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, created_at
             )
             VALUES ($1, $2, 'card-q1', $3, 'dispatched', 1, NOW())",
        )
        .bind(entry_a)
        .bind(run_id)
        .bind(agent_id)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, created_at
             )
             VALUES ($1, $2, 'card-q2', $3, 'pending', 2, NOW())",
        )
        .bind(entry_b)
        .bind(run_id)
        .bind(agent_id)
        .execute(pool)
        .await
        .unwrap();
        (run_id.to_string(), entry_a.to_string(), entry_b.to_string())
    }

    /// #110: Pipeline stage should NOT advance on implementation dispatch completion alone.
    /// The onDispatchCompleted in pipeline.js is now a no-op — advancement happens
    /// only through review-automation processVerdict after review passes.
    #[test]
    fn pipeline_no_auto_advance_on_dispatch_complete() {
        let db = test_db();
        let engine = test_engine(&db);

        seed_card_with_repo(&db, "card-pipe", "in_progress", "repo-1");
        let (stage1, _stage2) = seed_pipeline_stages(&db, "repo-1");

        // Assign pipeline stage (use integer id)
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET pipeline_stage_id = ?1 WHERE id = 'card-pipe'",
                [stage1],
            )
            .unwrap();
        }

        // Create and complete an implementation dispatch
        seed_dispatch(&db, "card-pipe", "pending");
        let dispatch_id = "dispatch-card-pipe-pending";
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE task_dispatches SET status = 'completed', result = '{}' WHERE id = ?1",
                [dispatch_id],
            )
            .unwrap();
        }

        // Fire OnDispatchCompleted — should NOT create a new dispatch for stage-2
        let _ = engine
            .try_fire_hook_by_name("OnDispatchCompleted", json!({ "dispatch_id": dispatch_id }));

        // Verify: pipeline_stage_id should still be stage-1 (not advanced)
        // pipeline_stage_id is TEXT, pipeline_stages.id is INTEGER AUTOINCREMENT
        let stage_id: Option<String> = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT pipeline_stage_id FROM kanban_cards WHERE id = 'card-pipe'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            stage_id.as_deref(),
            Some(stage1.to_string().as_str()),
            "pipeline_stage_id must NOT advance on dispatch completion alone"
        );

        // Verify: no new pending dispatch was created for stage-2
        let new_dispatches: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-pipe' AND status = 'pending'",
                [],
                |row| row.get(0),
            ).unwrap()
        };
        assert_eq!(
            new_dispatches, 0,
            "no new dispatch should be created by pipeline.js onDispatchCompleted"
        );
    }

    /// #110: Rust transition_status marks auto_queue_entries as done,
    /// and this single update is sufficient (no JS triple-update).
    #[tokio::test]
    async fn transition_to_done_marks_auto_queue_entry() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());

        // Seed cards for the queue
        seed_card_pg(&pool, "card-q1", "review").await;
        seed_card_pg(&pool, "card-q2", "ready").await;
        seed_dispatch_pg(&pool, "card-q1", "pending").await;
        let (_run_id, entry_a, _entry_b) = seed_auto_queue_run_pg(&pool, "agent-1").await;

        // Transition card-q1 to done
        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-q1",
            "done",
            "review",
            crate::engine::transition::ForceIntent::OperatorOverride,
        )
        .await;
        assert!(result.is_ok(), "transition to done should succeed");

        // Verify: entry_a should be 'done' (set by Rust transition_status)
        let entry_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = $1")
                .bind(&entry_a)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            entry_status, "done",
            "Rust must mark auto_queue_entry as done"
        );
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn run_completion_waits_for_phase_gate_then_enqueues_notify_to_main_channel() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine_with_pg(pool.clone());

        seed_card_with_repo_pg(&pool, "card-notify", "review", "repo-1").await;
        seed_dispatch_pg(&pool, "card-notify", "pending").await;

        sqlx::query(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, unified_thread, unified_thread_id, thread_group_count, created_at
             )
             VALUES ($1, $2, $3, 'active', TRUE, $4::jsonb, 1, NOW())",
        )
        .bind("run-notify")
        .bind("repo-1")
        .bind("agent-1")
        .bind(r#"{"123":"thread-999"}"#)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, priority_rank, created_at
             )
             VALUES ($1, $2, $3, $4, 'dispatched', $5, 1, NOW())",
        )
        .bind("entry-notify")
        .bind("run-notify")
        .bind("card-notify")
        .bind("agent-1")
        .bind("dispatch-card-notify-pending")
        .execute(&pool)
        .await
        .unwrap();

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-notify",
            "done",
            "review",
            crate::engine::transition::ForceIntent::OperatorOverride,
        )
        .await;
        assert!(result.is_ok(), "transition to done should succeed");

        let run_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = 'run-notify'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            run_status, "paused",
            "single-phase terminal completion must pause for a phase gate"
        );

        let phase_gate_dispatch_id: String = sqlx::query_scalar(
            "SELECT id FROM task_dispatches
             WHERE kanban_card_id = 'card-notify' AND dispatch_type = 'phase-gate'
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let queued_notifications: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            queued_notifications, 0,
            "completion notify must wait for the phase gate to pass"
        );

        let completed = crate::dispatch::complete_dispatch(
            &db,
            &engine,
            &phase_gate_dispatch_id,
            &json!({
                "verdict": "phase_gate_passed",
                "summary": "phase gate approved"
            }),
        )
        .expect("phase gate completion should succeed");
        assert_eq!(completed["status"], "completed");

        let run_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = 'run-notify'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(run_status, "completed");

        let (target, bot, content): (String, String, String) = sqlx::query_as(
            "SELECT target, bot, content FROM message_outbox ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(target, "channel:123");
        assert_eq!(bot, "notify");
        assert!(
            content.contains("자동큐 완료: repo-1 / run run-noti / 1개"),
            "notify message should summarize the completed run"
        );
        pg_db.drop().await;
    }

    /// #110: non-terminal manual recovery transitions must not complete auto-queue entries.
    #[tokio::test]
    async fn requested_force_transition_does_not_complete_auto_queue_entry() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());

        seed_card_pg(&pool, "card-pd", "review").await;
        seed_dispatch_pg(&pool, "card-pd", "pending").await;

        sqlx::query(
            "INSERT INTO auto_queue_runs (id, status, agent_id, created_at)
             VALUES ('run-pd', 'active', 'agent-1', NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, created_at
             )
             VALUES ('entry-pd', 'run-pd', 'card-pd', 'agent-1', 'dispatched', 1, NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Transition to requested (NOT done)
        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-pd",
            "requested",
            "pm-gate",
            crate::engine::transition::ForceIntent::SystemRecovery,
        )
        .await;
        assert!(result.is_ok());

        // Verify: entry should still be 'dispatched' (not done)
        let entry_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = 'entry-pd'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            entry_status, "dispatched",
            "requested must NOT mark auto_queue_entry as done"
        );
        pg_db.drop().await;
    }

    /// #128: started_at must reset on every in_progress re-entry (rework/resume).
    /// YAML pipeline uses `mode: coalesce` for in_progress clock, which preserves
    /// the original started_at on rework re-entry. This prevents losing the original
    /// start timestamp. Timeouts.js handles rework re-entry by checking the current
    /// dispatch's created_at rather than started_at.
    #[tokio::test]
    async fn started_at_coalesces_on_in_progress_reentry() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
             VALUES ('agent-1', 'Agent 1', '123', '456')
             ON CONFLICT (id) DO NOTHING",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, started_at, created_at, updated_at
             )
             VALUES ('card-rework', 'Test', 'review', 'agent-1', NOW() - INTERVAL '3 hours', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Add dispatch to authorize transition
        seed_dispatch_pg(&pool, "card-rework", "pending").await;

        // Transition back to in_progress (simulates rework)
        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-rework",
            "in_progress",
            "pm-decision",
            crate::engine::transition::ForceIntent::SystemRecovery,
        )
        .await;
        assert!(result.is_ok(), "rework transition should succeed");

        // Verify started_at was PRESERVED (coalesce mode: original timestamp kept)
        let age_seconds: i64 = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM (NOW() - started_at))::bigint
             FROM kanban_cards
             WHERE id = 'card-rework'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            age_seconds > 3500,
            "started_at should be preserved (coalesce mode), but was only {} seconds ago",
            age_seconds
        );
        pg_db.drop().await;
    }

    /// When started_at is NULL (first-time entry), coalesce mode sets it to now.
    #[tokio::test]
    async fn started_at_set_on_first_in_progress_entry() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());

        seed_card_pg(&pool, "card-first", "requested").await;

        seed_dispatch_pg(&pool, "card-first", "pending").await;

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-first",
            "in_progress",
            "system",
            crate::engine::transition::ForceIntent::SystemRecovery,
        )
        .await;
        assert!(result.is_ok());

        let age_seconds: i64 = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM (NOW() - started_at))::bigint
             FROM kanban_cards
             WHERE id = 'card-first'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            age_seconds < 60,
            "started_at should be set to now on first entry, but was {} seconds ago",
            age_seconds
        );
        pg_db.drop().await;
    }

    /// #800: `reset_full=true` reopens must scrub recorded worktree metadata
    /// from `task_dispatches.context` / `task_dispatches.result` so a follow-up
    /// `latest_completed_work_dispatch_target` call cannot silently re-inject
    /// the stale path into the new dispatch context.
    #[test]
    fn cleanup_force_transition_revert_fields_strips_dispatch_worktree_metadata() {
        let db = test_db();
        seed_card(&db, "card-800-strip-wt", "in_progress");

        let conn = db.lock().unwrap();
        // Two dispatches on the same card, one completed implementation that
        // recorded both context-side and result-side wt metadata, plus a
        // pending dispatch with only context-side wt metadata. We assert that
        // ALL wt-locating keys are removed but unrelated fields survive.
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'd-800-completed', 'card-800-strip-wt', 'agent-1', 'implementation', 'completed',
                'Old impl', ?1, ?2, datetime('now'), datetime('now')
             )",
            rusqlite::params![
                serde_json::json!({
                    "worktree_path": "/tmp/agentdesk-800-stale",
                    "worktree_branch": "wt/800-old",
                    "preserve_me": "context_value"
                })
                .to_string(),
                serde_json::json!({
                    "completed_worktree_path": "/tmp/agentdesk-800-stale",
                    "completed_branch": "wt/800-old",
                    "completed_commit": "deadbeefcafebabe",
                    "preserve_me_too": "result_value"
                })
                .to_string(),
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'd-800-pending', 'card-800-strip-wt', 'agent-1', 'implementation', 'pending',
                'New impl', ?1, NULL, datetime('now'), datetime('now')
             )",
            rusqlite::params![
                serde_json::json!({
                    "worktree_path": "/tmp/agentdesk-800-also-stale",
                    "worktree_branch": "wt/800-also-old",
                    "title_hint": "redispatch"
                })
                .to_string(),
            ],
        )
        .unwrap();
        // A second card's dispatch must be untouched by the scoped cleanup.
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at)
             VALUES ('card-800-other', 'Other', 'in_progress', 'agent-1', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'd-800-other-card', 'card-800-other', 'agent-1', 'implementation', 'completed',
                'Other impl', ?1, ?2, datetime('now'), datetime('now')
             )",
            rusqlite::params![
                serde_json::json!({
                    "worktree_path": "/tmp/agentdesk-800-other-keep",
                    "worktree_branch": "wt/800-other-keep"
                })
                .to_string(),
                serde_json::json!({
                    "completed_worktree_path": "/tmp/agentdesk-800-other-keep",
                    "completed_branch": "wt/800-other-keep"
                })
                .to_string(),
            ],
        )
        .unwrap();

        cleanup_force_transition_revert_fields_on_conn(&conn, "card-800-strip-wt").unwrap();

        // Helper to read a dispatch JSON column back as a serde value.
        let load_json = |dispatch_id: &str, column: &str| -> Option<serde_json::Value> {
            let raw: Option<String> = conn
                .query_row(
                    &format!("SELECT {column} FROM task_dispatches WHERE id = ?1"),
                    [dispatch_id],
                    |row| row.get(0),
                )
                .ok()
                .flatten();
            raw.and_then(|s| serde_json::from_str(&s).ok())
        };

        let completed_ctx = load_json("d-800-completed", "context").unwrap();
        assert!(
            completed_ctx.get("worktree_path").is_none(),
            "context.worktree_path must be removed, got {completed_ctx:?}"
        );
        assert!(
            completed_ctx.get("worktree_branch").is_none(),
            "context.worktree_branch must be removed, got {completed_ctx:?}"
        );
        assert_eq!(
            completed_ctx["preserve_me"].as_str(),
            Some("context_value"),
            "unrelated context fields must be preserved"
        );

        let completed_result = load_json("d-800-completed", "result").unwrap();
        assert!(
            completed_result.get("completed_worktree_path").is_none(),
            "result.completed_worktree_path must be removed, got {completed_result:?}"
        );
        assert!(
            completed_result.get("completed_branch").is_none(),
            "result.completed_branch must be removed, got {completed_result:?}"
        );
        assert_eq!(
            completed_result["completed_commit"].as_str(),
            Some("deadbeefcafebabe"),
            "completion evidence (completed_commit) must be preserved as audit history"
        );
        assert_eq!(
            completed_result["preserve_me_too"].as_str(),
            Some("result_value")
        );

        let pending_ctx = load_json("d-800-pending", "context").unwrap();
        assert!(pending_ctx.get("worktree_path").is_none());
        assert!(pending_ctx.get("worktree_branch").is_none());
        assert_eq!(pending_ctx["title_hint"].as_str(), Some("redispatch"));

        // Other card untouched — both wt-locating keys must still be present.
        let other_ctx = load_json("d-800-other-card", "context").unwrap();
        assert_eq!(
            other_ctx["worktree_path"].as_str(),
            Some("/tmp/agentdesk-800-other-keep"),
            "cleanup must be card-scoped and not touch unrelated cards"
        );
        let other_result = load_json("d-800-other-card", "result").unwrap();
        assert_eq!(
            other_result["completed_worktree_path"].as_str(),
            Some("/tmp/agentdesk-800-other-keep")
        );
    }

    #[test]
    fn github_sync_target_requires_registered_repo_and_matching_issue_repo() {
        let db = test_db();
        seed_card(&db, "card-github-sync-guard", "review");
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards
                 SET repo_id = 'owner/allowed',
                     github_issue_url = 'https://github.com/owner/other/issues/101',
                     github_issue_number = 101
                 WHERE id = 'card-github-sync-guard'",
                [],
            )
            .unwrap();
        }

        // Mismatched URL repo must be rejected.
        assert_eq!(
            github_sync_target_for_card(&db, "card-github-sync-guard"),
            None
        );

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards
                 SET github_issue_url = 'https://github.com/owner/allowed/issues/101'
                 WHERE id = 'card-github-sync-guard'",
                [],
            )
            .unwrap();
        }
        // Matching repo but not registered must still be rejected.
        assert_eq!(
            github_sync_target_for_card(&db, "card-github-sync-guard"),
            None
        );

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO github_repos (id, display_name, sync_enabled) VALUES ('owner/allowed', 'Allowed Repo', 1)",
                [],
            )
            .unwrap();
        }
        assert_eq!(
            github_sync_target_for_card(&db, "card-github-sync-guard"),
            Some(("owner/allowed".to_string(), 101))
        );
    }

    /// #821 (5): `onDispatchCompleted` (kanban-rules.js) must skip cancelled
    /// dispatches. A race can fire the hook after the user cancels a
    /// dispatch; without the guard the policy would force-transition the
    /// card to `review` and the terminal sweep would then push it to `done`,
    /// overriding the user's explicit stop. #815 added the guard —
    /// `if (dispatch.status === "cancelled") return;` — and this test locks
    /// the behaviour.
    #[test]
    fn cancelled_dispatch_does_not_enter_review() {
        let db = test_db();
        let engine = test_engine(&db);

        // Seed a card currently in `in_progress` with a cancelled
        // implementation dispatch. Absent the #815 guard the policy would
        // drive the card into `review` on hook fan-out.
        seed_card(&db, "card-821-no-review", "in_progress");
        let dispatch_id = "dispatch-821-no-review";
        seed_dispatch_with_type(
            &db,
            dispatch_id,
            "card-821-no-review",
            "implementation",
            "cancelled",
        );

        // Fire the hook the same way the real runtime would.
        engine
            .try_fire_hook_by_name("OnDispatchCompleted", json!({ "dispatch_id": dispatch_id }))
            .expect("fire OnDispatchCompleted");

        // The card must remain in its prior status — NOT `review`, NOT `done`.
        let status: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT status FROM kanban_cards WHERE id = 'card-821-no-review'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            status, "in_progress",
            "kanban-rules.onDispatchCompleted must skip cancelled dispatches"
        );
    }
}
