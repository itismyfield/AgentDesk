use super::{AutoQueueLogContext, AutoQueueService};
use crate::services::service_error::{ErrorCode, ServiceError, ServiceResult};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use std::sync::Arc;

use crate::db::auto_queue::run_status::is_live_run_status;

impl AutoQueueService {
    pub async fn cancel_run_with_pg(
        &self,
        health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
        pool: &PgPool,
        run_id: &str,
    ) -> ServiceResult<Value> {
        let run_status = sqlx::query_scalar::<_, Option<String>>(
            "SELECT status
             FROM auto_queue_runs
             WHERE id = $1",
        )
        .bind(run_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            ServiceError::internal(format!("load auto-queue run '{run_id}': {error}"))
                .with_code(ErrorCode::Database)
                .with_context("run_id", run_id)
                .with_operation("auto_queue.cancel_run_with_pg.load_run")
        })?;

        match run_status.flatten() {
            Some(status) if is_live_run_status(&status) => cancel_selected_runs_with_pg(
                health_registry,
                pool,
                &[run_id.to_string()],
                "auto_queue_cancel",
            )
            .await
            .map_err(|error| {
                ServiceError::internal(error)
                    .with_code(ErrorCode::Database)
                    .with_context("run_id", run_id)
                    .with_operation("auto_queue.cancel_run_with_pg.cancel_selected_runs_with_pg")
            }),
            Some(status) => Err(ServiceError::bad_request(format!(
                "auto-queue run '{run_id}' is not cancelable (status={status})"
            ))
            .with_code(ErrorCode::AutoQueue)
            .with_context("run_id", run_id)
            .with_context("status", status)),
            None => Err(
                ServiceError::not_found(format!("auto-queue run '{run_id}' not found"))
                    .with_code(ErrorCode::AutoQueue)
                    .with_context("run_id", run_id),
            ),
        }
    }

    pub async fn cancel_runs_with_pg(
        &self,
        health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
        pool: &PgPool,
    ) -> ServiceResult<Value> {
        cancel_with_pg(health_registry, pool)
            .await
            .map_err(|error| {
                ServiceError::internal(error)
                    .with_code(ErrorCode::Database)
                    .with_operation("auto_queue.cancel_runs_with_pg.cancel_with_pg")
            })
    }
}

#[derive(Debug, Default)]
pub(crate) struct SlotCleanupResult {
    pub(crate) released_slots: usize,
    pub(crate) cleared_slot_sessions: usize,
    pub(crate) warnings: Vec<String>,
}

#[derive(Debug, Default)]
pub(crate) struct LiveRunCleanupResult {
    pub(crate) cancelled_dispatches: usize,
    pub(crate) slot_cleanup: SlotCleanupResult,
}

pub(crate) fn slot_cleanup_warning(warnings: &[String]) -> Option<String> {
    (!warnings.is_empty()).then(|| warnings.join("; "))
}

pub(crate) async fn load_run_ids_with_status_pg(
    pool: &PgPool,
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

pub(crate) async fn load_live_dispatch_ids_for_runs_pg(
    pool: &PgPool,
    run_ids: &[String],
) -> Result<Vec<String>, String> {
    if run_ids.is_empty() {
        return Ok(Vec::new());
    }

    // #2048 F13: guard the jsonb cast against malformed `context` rows.
    // A single corrupt context (legacy migration / direct DB edit) would
    // otherwise crash the WHOLE cancel/force-pause query with a JSON parse
    // error, leaving the operator unable to stop the auto-queue. We probe
    // the leading non-whitespace char before casting; anything that does
    // not start with `{` or `[` is treated as non-JSON and yields NULL.
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
                       WHEN substring(BTRIM(td.context) FROM 1 FOR 1) NOT IN ('{', '[') THEN NULL
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
    pool: &PgPool,
    run_ids: &[String],
) -> Result<Vec<String>, String> {
    if run_ids.is_empty() {
        return Ok(Vec::new());
    }

    sqlx::query_scalar(
        "SELECT DISTINCT e.kanban_card_id
         FROM auto_queue_entries e
         WHERE e.run_id = ANY($1)
           AND e.status IN ('dispatched', 'user_cancelled')
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

pub(crate) async fn delete_phase_gate_rows_for_runs_pg(
    pool: &PgPool,
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
    pool: &PgPool,
    run_ids: &[String],
) -> Result<i64, String> {
    load_live_dispatch_ids_for_runs_pg(pool, run_ids)
        .await
        .map(|rows| rows.len() as i64)
}

pub(crate) async fn cancel_live_dispatches_for_runs_pg(
    pool: &PgPool,
    run_ids: &[String],
    reason: &str,
) -> Result<CancelledDispatchesWithCleanup, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres run-owned dispatch cancel: {error}"))?;
    let transitions = crate::dispatch::cancel_dispatches_for_runs_on_pg_tx_with_meta(
        &mut tx,
        run_ids,
        Some(reason),
        false,
    )
    .await?;

    let dispatch_ids = transitions
        .iter()
        .map(|transition| transition.dispatch_id.clone())
        .collect::<Vec<_>>();
    // #5142: record the post-commit debt inside the same transaction. Before
    // this, the emit / wake / session clear / slot release that follow the
    // commit existed only on this stack, so a crash here left the cancel
    // durable and the cleanup lost.
    let cleanup_task_id = super::cleanup_tasks::enqueue_run_cleanup_task_on_tx(
        &mut tx,
        run_ids,
        &dispatch_ids,
        &[],
        &transitions,
    )
    .await?;

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres run-owned dispatch cancel: {error}"))?;

    Ok(CancelledDispatchesWithCleanup {
        dispatch_ids,
        cleanup_task_id,
    })
}

/// Dispatches cancelled by a committed transaction plus the durable cleanup row
/// that still owes the post-commit steps for them (#5142).
#[derive(Debug)]
pub(crate) struct CancelledDispatchesWithCleanup {
    pub(crate) dispatch_ids: Vec<String>,
    pub(crate) cleanup_task_id: i64,
}

pub(crate) async fn clear_sessions_for_dispatches_pg(
    pool: &PgPool,
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

async fn self_heal_orphan_dispatched_entries_without_slot_pg(
    pool: &PgPool,
    run_ids: &[String],
    trigger_source: &str,
) -> Result<usize, String> {
    if run_ids.is_empty() {
        return Ok(0);
    }

    let entry_ids = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM auto_queue_entries
         WHERE run_id = ANY($1)
           AND status = 'dispatched'
           AND dispatch_id IS NULL
           AND slot_index IS NULL
         ORDER BY id ASC",
    )
    .bind(run_ids)
    .fetch_all(pool)
    .await
    .map_err(|error| {
        format!(
            "load postgres orphan dispatched entry ids {:?}: {error}",
            run_ids
        )
    })?;

    let mut healed = 0usize;
    for entry_id in entry_ids {
        let mut tx = pool.begin().await.map_err(|error| {
            format!("begin postgres orphan dispatched repair transaction {entry_id}: {error}")
        })?;
        let changed = sqlx::query(
            "UPDATE auto_queue_entries
             SET status = 'pending',
                 dispatched_at = NULL
             WHERE id = $1
               AND status = 'dispatched'
               AND dispatch_id IS NULL
               AND slot_index IS NULL",
        )
        .bind(&entry_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("repair postgres orphan dispatched entry {entry_id}: {error}"))?
        .rows_affected() as usize;
        if changed == 0 {
            tx.rollback().await.map_err(|error| {
                format!("rollback unchanged postgres orphan dispatched entry {entry_id}: {error}")
            })?;
            continue;
        }
        let _ = sqlx::query(
            "INSERT INTO auto_queue_entry_transitions (
                entry_id,
                from_status,
                to_status,
                trigger_source
            ) VALUES ($1, 'dispatched', 'pending', $2)",
        )
        .bind(&entry_id)
        .bind(trigger_source)
        .execute(&mut *tx)
        .await;
        tx.commit().await.map_err(|error| {
            format!("commit postgres orphan dispatched repair {entry_id}: {error}")
        })?;
        healed += 1;
    }

    Ok(healed)
}

pub(crate) async fn cancel_and_release_runs_with_pg(
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &PgPool,
    run_ids: &[String],
    reason: &str,
    orphan_trigger_source: Option<&str>,
) -> Result<LiveRunCleanupResult, String> {
    let cancelled = cancel_live_dispatches_for_runs_pg(pool, run_ids, reason).await?;
    let cancelled_dispatches = cancelled.dispatch_ids.len();
    let _self_healed_orphan_entries = match orphan_trigger_source {
        Some(trigger_source) => {
            self_heal_orphan_dispatched_entries_without_slot_pg(pool, run_ids, trigger_source)
                .await?
        }
        None => 0,
    };
    // #5142: the emit / wake / session clear / slot release below used to run as
    // three loose transactions whose progress was invisible after a crash. They
    // now run out of the durable cleanup row committed with the cancel, and a
    // step that fails leaves that row behind for the replay sweep instead of
    // degrading into a warning string.
    let slot_cleanup = super::cleanup_tasks::drain_run_cleanup_task_by_id_pg(
        health_registry,
        pool,
        cancelled.cleanup_task_id,
    )
    .await
    .slot_cleanup;

    Ok(LiveRunCleanupResult {
        cancelled_dispatches,
        slot_cleanup,
    })
}

async fn transition_entry_to_skipped_pg(
    pool: &PgPool,
    entry_id: &str,
    trigger_source: &str,
) -> Result<bool, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres entry skip transaction {entry_id}: {error}"))?;

    let current_row = sqlx::query(
        "SELECT status, dispatch_id
         FROM auto_queue_entries
         WHERE id = $1",
    )
    .bind(entry_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| format!("load postgres entry status {entry_id}: {error}"))?;
    let Some(current_row) = current_row else {
        tx.rollback()
            .await
            .map_err(|error| format!("rollback missing postgres entry {entry_id}: {error}"))?;
        return Ok(false);
    };
    let current_status: Option<String> = current_row
        .try_get("status")
        .map_err(|error| format!("decode postgres entry status {entry_id}: {error}"))?;
    let previous_dispatch_id: Option<String> = current_row
        .try_get("dispatch_id")
        .map_err(|error| format!("decode postgres entry dispatch_id {entry_id}: {error}"))?;
    let Some(current_status) = current_status else {
        tx.rollback()
            .await
            .map_err(|error| format!("rollback missing postgres entry {entry_id}: {error}"))?;
        return Ok(false);
    };
    if !matches!(
        current_status.as_str(),
        "pending" | "dispatched" | "user_cancelled"
    ) {
        tx.rollback().await.map_err(|error| {
            format!("rollback non-skippable postgres entry {entry_id}: {error}")
        })?;
        return Ok(false);
    }

    // #2048 F15: preserve dispatch history before nulling the pointer.
    // `auto_queue_entry_dispatch_history` is the canonical join table for
    // entry↔dispatch correlation in audits/dashboards. Clearing dispatch_id
    // without inserting here makes the cancelled entry's dispatch lineage
    // unrecoverable post-cancel.
    if let Some(previous_dispatch_id) = previous_dispatch_id.as_deref() {
        let _ = sqlx::query(
            "INSERT INTO auto_queue_entry_dispatch_history (
                 entry_id, dispatch_id, trigger_source
             )
             SELECT $1, $2, $3
             WHERE EXISTS (SELECT 1 FROM task_dispatches WHERE id = $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(entry_id)
        .bind(previous_dispatch_id)
        .bind(trigger_source)
        .execute(&mut *tx)
        .await;
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

async fn rollback_cancelled_run_cards_pg(
    pool: &PgPool,
    card_ids: &[String],
    source: &str,
) -> usize {
    let mut rolled_back = 0usize;

    for card_id in card_ids {
        let status = match sqlx::query_scalar::<_, Option<String>>(
            "SELECT status FROM kanban_cards WHERE id = $1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some(status)) => status,
            Ok(None) => continue,
            Err(error) => {
                crate::auto_queue_log!(
                    warn,
                    "run_cancel_card_status_pg_failed",
                    AutoQueueLogContext::new().card(card_id),
                    "[auto-queue] failed to load postgres card {} during run cancel rollback: {}",
                    card_id,
                    error
                );
                continue;
            }
        };
        if !matches!(status.as_deref(), Some("requested") | Some("in_progress")) {
            continue;
        }

        let has_active_dispatch = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM task_dispatches
             WHERE kanban_card_id = $1 AND status IN ('pending', 'dispatched')",
        )
        .bind(card_id)
        .fetch_one(pool)
        .await
        .ok()
        .unwrap_or(0)
            > 0;
        if has_active_dispatch {
            continue;
        }

        let mut tx = match pool.begin().await {
            Ok(tx) => tx,
            Err(error) => {
                crate::auto_queue_log!(
                    warn,
                    "run_cancel_card_rollback_pg_begin_failed",
                    AutoQueueLogContext::new().card(card_id),
                    "[auto-queue] failed to open postgres rollback transaction for card {} during run cancel: {}",
                    card_id,
                    error
                );
                continue;
            }
        };

        let rollback_result = async {
            // #1081: route status + review/dispatch pointer clears through the
            // canonical FSM executor (`execute_pg_transition_intent`) instead
            // of a direct status write. The enclosing `tx` keeps the
            // transition + ancillary field cleanup atomic.
            let current_status: Option<String> = sqlx::query_scalar::<_, Option<String>>(
                "SELECT status FROM kanban_cards WHERE id = $1 FOR UPDATE",
            )
            .bind(card_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| format!("reload postgres card status {card_id}: {error}"))?
            .flatten();
            match current_status.as_deref() {
                Some("requested") | Some("in_progress") => {}
                _ => return Ok(false),
            }
            let from_status = current_status
                .or_else(|| status.clone())
                .unwrap_or_default();

            crate::engine::transition_executor_pg::execute_pg_transition_intent(
                &mut tx,
                &crate::engine::transition::TransitionIntent::UpdateStatus {
                    card_id: card_id.to_string(),
                    from: from_status.clone(),
                    to: "ready".to_string(),
                },
            )
            .await
            .map_err(|error| error.to_string())?;

            crate::engine::transition_executor_pg::execute_pg_transition_intent(
                &mut tx,
                &crate::engine::transition::TransitionIntent::SetLatestDispatchId {
                    card_id: card_id.to_string(),
                    dispatch_id: None,
                },
            )
            .await
            .map_err(|error| error.to_string())?;

            crate::engine::transition_executor_pg::execute_pg_transition_intent(
                &mut tx,
                &crate::engine::transition::TransitionIntent::SetReviewStatus {
                    card_id: card_id.to_string(),
                    review_status: None,
                },
            )
            .await
            .map_err(|error| error.to_string())?;

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
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("reset postgres card cleanup fields {card_id}: {error}"))?;

            sqlx::query(
                "INSERT INTO card_review_state (
                    card_id, review_round, state, pending_dispatch_id, last_verdict, last_decision,
                    decided_by, decided_at, approach_change_round, session_reset_round, review_entered_at, updated_at
                 ) VALUES (
                    $1, 0, 'idle', NULL, NULL, NULL,
                    NULL, NULL, NULL, NULL, NULL, NOW()
                 )
                 ON CONFLICT (card_id) DO UPDATE SET
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
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("reset postgres card review state {card_id}: {error}"))?;

            sqlx::query("DELETE FROM kv_meta WHERE key = $1 OR key = $2")
                .bind(format!("pm_pending:{card_id}"))
                .bind(format!("pm_decision_sent:{card_id}"))
                .execute(&mut *tx)
                .await
                .map_err(|error| format!("clear postgres card escalation state {card_id}: {error}"))?;

            sqlx::query(
                "INSERT INTO kanban_audit_logs (card_id, from_status, to_status, source, result)
                 VALUES ($1, $2, 'ready', $3, 'OK (run cancel rollback)')",
            )
            .bind(card_id)
            .bind(&status)
            .bind(source)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("insert postgres kanban audit log {card_id}: {error}"))?;

            sqlx::query(
                "UPDATE task_dispatches
                 SET context = CASE
                         WHEN context IS NULL OR context = '' THEN context
                         ELSE NULLIF(
                             (context::jsonb
                                 - 'worktree_path'
                                 - 'worktree_branch'
                                 - 'completed_worktree_path'
                                 - 'completed_branch'
                             )::text,
                             '{}'
                         )
                     END,
                     result = CASE
                         WHEN result IS NULL OR result = '' THEN result
                         ELSE NULLIF(
                             (result::jsonb
                                 - 'worktree_path'
                                 - 'worktree_branch'
                                 - 'completed_worktree_path'
                                 - 'completed_branch'
                             )::text,
                             '{}'
                         )
                     END
                 WHERE kanban_card_id = $1
                   AND (
                       (context IS NOT NULL AND context <> '' AND (
                           (context::jsonb) ? 'worktree_path'
                           OR (context::jsonb) ? 'worktree_branch'
                           OR (context::jsonb) ? 'completed_worktree_path'
                           OR (context::jsonb) ? 'completed_branch'
                       ))
                       OR (result IS NOT NULL AND result <> '' AND (
                           (result::jsonb) ? 'worktree_path'
                           OR (result::jsonb) ? 'worktree_branch'
                           OR (result::jsonb) ? 'completed_worktree_path'
                           OR (result::jsonb) ? 'completed_branch'
                       ))
                   )",
            )
            .bind(card_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                format!("scrub postgres dispatch worktree metadata {card_id}: {error}")
            })?;

            Ok::<bool, String>(true)
        }
        .await;

        match rollback_result {
            Ok(true) => {
                if tx.commit().await.is_ok() {
                    rolled_back += 1;
                }
            }
            Ok(false) => {
                let _ = tx.rollback().await;
            }
            Err(error) => {
                let _ = tx.rollback().await;
                crate::auto_queue_log!(
                    warn,
                    "run_cancel_card_rollback_pg_failed",
                    AutoQueueLogContext::new().card(card_id),
                    "[auto-queue] failed to roll back postgres card {} during run cancel: {}",
                    card_id,
                    error
                );
            }
        }
    }

    rolled_back
}

async fn terminalize_selected_runs_with_pg(
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &PgPool,
    target_run_ids: &[String],
    reason: &str,
) -> Result<(Value, Vec<String>), String> {
    let completes_run = reason == "auto_queue_end";
    let run_status_filter = if completes_run {
        "status IN ('active', 'paused', 'generated', 'pending')"
    } else {
        "status IN ('active', 'paused', 'restoring')"
    };
    let terminal_status = if completes_run {
        "completed"
    } else {
        "cancelled"
    };
    if target_run_ids.is_empty() {
        return Ok((
            json!({
                "ok": true,
                "cancelled_entries": 0usize,
                "cancelled_runs": 0usize,
                "cancelled_dispatches": 0usize,
                "deleted_phase_gates": 0usize,
                "rolled_back_cards": 0usize,
                "remaining_live_dispatches": 0usize,
                "released_slots": 0usize,
                "cleared_slot_sessions": 0usize,
            }),
            Vec::new(),
        ));
    }

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres run cancel transaction: {error}"))?;

    let lock_status_filter = if completes_run {
        run_status_filter
    } else {
        "TRUE"
    };
    let lock_runs_sql = format!(
        "SELECT id
         FROM auto_queue_runs
         WHERE id = ANY($1)
           AND {lock_status_filter}"
    );
    let lock_candidates = sqlx::query_scalar::<_, String>(&lock_runs_sql)
        .bind(target_run_ids)
        .fetch_all(&mut *tx)
        .await
        .map_err(|error| format!("load postgres auto_queue_runs for cancel: {error}"))?;
    let locked_run_ids =
        crate::db::auto_queue::acquire_run_advisory_xact_locks_on_pg_tx(&mut tx, &lock_candidates)
            .await?;

    let rollback_candidate_card_ids = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT kanban_card_id
         FROM auto_queue_entries
         WHERE run_id = ANY($1)
           AND status IN ('dispatched', 'user_cancelled')
           AND kanban_card_id IS NOT NULL
           AND BTRIM(kanban_card_id) <> ''
         ORDER BY kanban_card_id",
    )
    .bind(&locked_run_ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(|error| format!("load postgres cancellation card ids: {error}"))?;

    let entry_rows = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT id, status, dispatch_id
         FROM auto_queue_entries
         WHERE run_id = ANY($1)
           AND status IN ('pending', 'dispatched', 'user_cancelled')
         ORDER BY id ASC
         FOR UPDATE",
    )
    .bind(&locked_run_ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(|error| format!("load postgres cancel entries: {error}"))?;

    let cancel_metas = crate::dispatch::cancel_dispatches_for_runs_on_pg_tx_with_meta(
        &mut tx,
        &locked_run_ids,
        Some(reason),
        false,
    )
    .await?;

    // Delete gate rows in the same commit as the terminal state changes. This
    // prevents terminal-dispatch reconciliation from manufacturing a failure
    // alert while an operator-requested run cancellation is in flight.
    let deleted_phase_gates =
        sqlx::query("DELETE FROM auto_queue_phase_gates WHERE run_id = ANY($1)")
            .bind(&locked_run_ids)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("delete postgres auto_queue_phase_gates: {error}"))?
            .rows_affected() as usize;

    let update_runs_sql = format!(
        "UPDATE auto_queue_runs
         SET status = $2,
             completed_at = NOW()
         WHERE id = ANY($1)
           AND {run_status_filter}"
    );
    let terminalized_runs = sqlx::query(&update_runs_sql)
        .bind(&locked_run_ids)
        .bind(terminal_status)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("cancel postgres auto_queue_runs: {error}"))?
        .rows_affected() as usize;

    let mut terminalized_entries = 0usize;
    for (entry_id, _entry_status, _dispatch_id) in entry_rows {
        let current_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_entries WHERE id = $1")
                .bind(&entry_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(|error| format!("reload postgres cancel entry {entry_id}: {error}"))?;
        if matches!(
            current_status.as_str(),
            "pending" | "dispatched" | "user_cancelled"
        ) {
            crate::db::auto_queue::update_entry_status_on_pg_tx(
                &mut tx,
                &entry_id,
                crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
                "run_cancel",
                &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
            )
            .await?;
        }
        terminalized_entries += 1;
    }

    let released_slot_rows = sqlx::query(
        "UPDATE auto_queue_slots
         SET assigned_run_id = NULL,
             assigned_thread_group = NULL,
             updated_at = NOW()
         WHERE assigned_run_id = ANY($1)
         RETURNING agent_id, slot_index",
    )
    .bind(&locked_run_ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(|error| format!("release postgres slots for cancelled runs: {error}"))?;
    let released_slots = released_slot_rows.len();
    let released_slot_keys = released_slot_rows
        .into_iter()
        .map(|row| {
            let agent_id = row
                .try_get::<String, _>("agent_id")
                .map_err(|error| format!("decode released slot agent: {error}"))?;
            let slot_index = row
                .try_get::<i64, _>("slot_index")
                .map_err(|error| format!("decode released slot index: {error}"))?;
            Ok(super::cleanup_tasks::ReleasedSlot {
                agent_id,
                slot_index,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    if completes_run && terminalized_runs > 0 {
        for run_id in &locked_run_ids {
            crate::db::auto_queue::queue_run_completion_notify_on_pg(&mut tx, run_id).await?;
        }
    }

    let cancelled_dispatch_ids = cancel_metas
        .iter()
        .map(|meta| meta.dispatch_id.clone())
        .collect::<Vec<_>>();
    // #5142: this transaction already released the slot rows, so the cleanup row
    // carries the slot keys directly; the steps that still have to run outside
    // the commit (emit, wake, session clear, slot-thread clear) become durable
    // here rather than living only on this stack.
    let cleanup_task_id = super::cleanup_tasks::enqueue_run_cleanup_task_on_tx(
        &mut tx,
        &locked_run_ids,
        &cancelled_dispatch_ids,
        &released_slot_keys,
        &cancel_metas,
    )
    .await?;

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres run cancel transaction: {error}"))?;

    let drained = super::cleanup_tasks::drain_run_cleanup_task_by_id_pg(
        health_registry,
        pool,
        cleanup_task_id,
    )
    .await;
    let remaining_live_dispatches = count_live_dispatches_for_runs_pg(pool, target_run_ids).await?;
    let cleanup = LiveRunCleanupResult {
        cancelled_dispatches: cancel_metas.len(),
        slot_cleanup: SlotCleanupResult {
            released_slots,
            cleared_slot_sessions: drained.slot_cleanup.cleared_slot_sessions,
            warnings: drained.slot_cleanup.warnings,
        },
    };
    if remaining_live_dispatches > 0 {
        let log_ctx = target_run_ids
            .first()
            .map(|run_id| AutoQueueLogContext::new().run(run_id))
            .unwrap_or_default();
        crate::auto_queue_log!(
            warn,
            "run_cancel_remaining_live_dispatches_pg",
            log_ctx,
            "[auto-queue] postgres cancel left {} non-terminal dispatches for runs {:?}",
            remaining_live_dispatches,
            target_run_ids
        );
    }

    let mut response = json!({
        "ok": true,
        "cancelled_entries": terminalized_entries,
        "cancelled_runs": terminalized_runs,
        "cancelled_dispatches": cleanup.cancelled_dispatches,
        "deleted_phase_gates": deleted_phase_gates,
        "rolled_back_cards": 0usize,
        "remaining_live_dispatches": remaining_live_dispatches,
        "released_slots": cleanup.slot_cleanup.released_slots,
        "cleared_slot_sessions": cleanup.slot_cleanup.cleared_slot_sessions,
    });
    if let Some(warning) = slot_cleanup_warning(&cleanup.slot_cleanup.warnings) {
        response["warning"] = json!(warning);
    }
    Ok((response, rollback_candidate_card_ids))
}

pub(crate) async fn cancel_selected_runs_with_pg(
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &PgPool,
    target_run_ids: &[String],
    reason: &str,
) -> Result<Value, String> {
    let (mut response, rollback_candidate_card_ids) =
        terminalize_selected_runs_with_pg(health_registry, pool, target_run_ids, reason).await?;
    let rolled_back_cards =
        rollback_cancelled_run_cards_pg(pool, &rollback_candidate_card_ids, reason).await;
    response["rolled_back_cards"] = json!(rolled_back_cards);
    Ok(response)
}

pub(crate) async fn end_run_with_pg(
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &PgPool,
    run_id: &str,
) -> Result<bool, String> {
    let (response, _) = terminalize_selected_runs_with_pg(
        health_registry,
        pool,
        &[run_id.to_string()],
        "auto_queue_end",
    )
    .await?;
    Ok(response
        .get("cancelled_runs")
        .and_then(Value::as_u64)
        .is_some_and(|count| count > 0))
}

pub(crate) async fn skip_dispatched_entries_for_runs_pg(
    pool: &PgPool,
    run_ids: &[String],
    trigger_source: &str,
) -> Result<usize, String> {
    if run_ids.is_empty() {
        return Ok(0);
    }

    let entry_ids = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM auto_queue_entries
         WHERE run_id = ANY($1)
           AND status = 'dispatched'
         ORDER BY id ASC",
    )
    .bind(run_ids)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load postgres dispatched entries for pause: {error}"))?;

    let mut skipped = 0usize;
    for entry_id in entry_ids {
        match transition_entry_to_skipped_pg(pool, &entry_id, trigger_source).await {
            Ok(true) => skipped += 1,
            Ok(false) => {}
            Err(error) => crate::auto_queue_log!(
                warn,
                "pause_skip_entry_pg_failed",
                AutoQueueLogContext::new().entry(&entry_id),
                "[auto-queue] failed to skip postgres dispatched entry {}: {}",
                entry_id,
                error
            ),
        }
    }

    Ok(skipped)
}

pub(crate) async fn cancel_with_pg(
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &PgPool,
) -> Result<Value, String> {
    let target_run_ids =
        load_run_ids_with_status_pg(pool, crate::db::auto_queue::run_status::LIVE_RUN_STATUSES)
            .await?;
    cancel_selected_runs_with_pg(health_registry, pool, &target_run_ids, "auto_queue_cancel").await
}

// #4953: every test here needs a live PostgreSQL server, so the module name
// must carry the `pg_` marker that `just test-postgres` and the test-lane
// coverage gate select on. Naming it `tests` put it outside the PG lane's
// module-level match while the non-PG lane still ran it, so the PG-less
// `full_non_pg` CI job executed it and failed with a 15s pool timeout.
#[cfg(test)]
mod pg_tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;

    async fn seed_cancel_fixture(pool: &PgPool, suffix: &str) -> (String, String, String) {
        let run_id = format!("run-cancel-{suffix}");
        let entry_id = format!("entry-cancel-{suffix}");
        let dispatch_id = format!("dispatch-cancel-{suffix}");
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-cancel', 'Cancel Agent', 'claude', '123')
             ON CONFLICT (id) DO NOTHING",
        )
        .execute(pool)
        .await
        .expect("seed cancel agent");
        let card_id = format!("card-cancel-{suffix}");
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES ($1, 'Cancel Card', 'in_progress', 'agent-cancel')",
        )
        .bind(&card_id)
        .execute(pool)
        .await
        .expect("seed cancel card");
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, agent_id, status)
             VALUES ($1, 'agent-cancel', 'active')",
        )
        .bind(&run_id)
        .execute(pool)
        .await
        .expect("seed cancel run");
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
             VALUES ($1, $2, 'agent-cancel', 'implementation', 'dispatched', 'Cancel Dispatch')",
        )
        .bind(&dispatch_id)
        .bind(&card_id)
        .execute(pool)
        .await
        .expect("seed cancel dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index)
             VALUES ($1, $2, $3, 'agent-cancel', 'dispatched', $4, 0)",
        )
        .bind(&entry_id)
        .bind(&run_id)
        .bind(&card_id)
        .bind(&dispatch_id)
        .execute(pool)
        .await
        .expect("seed cancel entry");
        sqlx::query(
            "INSERT INTO auto_queue_slots
                (agent_id, slot_index, assigned_run_id, assigned_thread_group)
             VALUES ('agent-cancel', 0, $1, 0)",
        )
        .bind(&run_id)
        .execute(pool)
        .await
        .expect("seed cancel slot");
        (run_id, entry_id, dispatch_id)
    }

    #[derive(Clone, Copy)]
    enum OwnerArm {
        Entry,
        PhaseGate,
        Context,
    }

    async fn add_dispatch_owner(pool: &PgPool, dispatch_id: &str, run_id: &str, arm: OwnerArm) {
        match arm {
            OwnerArm::Entry => {
                sqlx::query(
                    "INSERT INTO auto_queue_entries
                        (id, run_id, agent_id, status, dispatch_id)
                     VALUES ($1, $2, 'agent-cancel', 'dispatched', $3)",
                )
                .bind(format!("entry-{dispatch_id}-{run_id}"))
                .bind(run_id)
                .bind(dispatch_id)
                .execute(pool)
                .await
                .expect("seed dispatch entry owner"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
            }
            OwnerArm::PhaseGate => {
                sqlx::query(
                    "INSERT INTO auto_queue_phase_gates (run_id, phase, status, dispatch_id)
                     VALUES ($1, 0, 'pending', $2)",
                )
                .bind(run_id)
                .bind(dispatch_id)
                .execute(pool)
                .await
                .expect("seed dispatch phase-gate owner"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
            }
            OwnerArm::Context => {
                sqlx::query(
                    "UPDATE task_dispatches
                     SET context = jsonb_build_object(
                         'phase_gate', jsonb_build_object('run_id', $1::text)
                     )::text
                     WHERE id = $2",
                )
                .bind(run_id)
                .bind(dispatch_id)
                .execute(pool)
                .await
                .expect("seed dispatch context owner"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
            }
        }
    }

    async fn assert_foreign_owner_preserved(
        suffix: &str,
        target_arm: OwnerArm,
        foreign_arm: OwnerArm,
        sole_arm: OwnerArm,
    ) {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-cancel', 'Cancel Agent', 'claude', '123')
             ON CONFLICT (id) DO NOTHING",
        )
        .execute(&pool)
        .await
        .expect("seed cancel agent"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        let target_run = format!("run-target-{suffix}");
        let foreign_run = format!("run-foreign-{suffix}");
        let shared_dispatch = format!("dispatch-shared-{suffix}");
        let sole_dispatch = format!("dispatch-sole-{suffix}");
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, agent_id, status)
             VALUES ($1, 'agent-cancel', 'active'), ($2, 'agent-cancel', 'active')",
        )
        .bind(&target_run)
        .bind(&foreign_run)
        .execute(&pool)
        .await
        .expect("seed owner runs"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, to_agent_id, dispatch_type, status, title)
             VALUES ($1, 'agent-cancel', 'implementation', 'dispatched', 'Shared'),
                    ($2, 'agent-cancel', 'implementation', 'dispatched', 'Sole')",
        )
        .bind(&shared_dispatch)
        .bind(&sole_dispatch)
        .execute(&pool)
        .await
        .expect("seed owner dispatches"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        add_dispatch_owner(&pool, &shared_dispatch, &target_run, target_arm).await;
        add_dispatch_owner(&pool, &shared_dispatch, &foreign_run, foreign_arm).await;
        add_dispatch_owner(&pool, &sole_dispatch, &target_run, sole_arm).await;
        let foreign_entry = (matches!(foreign_arm, OwnerArm::Entry))
            .then(|| format!("entry-{shared_dispatch}-{foreign_run}"));

        let response = cancel_selected_runs_with_pg(
            None,
            &pool,
            std::slice::from_ref(&target_run),
            "auto_queue_cancel",
        )
        .await
        .expect("cancel target run through production entrypoint"); // agentdesk-audit: allow-unwrap — test invokes the production entrypoint
        assert_eq!(response["cancelled_dispatches"], 1);
        let states = sqlx::query_as::<_, (String, String)>(
            "SELECT shared.status, sole.status
             FROM task_dispatches shared
             JOIN task_dispatches sole ON sole.id = $2
             WHERE shared.id = $1",
        )
        .bind(&shared_dispatch)
        .bind(&sole_dispatch)
        .fetch_one(&pool)
        .await
        .expect("load owner dispatch states"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
        assert_eq!(states, ("dispatched".to_string(), "cancelled".to_string()));
        if let Some(foreign_entry) = foreign_entry {
            let entry = sqlx::query_as::<_, (String, Option<String>, i64)>(
                "SELECT status, dispatch_id,
                        (SELECT COUNT(*) FROM auto_queue_entry_transitions WHERE entry_id = $1)
                 FROM auto_queue_entries WHERE id = $1",
            )
            .bind(&foreign_entry)
            .fetch_one(&pool)
            .await
            .expect("load foreign entry state"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
            assert_eq!(entry, ("dispatched".to_string(), Some(shared_dispatch), 0));
        }
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn cancel_live_dispatches_for_runs_pg_preserves_entry_until_force_pause_skip() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, entry_id, dispatch_id) = seed_cancel_fixture(&pool, "force-pause-entry").await;

        let cancelled = cancel_live_dispatches_for_runs_pg(
            &pool,
            std::slice::from_ref(&run_id),
            "auto_queue_pause",
        )
        .await
        .expect("cancel force-pause dispatches"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(cancelled.dispatch_ids, vec![dispatch_id.clone()]);
        let state = sqlx::query_as::<_, (String, Option<String>, String)>(
            "SELECT e.status, e.dispatch_id, d.status
             FROM auto_queue_entries e
             JOIN task_dispatches d ON d.id = $2
             WHERE e.id = $1",
        )
        .bind(&entry_id)
        .bind(&dispatch_id)
        .fetch_one(&pool)
        .await
        .expect("load force-pause entry state"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
        assert_eq!(
            state,
            (
                "dispatched".to_string(),
                Some(dispatch_id),
                "cancelled".to_string()
            )
        );
        assert_eq!(
            skip_dispatched_entries_for_runs_pg(&pool, &[run_id], "run_pause")
                .await
                .expect("skip force-pause entry"), // agentdesk-audit: allow-unwrap — production entrypoint assertion
            1
        );
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn cancel_live_dispatches_for_runs_pg_preserves_foreign_session() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (target_run, _entry_id, dispatch_id) =
            seed_cancel_fixture(&pool, "force-pause-session").await;
        let foreign_run = "run-foreign-force-pause-session";
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, agent_id, status)
             VALUES ($1, 'agent-cancel', 'active')",
        )
        .bind(foreign_run)
        .execute(&pool)
        .await
        .expect("seed force-pause foreign run"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        add_dispatch_owner(&pool, &dispatch_id, foreign_run, OwnerArm::Context).await;
        sqlx::query(
            "INSERT INTO sessions (session_key, agent_id, status, active_dispatch_id)
             VALUES ('session-force-pause-foreign', 'agent-cancel', 'turn_active', $1)",
        )
        .bind(&dispatch_id)
        .execute(&pool)
        .await
        .expect("seed force-pause foreign session"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        assert!(
            cancel_live_dispatches_for_runs_pg(&pool, &[target_run], "auto_queue_pause")
                .await
                .expect("cancel target force-pause dispatches") // agentdesk-audit: allow-unwrap — production entrypoint assertion
                .dispatch_ids
                .is_empty()
        );
        let state = sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT d.status, s.status, s.active_dispatch_id
             FROM task_dispatches d
             JOIN sessions s ON s.session_key = 'session-force-pause-foreign'
             WHERE d.id = $1",
        )
        .bind(&dispatch_id)
        .fetch_one(&pool)
        .await
        .expect("load force-pause foreign session state"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
        assert_eq!(
            state,
            (
                "dispatched".to_string(),
                "turn_active".to_string(),
                Some(dispatch_id)
            )
        );
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn cancel_selected_runs_with_pg_atomically_terminalizes_run_entry_and_dispatch() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, entry_id, dispatch_id) = seed_cancel_fixture(&pool, "entrypoint").await;

        let response = cancel_selected_runs_with_pg(
            None,
            &pool,
            std::slice::from_ref(&run_id),
            "auto_queue_cancel",
        )
        .await
        .expect("cancel selected run through production entrypoint");
        assert_eq!(response["cancelled_runs"], 1);
        assert_eq!(response["cancelled_entries"], 1);
        assert_eq!(response["cancelled_dispatches"], 1);

        let state = sqlx::query_as::<_, (String, String, Option<String>, String, Option<String>)>(
            "SELECT r.status, e.status, e.dispatch_id, d.status, s.assigned_run_id
             FROM auto_queue_runs r
             JOIN auto_queue_entries e ON e.id = $2
             JOIN task_dispatches d ON d.id = $3
             JOIN auto_queue_slots s ON s.agent_id = 'agent-cancel' AND s.slot_index = 0
             WHERE r.id = $1",
        )
        .bind(&run_id)
        .bind(&entry_id)
        .bind(&dispatch_id)
        .fetch_one(&pool)
        .await
        .expect("load cancelled run state");
        assert_eq!(
            state,
            (
                "cancelled".to_string(),
                "skipped".to_string(),
                None,
                "cancelled".to_string(),
                None,
            )
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn cancel_selected_runs_with_pg_preserves_foreign_context_owner() {
        assert_foreign_owner_preserved(
            "foreign-context",
            OwnerArm::Entry,
            OwnerArm::Context,
            OwnerArm::Context,
        )
        .await;
    }

    #[tokio::test]
    async fn cancel_selected_runs_with_pg_preserves_foreign_entry_owner() {
        assert_foreign_owner_preserved(
            "foreign-entry",
            OwnerArm::Context,
            OwnerArm::Entry,
            OwnerArm::Entry,
        )
        .await;
    }

    #[tokio::test]
    async fn cancel_selected_runs_with_pg_preserves_foreign_phase_gate_owner() {
        assert_foreign_owner_preserved(
            "foreign-phase-gate",
            OwnerArm::Context,
            OwnerArm::PhaseGate,
            OwnerArm::PhaseGate,
        )
        .await;
    }
}
