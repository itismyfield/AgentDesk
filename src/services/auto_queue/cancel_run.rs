use super::{AutoQueueLogContext, AutoQueueService};
use crate::services::service_error::{ErrorCode, ServiceError, ServiceResult};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use std::collections::HashSet;
use std::sync::Arc;

const RUN_STATUS_RESTORING: &str = "restoring";

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

        match run_status.flatten().as_deref() {
            Some("active") | Some("paused") | Some(RUN_STATUS_RESTORING) => {
                cancel_selected_runs_with_pg(
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
                        .with_operation(
                            "auto_queue.cancel_run_with_pg.cancel_selected_runs_with_pg",
                        )
                })
            }
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
) -> Result<usize, String> {
    let dispatch_ids = load_live_dispatch_ids_for_runs_pg(pool, run_ids).await?;
    let cancel_payload = json!({ "reason": reason });
    let mut cancelled = 0usize;
    for dispatch_id in dispatch_ids {
        cancelled += crate::dispatch::set_dispatch_status_without_queue_sync_with_backends(
            None,
            Some(pool),
            &dispatch_id,
            "cancelled",
            Some(&cancel_payload),
            "cancel_dispatch",
            Some(&["pending", "dispatched"]),
            false,
        )
        .map_err(|error| format!("cancel postgres dispatch {dispatch_id}: {error}"))?;
    }
    Ok(cancelled)
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
               AND status IN ('working', 'idle')",
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
    let live_dispatch_ids = load_live_dispatch_ids_for_runs_pg(pool, run_ids).await?;
    let cancelled_dispatches = cancel_live_dispatches_for_runs_pg(pool, run_ids, reason).await?;
    let _self_healed_orphan_entries = match orphan_trigger_source {
        Some(trigger_source) => {
            self_heal_orphan_dispatched_entries_without_slot_pg(pool, run_ids, trigger_source)
                .await?
        }
        None => 0,
    };
    let mut slot_cleanup =
        clear_and_release_slots_for_runs_pg(health_registry, pool, run_ids).await;
    match clear_sessions_for_dispatches_pg(pool, &live_dispatch_ids).await {
        Ok(cleared) => slot_cleanup.cleared_slot_sessions += cleared,
        Err(error) => {
            crate::auto_queue_log!(
                warn,
                "run_cleanup_dispatch_session_clear_pg_failed",
                run_ids
                    .first()
                    .map(|run_id| AutoQueueLogContext::new().run(run_id))
                    .unwrap_or_default(),
                "[auto-queue] failed to clear postgres sessions for run cleanup dispatches {:?}: {}",
                live_dispatch_ids,
                error
            );
            slot_cleanup.warnings.push(format!(
                "failed to clear postgres sessions for run cleanup dispatches {:?}: {}",
                live_dispatch_ids, error
            ));
        }
    }

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
    if !matches!(
        current_status.as_str(),
        "pending" | "dispatched" | "user_cancelled"
    ) {
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

pub(crate) async fn clear_and_release_slots_for_runs_pg(
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &PgPool,
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
                        match super::runtime::clear_slot_threads_for_slot_pg(
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

pub(crate) async fn cancel_selected_runs_with_pg(
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &PgPool,
    target_run_ids: &[String],
    reason: &str,
) -> Result<Value, String> {
    let rollback_candidate_card_ids =
        load_dispatched_card_ids_for_runs_pg(pool, target_run_ids).await?;
    let cleanup = cancel_and_release_runs_with_pg(
        health_registry,
        pool,
        target_run_ids,
        reason,
        Some("run_cancel_orphan_self_heal"),
    )
    .await?;
    let deleted_phase_gates = delete_phase_gate_rows_for_runs_pg(pool, target_run_ids).await?;

    let cancelled_runs = if target_run_ids.is_empty() {
        0
    } else {
        let mut query = QueryBuilder::<Postgres>::new(
            "UPDATE auto_queue_runs
             SET status = 'cancelled',
                 completed_at = NOW()
             WHERE id IN (",
        );
        let mut separated = query.separated(", ");
        for run_id in target_run_ids {
            separated.push_bind(run_id);
        }
        separated.push_unseparated(format!(
            ") AND status IN ('active', 'paused', '{}')",
            RUN_STATUS_RESTORING
        ));
        query
            .build()
            .execute(pool)
            .await
            .map_err(|error| {
                format!(
                    "cancel postgres auto_queue_runs {:?}: {error}",
                    target_run_ids
                )
            })?
            .rows_affected() as usize
    };

    let entry_rows: Vec<String> = if target_run_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_scalar::<_, String>(
            "SELECT id
             FROM auto_queue_entries
             WHERE run_id = ANY($1)
               AND status IN ('pending', 'dispatched', 'user_cancelled')
             ORDER BY id ASC",
        )
        .bind(target_run_ids)
        .fetch_all(pool)
        .await
        .map_err(|error| {
            format!(
                "load postgres cancel entry ids {:?}: {error}",
                target_run_ids
            )
        })?
    };

    let mut cancelled_entries = 0usize;
    for entry_id in entry_rows {
        match transition_entry_to_skipped_pg(pool, &entry_id, "run_cancel").await {
            Ok(true) => cancelled_entries += 1,
            Ok(false) => {}
            Err(error) => crate::auto_queue_log!(
                warn,
                "run_cancel_entry_pg_failed",
                AutoQueueLogContext::new().entry(&entry_id),
                "[auto-queue] failed to cancel postgres entry {} during run cancel: {}",
                entry_id,
                error
            ),
        }
    }

    let rolled_back_cards =
        rollback_cancelled_run_cards_pg(pool, &rollback_candidate_card_ids, reason).await;
    let remaining_live_dispatches = count_live_dispatches_for_runs_pg(pool, target_run_ids).await?;
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
        "cancelled_entries": cancelled_entries,
        "cancelled_runs": cancelled_runs,
        "cancelled_dispatches": cleanup.cancelled_dispatches,
        "deleted_phase_gates": deleted_phase_gates,
        "rolled_back_cards": rolled_back_cards,
        "remaining_live_dispatches": remaining_live_dispatches,
        "released_slots": cleanup.slot_cleanup.released_slots,
        "cleared_slot_sessions": cleanup.slot_cleanup.cleared_slot_sessions,
    });
    if let Some(warning) = slot_cleanup_warning(&cleanup.slot_cleanup.warnings) {
        response["warning"] = json!(warning);
    }
    Ok(response)
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
        load_run_ids_with_status_pg(pool, &["active", "paused", RUN_STATUS_RESTORING]).await?;
    cancel_selected_runs_with_pg(health_registry, pool, &target_run_ids, "auto_queue_cancel").await
}
