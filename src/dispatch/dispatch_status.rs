use anyhow::Result;
use serde_json::json;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use sqlite_test::OptionalExtension;
use sqlx::{PgPool, Row};

use crate::db::Db;
use crate::engine::PolicyEngine;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use super::dispatch_context::validate_dispatch_completion_evidence_on_conn;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use super::dispatch_query::query_dispatch_row;
use super::dispatch_query::query_dispatch_row_pg;

/// #750: Sources whose completion path already writes ✅ to the Discord
/// message via the command bot (turn_bridge / tmux watcher). For those, the
/// announce-bot sync would only bump the reaction count; skip the enqueue.
///
/// Non-live paths (api, recovery_*, supervisor_*, test_*, cli, etc.) bypass
/// the command bot entirely and need the announce-bot ✅ as the only
/// terminal-state signal on the original dispatch message.
fn transition_source_is_live_command_bot(transition_source: &str) -> bool {
    let src = transition_source.trim();
    src.starts_with("turn_bridge") || src.starts_with("watcher")
}

fn should_enqueue_status_reaction(to_status: &str, transition_source: &str) -> bool {
    match to_status {
        "failed" | "cancelled" => true,
        "completed" => !transition_source_is_live_command_bot(transition_source),
        _ => false,
    }
}

fn emit_dispatch_quality_event(
    dispatch_id: &str,
    agent_id: Option<&str>,
    card_id: Option<&str>,
    dispatch_type: Option<&str>,
    from_status: Option<&str>,
    to_status: &str,
    transition_source: &str,
    payload: Option<&serde_json::Value>,
) {
    let Some(event_type) = (match to_status {
        "dispatched" => Some("dispatch_dispatched"),
        "completed" => Some("dispatch_completed"),
        _ => None,
    }) else {
        return;
    };
    crate::services::observability::emit_agent_quality_event(
        crate::services::observability::AgentQualityEvent {
            source_event_id: Some(dispatch_id.to_string()),
            correlation_id: Some(dispatch_id.to_string()),
            agent_id: agent_id.map(str::to_string),
            provider: None,
            channel_id: None,
            card_id: card_id.map(str::to_string),
            dispatch_id: Some(dispatch_id.to_string()),
            event_type: event_type.to_string(),
            payload: json!({
                "dispatch_type": dispatch_type,
                "from_status": from_status,
                "to_status": to_status,
                "transition_source": transition_source,
                "payload": payload.cloned().unwrap_or_else(|| json!({})),
            }),
        },
    );
}

fn is_noop_completion_result(result: Option<&serde_json::Value>) -> bool {
    result.is_some_and(|value| {
        value.get("work_outcome").and_then(|entry| entry.as_str()) == Some("noop")
            || value
                .get("completed_without_changes")
                .and_then(|entry| entry.as_bool())
                == Some(true)
    })
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn auto_queue_review_disabled_for_dispatch_on_conn(
    conn: &sqlite_test::Connection,
    dispatch_id: &str,
) -> bool {
    conn.query_row(
        "SELECT COUNT(*) > 0
         FROM auto_queue_entries e
         JOIN auto_queue_runs r ON r.id = e.run_id
         WHERE e.dispatch_id = ?1
           AND r.status IN ('active', 'paused', 'completed')
           AND COALESCE(r.review_mode, 'enabled') = 'disabled'
           AND (
                e.status = 'dispatched'
                OR (
                    e.status = 'done'
                    AND COALESCE(
                        (SELECT status FROM task_dispatches WHERE id = e.dispatch_id),
                        ''
                    ) = 'completed'
                )
           )",
        [dispatch_id],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn restore_auto_queue_mainline_after_review_skip_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
    dispatch_id: &str,
) -> Result<()> {
    let (repo_id, agent_id): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| anyhow::anyhow!("load card scope for {card_id}: {error}"))?;
    let effective =
        crate::pipeline::resolve_for_card(conn, repo_id.as_deref(), agent_id.as_deref());
    let target_status = effective
        .kickoff_for(effective.initial_state())
        .filter(|status| status != "review")
        .unwrap_or_else(|| "in_progress".to_string());

    conn.execute(
        "DELETE FROM task_dispatches
         WHERE kanban_card_id = ?1
           AND dispatch_type IN ('review', 'review-decision')
           AND status IN ('pending', 'dispatched')",
        [card_id],
    )
    .map_err(|error| anyhow::anyhow!("delete skipped review dispatches for {card_id}: {error}"))?;
    let _ = conn.execute(
        "DELETE FROM card_review_state WHERE card_id = ?1",
        [card_id],
    );
    conn.execute(
        "UPDATE kanban_cards
         SET status = ?1,
             latest_dispatch_id = ?2,
             review_status = NULL,
             review_round = NULL,
             review_entered_at = NULL,
             awaiting_dod_at = NULL,
             blocked_reason = NULL,
             suggestion_pending_at = NULL,
             deferred_dod_json = NULL,
             updated_at = datetime('now')
         WHERE id = ?3",
        sqlite_test::params![target_status, dispatch_id, card_id],
    )
    .map_err(|error| anyhow::anyhow!("restore mainline card state for {card_id}: {error}"))?;
    Ok(())
}

async fn auto_queue_review_disabled_for_dispatch_on_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
) -> Result<bool> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
            SELECT 1
            FROM auto_queue_entries e
            JOIN auto_queue_runs r ON r.id = e.run_id
            WHERE e.dispatch_id = $1
              AND r.status IN ('active', 'paused', 'completed')
              AND COALESCE(r.review_mode, 'enabled') = 'disabled'
              AND (
                    e.status = 'dispatched'
                    OR (
                        e.status = 'done'
                        AND COALESCE(
                            (SELECT status FROM task_dispatches WHERE id = e.dispatch_id),
                            ''
                        ) = 'completed'
                    )
              )
        )",
    )
    .bind(dispatch_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!("load auto-queue review_mode for dispatch {dispatch_id}: {error}")
    })
}

async fn auto_queue_review_disabled_for_dispatch_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<bool> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
            SELECT 1
            FROM auto_queue_entries e
            JOIN auto_queue_runs r ON r.id = e.run_id
            WHERE e.dispatch_id = $1
              AND COALESCE(r.review_mode, 'enabled') = 'disabled'
              AND r.status IN ('active', 'paused', 'completed')
              AND (
                    e.status = 'dispatched'
                    OR (
                        e.status = 'done'
                        AND COALESCE(
                            (SELECT status FROM task_dispatches WHERE id = e.dispatch_id),
                            ''
                        ) = 'completed'
                    )
              )
        )",
    )
    .bind(dispatch_id)
    .fetch_one(pool)
    .await
    .map_err(|error| {
        anyhow::anyhow!("load auto-queue review_mode for dispatch {dispatch_id}: {error}")
    })
}

async fn restore_auto_queue_mainline_after_review_skip_on_pg(
    pool: &PgPool,
    card_id: &str,
    dispatch_id: &str,
) -> Result<()> {
    let row = sqlx::query(
        "SELECT repo_id, assigned_agent_id
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_one(pool)
    .await
    .map_err(|error| anyhow::anyhow!("load card scope for {card_id}: {error}"))?;
    let repo_id: Option<String> = row
        .try_get("repo_id")
        .map_err(|error| anyhow::anyhow!("decode repo_id for {card_id}: {error}"))?;
    let agent_id: Option<String> = row
        .try_get("assigned_agent_id")
        .map_err(|error| anyhow::anyhow!("decode assigned_agent_id for {card_id}: {error}"))?;
    let effective =
        crate::pipeline::resolve_for_card_pg(pool, repo_id.as_deref(), agent_id.as_deref()).await;
    let target_status = effective
        .kickoff_for(effective.initial_state())
        .filter(|status| status != "review")
        .unwrap_or_else(|| "in_progress".to_string());

    sqlx::query(
        "DELETE FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type IN ('review', 'review-decision')
           AND status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| anyhow::anyhow!("delete skipped review dispatches for {card_id}: {error}"))?;
    let _ = sqlx::query("DELETE FROM card_review_state WHERE card_id = $1")
        .bind(card_id)
        .execute(pool)
        .await;
    sqlx::query(
        "UPDATE kanban_cards
         SET status = $1,
             latest_dispatch_id = $2,
             review_status = NULL,
             review_round = NULL,
             review_entered_at = NULL,
             awaiting_dod_at = NULL,
             blocked_reason = NULL,
             suggestion_pending_at = NULL,
             deferred_dod_json = NULL,
             updated_at = NOW()
         WHERE id = $3",
    )
    .bind(&target_status)
    .bind(dispatch_id)
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| anyhow::anyhow!("restore mainline card state for {card_id}: {error}"))?;
    Ok(())
}

fn should_skip_auto_queue_terminal_sync(
    dispatch_type: Option<&str>,
    to_status: &str,
    result: Option<&serde_json::Value>,
    sync_auto_queue_terminal_entries: bool,
    auto_queue_review_disabled: bool,
) -> bool {
    if !sync_auto_queue_terminal_entries {
        return true;
    }

    if to_status != "completed" {
        return false;
    }

    match dispatch_type {
        Some("consultation") => true,
        Some("implementation" | "rework") => {
            is_noop_completion_result(result) && !auto_queue_review_disabled
        }
        _ => false,
    }
}

fn block_on_dispatch_pg<F, T>(
    pool: &PgPool,
    future_factory: impl FnOnce(PgPool) -> F + Send + 'static,
) -> Result<T>
where
    F: std::future::Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    crate::utils::async_bridge::block_on_pg_result(pool, future_factory, |error| {
        anyhow::anyhow!("{error}")
    })
}

async fn dispatch_exists_pg(pool: &PgPool, dispatch_id: &str) -> Result<bool> {
    sqlx::query_scalar::<_, bool>("SELECT COUNT(*) > 0 FROM task_dispatches WHERE id = $1")
        .bind(dispatch_id)
        .fetch_one(pool)
        .await
        .map_err(|error| {
            anyhow::anyhow!("postgres dispatch existence lookup {dispatch_id}: {error}")
        })
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn dispatch_exists_on_conn(conn: &sqlite_test::Connection, dispatch_id: &str) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM task_dispatches WHERE id = ?1)",
        [dispatch_id],
        |row| row.get(0),
    )
    .map_err(|error| anyhow::anyhow!("sqlite dispatch existence lookup {dispatch_id}: {error}"))
}

async fn validate_dispatch_completion_evidence_on_pg(
    pool: &PgPool,
    db: Option<&Db>,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<()> {
    let row = sqlx::query("SELECT dispatch_type, status FROM task_dispatches WHERE id = $1")
        .bind(dispatch_id)
        .fetch_one(pool)
        .await
        .map_err(|error| anyhow::anyhow!("Dispatch lookup error: {error}"))?;

    let dispatch_type: Option<String> = row
        .try_get("dispatch_type")
        .map_err(|error| anyhow::anyhow!("Dispatch lookup decode error: {error}"))?;
    let status: String = row
        .try_get("status")
        .map_err(|error| anyhow::anyhow!("Dispatch lookup decode error: {error}"))?;

    if !matches!(status.as_str(), "pending" | "dispatched")
        || !matches!(
            dispatch_type.as_deref(),
            Some("implementation") | Some("rework")
        )
    {
        return Ok(());
    }

    let result_has_work_completion_evidence = result
        .get("completed_commit")
        .and_then(|v| v.as_str())
        .is_some()
        || result
            .get("assistant_message")
            .and_then(|v| v.as_str())
            .is_some()
        || result
            .get("agent_response_present")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        || result
            .get("work_outcome")
            .and_then(|v| v.as_str())
            .is_some();

    if result_has_work_completion_evidence
        || crate::db::session_transcripts::dispatch_has_assistant_response_db(
            db,
            Some(pool),
            dispatch_id,
        )?
    {
        return Ok(());
    }

    let dispatch_label = dispatch_type.as_deref().unwrap_or("work");
    let completion_source = result
        .get("completion_source")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    tracing::warn!(
        "[dispatch] rejecting {} completion for {}: no agent execution evidence",
        dispatch_label,
        dispatch_id
    );
    Err(anyhow::anyhow!(
        "Cannot complete {dispatch_label} dispatch {dispatch_id} via {completion_source}: no agent execution evidence (expected assistant response, completed_commit, or explicit work_outcome)"
    ))
}

fn log_phase_gate_reconciliation(
    dispatch_id: &str,
    outcome: &crate::db::auto_queue::PhaseGateReconciliation,
) {
    use crate::db::auto_queue::PhaseGateReconciliation;
    match outcome {
        PhaseGateReconciliation::NoContext | PhaseGateReconciliation::StaleRow => {}
        PhaseGateReconciliation::AlreadyFailed => {
            tracing::debug!(
                dispatch_id,
                "[phase_gate] terminal dispatch already in failed gate state — leaving as-is"
            );
        }
        PhaseGateReconciliation::AwaitingSiblings {
            run_id,
            phase,
            pending_count,
        } => {
            tracing::info!(
                dispatch_id,
                run_id = %run_id,
                phase,
                pending_count,
                "[phase_gate] dispatch passed; awaiting sibling gate dispatches"
            );
        }
        PhaseGateReconciliation::MarkedFailed {
            run_id,
            phase,
            failed_dispatch_id,
            failed_reason,
        } => {
            tracing::warn!(
                dispatch_id,
                run_id = %run_id,
                phase,
                failed_dispatch_id = %failed_dispatch_id,
                failed_reason = %failed_reason,
                "[phase_gate] durable reconciliation marked phase gate failed"
            );
        }
        PhaseGateReconciliation::Cleared {
            run_id,
            phase,
            next_phase,
            final_phase,
            run_resumed,
            run_finalized,
        } => {
            tracing::info!(
                dispatch_id,
                run_id = %run_id,
                phase,
                next_phase = ?next_phase,
                final_phase,
                run_resumed,
                run_finalized,
                "[phase_gate] durable reconciliation cleared phase gate"
            );
        }
    }
}

async fn set_dispatch_status_on_pg_with_sync(
    pool: &PgPool,
    dispatch_id: &str,
    to_status: &str,
    result: Option<&serde_json::Value>,
    transition_source: &str,
    allowed_from: Option<&[&str]>,
    touch_completed_at: bool,
    sync_auto_queue_terminal_entries: bool,
    assume_external_phase_gate_lifecycle: bool,
) -> Result<usize> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| anyhow::anyhow!("begin postgres dispatch status tx: {error}"))?;

    let current = sqlx::query(
        "SELECT status, kanban_card_id, to_agent_id, dispatch_type,
                context::TEXT AS context_text,
                result::TEXT  AS persisted_result_text
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| anyhow::anyhow!("load postgres dispatch {dispatch_id}: {error}"))?;
    let Some(current) = current else {
        tx.rollback()
            .await
            .map_err(|error| anyhow::anyhow!("rollback postgres dispatch status tx: {error}"))?;
        return Ok(0);
    };

    let current_status = current
        .try_get::<Option<String>, _>("status")
        .map_err(|error| {
            anyhow::anyhow!("decode postgres dispatch status for {dispatch_id}: {error}")
        })?
        .unwrap_or_default();

    if let Some(allowed_from) = allowed_from {
        if !allowed_from
            .iter()
            .any(|status| *status == current_status.as_str())
        {
            tx.rollback().await.map_err(|error| {
                anyhow::anyhow!("rollback postgres dispatch status tx: {error}")
            })?;
            return Ok(0);
        }
    }

    let result_json = result.map(|value| value.to_string());
    let changed = match (result_json.as_deref(), touch_completed_at) {
        (Some(result_json), true) => sqlx::query(
            "UPDATE task_dispatches
             SET status = $1,
                 result = CAST($2 AS jsonb),
                 updated_at = NOW(),
                 completed_at = CASE
                     WHEN $1 = 'completed' THEN COALESCE(completed_at, NOW())
                     ELSE completed_at
                 END
             WHERE id = $3
               AND status = $4",
        )
        .bind(to_status)
        .bind(result_json)
        .bind(dispatch_id)
        .bind(&current_status)
        .execute(&mut *tx)
        .await
        .map_err(|error| anyhow::anyhow!("update postgres dispatch {dispatch_id}: {error}"))?
        .rows_affected() as usize,
        (Some(result_json), false) => sqlx::query(
            "UPDATE task_dispatches
             SET status = $1,
                 result = CAST($2 AS jsonb),
                 updated_at = NOW()
             WHERE id = $3
               AND status = $4",
        )
        .bind(to_status)
        .bind(result_json)
        .bind(dispatch_id)
        .bind(&current_status)
        .execute(&mut *tx)
        .await
        .map_err(|error| anyhow::anyhow!("update postgres dispatch {dispatch_id}: {error}"))?
        .rows_affected() as usize,
        (None, true) => sqlx::query(
            "UPDATE task_dispatches
             SET status = $1,
                 updated_at = NOW(),
                 completed_at = CASE
                     WHEN $1 = 'completed' THEN COALESCE(completed_at, NOW())
                     ELSE completed_at
                 END
             WHERE id = $2
               AND status = $3",
        )
        .bind(to_status)
        .bind(dispatch_id)
        .bind(&current_status)
        .execute(&mut *tx)
        .await
        .map_err(|error| anyhow::anyhow!("update postgres dispatch {dispatch_id}: {error}"))?
        .rows_affected() as usize,
        (None, false) => sqlx::query(
            "UPDATE task_dispatches
             SET status = $1,
                 updated_at = NOW()
             WHERE id = $2
               AND status = $3",
        )
        .bind(to_status)
        .bind(dispatch_id)
        .bind(&current_status)
        .execute(&mut *tx)
        .await
        .map_err(|error| anyhow::anyhow!("update postgres dispatch {dispatch_id}: {error}"))?
        .rows_affected() as usize,
    };

    if changed > 0 && current_status != to_status {
        let kanban_card_id = current
            .try_get::<Option<String>, _>("kanban_card_id")
            .map_err(|error| {
                anyhow::anyhow!("decode postgres kanban_card_id for {dispatch_id}: {error}")
            })?;
        let agent_id = current
            .try_get::<Option<String>, _>("to_agent_id")
            .map_err(|error| {
                anyhow::anyhow!("decode postgres to_agent_id for {dispatch_id}: {error}")
            })?;
        let dispatch_type = current
            .try_get::<Option<String>, _>("dispatch_type")
            .map_err(|error| {
                anyhow::anyhow!("decode postgres dispatch_type for {dispatch_id}: {error}")
            })?;

        sqlx::query(
            "INSERT INTO dispatch_events (
                dispatch_id,
                kanban_card_id,
                dispatch_type,
                from_status,
                to_status,
                transition_source,
                payload_json
            ) VALUES ($1, $2, $3, $4, $5, $6, CAST($7 AS jsonb))",
        )
        .bind(dispatch_id)
        .bind(&kanban_card_id)
        .bind(&dispatch_type)
        .bind(&current_status)
        .bind(to_status)
        .bind(transition_source)
        .bind(result_json.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!("insert postgres dispatch event for {dispatch_id}: {error}")
        })?;
        crate::services::observability::emit_dispatch_result(
            dispatch_id,
            kanban_card_id.as_deref(),
            dispatch_type.as_deref(),
            Some(&current_status),
            to_status,
            transition_source,
            result_json
                .as_ref()
                .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
                .as_ref(),
        );
        emit_dispatch_quality_event(
            dispatch_id,
            agent_id.as_deref(),
            kanban_card_id.as_deref(),
            dispatch_type.as_deref(),
            Some(&current_status),
            to_status,
            transition_source,
            result_json
                .as_ref()
                .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
                .as_ref(),
        );

        if should_enqueue_status_reaction(to_status, transition_source) {
            sqlx::query(
                "INSERT INTO dispatch_outbox (dispatch_id, action)
                 SELECT $1, 'status_reaction'
                 WHERE NOT EXISTS (
                     SELECT 1
                     FROM dispatch_outbox
                     WHERE dispatch_id = $1
                       AND action = 'status_reaction'
                       AND status IN ('pending', 'processing')
                 )",
            )
            .bind(dispatch_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                anyhow::anyhow!("enqueue postgres status_reaction for {dispatch_id}: {error}")
            })?;
        }

        // Sync any auto_queue_entry bound to this dispatch when the dispatch
        // reaches a terminal status. The card-terminal SyncAutoQueue intent
        // (transition.rs) only fires when the *card* goes terminal — but an
        // implementation dispatch typically completes into `review`, leaving
        // the entry stuck at `dispatched` until the card eventually reaches
        // `done`. Mirror dispatch terminal here so the slot frees promptly.
        let auto_queue_review_disabled =
            if matches!(dispatch_type.as_deref(), Some("implementation" | "rework")) {
                auto_queue_review_disabled_for_dispatch_on_pg(&mut tx, dispatch_id).await?
            } else {
                false
            };
        let skip_auto_queue_terminal_sync = should_skip_auto_queue_terminal_sync(
            dispatch_type.as_deref(),
            to_status,
            result,
            sync_auto_queue_terminal_entries,
            auto_queue_review_disabled,
        );
        if matches!(to_status, "completed" | "failed" | "cancelled")
            && !skip_auto_queue_terminal_sync
        {
            match to_status {
                "completed" => {
                    crate::db::auto_queue::finalize_completed_dispatch_terminal_entry_on_pg_tx(
                        &mut tx,
                        dispatch_id,
                        transition_source,
                        true,
                    )
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "finalize auto_queue entry on dispatch completion {dispatch_id}: {error}"
                        )
                    })?;
                }
                "failed" | "cancelled" => {
                    let entry_status = if to_status == "failed" {
                        crate::db::auto_queue::ENTRY_STATUS_FAILED
                    } else {
                        crate::db::auto_queue::ENTRY_STATUS_SKIPPED
                    };
                    crate::db::auto_queue::sync_dispatch_terminal_entries_on_pg_tx(
                        &mut tx,
                        dispatch_id,
                        entry_status,
                        transition_source,
                        true,
                    )
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "sync auto_queue_entries on dispatch terminal {dispatch_id}: {error}"
                        )
                    })?;
                }
                _ => {}
            }
        }

        if matches!(to_status, "completed" | "failed" | "cancelled")
            && !assume_external_phase_gate_lifecycle
        {
            // #1980: phase-gate reconciliation in the durable Postgres path so
            // sidecar gate dispatches are cleared/marked-failed even when the
            // JS `onDispatchCompleted` hook does not fire (CRUD route, recovery
            // helpers, etc. flow through this path). The
            // `assume_external_phase_gate_lifecycle` flag is set by callers
            // (notably `complete_dispatch_inner_with_backends`) that will fire
            // the JS policy hook themselves — those callers own the gate-row
            // lifecycle plus the run finalize/activate side effects, and we
            // must not pre-empt them by deleting the row here.
            let context_text = current
                .try_get::<Option<String>, _>("context_text")
                .map_err(|error| {
                    anyhow::anyhow!("decode postgres dispatch context for {dispatch_id}: {error}")
                })?;
            // Caller-supplied result wins; fall back to whatever was persisted
            // on the dispatch row so status-only completion writes (CRUD route,
            // legacy callers) reuse the verdict that produced the original
            // result instead of looking like an empty-result failure.
            let persisted_result_text = current
                .try_get::<Option<String>, _>("persisted_result_text")
                .map_err(|error| {
                    anyhow::anyhow!("decode postgres dispatch result for {dispatch_id}: {error}")
                })?;
            let result_text = result_json.clone().or(persisted_result_text);
            let outcome =
                crate::db::auto_queue::reconcile_phase_gate_for_terminal_dispatch_on_pg_tx(
                    &mut tx,
                    dispatch_id,
                    to_status,
                    context_text.as_deref(),
                    result_text.as_deref(),
                )
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "reconcile phase-gate for terminal dispatch {dispatch_id}: {error}"
                    )
                })?;
            log_phase_gate_reconciliation(dispatch_id, &outcome);
        }

        if matches!(to_status, "completed" | "failed" | "cancelled") {
            crate::db::dispatch_semaphores::release_dispatch_semaphores_on_pg_tx(
                &mut tx,
                dispatch_id,
            )
            .await
            .map_err(|error| {
                anyhow::anyhow!("release postgres dispatch semaphores for {dispatch_id}: {error}")
            })?;

            let session_info = format!("Dispatch {to_status}");
            let cleared = sqlx::query(
                "UPDATE sessions
                 SET status = CASE
                         WHEN status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working') THEN 'idle'
                         ELSE status
                     END,
                     active_dispatch_id = NULL,
                     session_info = $1,
                     last_heartbeat = NOW()
                 WHERE active_dispatch_id = $2",
            )
            .bind(&session_info)
            .bind(dispatch_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                anyhow::anyhow!("clear postgres session dispatch link {dispatch_id}: {error}")
            })?
            .rows_affected();
            if cleared > 0 {
                tracing::info!(
                    "[dispatch] cleared {} stale session link(s) for terminal dispatch {} ({})",
                    cleared,
                    dispatch_id,
                    to_status
                );
            }
        }
    }

    tx.commit()
        .await
        .map_err(|error| anyhow::anyhow!("commit postgres dispatch status tx: {error}"))?;
    if changed > 0 && matches!(to_status, "completed" | "failed" | "cancelled") {
        crate::services::dispatches::wait_queue::spawn_cached_constraint_release_wake(
            pool.clone(),
            "constraint_release",
            dispatch_id.to_string(),
            "dispatch_terminal_status",
        );
    }
    Ok(changed)
}

async fn set_dispatch_status_on_pg(
    pool: &PgPool,
    dispatch_id: &str,
    to_status: &str,
    result: Option<&serde_json::Value>,
    transition_source: &str,
    allowed_from: Option<&[&str]>,
    touch_completed_at: bool,
) -> Result<usize> {
    set_dispatch_status_on_pg_with_sync(
        pool,
        dispatch_id,
        to_status,
        result,
        transition_source,
        allowed_from,
        touch_completed_at,
        true,
        false,
    )
    .await
}

/// Variant for callers that will themselves invoke the `OnDispatchCompleted`
/// JS policy hook (currently `complete_dispatch_inner_with_backends`). The JS
/// hook owns the phase-gate row lifecycle plus the run finalize/activate side
/// effects after a passing gate, so the durable Rust path must NOT clear the
/// gate row beneath it. CRUD/recovery callers that bypass the hook should keep
/// using `set_dispatch_status_on_pg`.
async fn set_dispatch_status_on_pg_with_external_phase_gate(
    pool: &PgPool,
    dispatch_id: &str,
    to_status: &str,
    result: Option<&serde_json::Value>,
    transition_source: &str,
    allowed_from: Option<&[&str]>,
    touch_completed_at: bool,
) -> Result<usize> {
    set_dispatch_status_on_pg_with_sync(
        pool,
        dispatch_id,
        to_status,
        result,
        transition_source,
        allowed_from,
        touch_completed_at,
        true,
        true,
    )
    .await
}

async fn card_needs_review_dispatch_pg(pool: &PgPool, card_id: &str) -> Result<bool> {
    let row = sqlx::query(
        "SELECT status, repo_id, assigned_agent_id
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        anyhow::anyhow!("load postgres card {card_id} for review redispatch: {error}")
    })?;
    let Some(row) = row else {
        return Ok(false);
    };

    let card_status: Option<String> = row
        .try_get("status")
        .map_err(|error| anyhow::anyhow!("decode status for {card_id}: {error}"))?;
    let repo_id: Option<String> = row
        .try_get("repo_id")
        .map_err(|error| anyhow::anyhow!("decode repo_id for {card_id}: {error}"))?;
    let agent_id: Option<String> = row
        .try_get("assigned_agent_id")
        .map_err(|error| anyhow::anyhow!("decode assigned_agent_id for {card_id}: {error}"))?;

    let has_review_dispatch = sqlx::query_scalar::<_, bool>(
        "SELECT COUNT(*) > 0
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type IN ('review', 'review-decision')
           AND status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .fetch_one(pool)
    .await
    .map_err(|error| anyhow::anyhow!("load review dispatch gate for {card_id}: {error}"))?;
    let has_active_work = sqlx::query_scalar::<_, bool>(
        "SELECT COUNT(*) > 0
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type IN ('implementation', 'rework')
           AND status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .fetch_one(pool)
    .await
    .map_err(|error| anyhow::anyhow!("load active work gate for {card_id}: {error}"))?;

    let Some(card_status) = card_status else {
        return Ok(false);
    };
    let effective =
        crate::pipeline::resolve_for_card_pg(pool, repo_id.as_deref(), agent_id.as_deref()).await;
    let is_review_state = effective
        .hooks_for_state(&card_status)
        .is_some_and(|hooks| hooks.on_enter.iter().any(|name| name == "OnReviewEnter"));

    Ok(is_review_state && !has_review_dispatch && !has_active_work)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn card_needs_review_dispatch_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> Result<bool> {
    let row: Option<(Option<String>, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT status, repo_id, assigned_agent_id
             FROM kanban_cards
             WHERE id = ?1",
            [card_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| {
            anyhow::anyhow!("load sqlite card {card_id} for review redispatch: {error}")
        })?;
    let Some((card_status, repo_id, agent_id)) = row else {
        return Ok(false);
    };

    let has_review_dispatch: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0
             FROM task_dispatches
             WHERE kanban_card_id = ?1
               AND dispatch_type IN ('review', 'review-decision')
               AND status IN ('pending', 'dispatched')",
            [card_id],
            |row| row.get(0),
        )
        .map_err(|error| {
            anyhow::anyhow!("load sqlite review dispatch gate for {card_id}: {error}")
        })?;
    let has_active_work: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0
             FROM task_dispatches
             WHERE kanban_card_id = ?1
               AND dispatch_type IN ('implementation', 'rework')
               AND status IN ('pending', 'dispatched')",
            [card_id],
            |row| row.get(0),
        )
        .map_err(|error| anyhow::anyhow!("load sqlite active work gate for {card_id}: {error}"))?;

    let Some(card_status) = card_status else {
        return Ok(false);
    };
    let effective =
        crate::pipeline::resolve_for_card(conn, repo_id.as_deref(), agent_id.as_deref());
    let is_review_state = effective
        .hooks_for_state(&card_status)
        .is_some_and(|hooks| hooks.on_enter.iter().any(|name| name == "OnReviewEnter"));

    Ok(is_review_state && !has_review_dispatch && !has_active_work)
}

async fn maybe_inject_phase_gate_verdict_pg(
    pool: &PgPool,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Option<serde_json::Value> {
    let context_raw = sqlx::query_scalar::<_, Option<String>>(
        "SELECT context FROM task_dispatches WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .flatten()?;
    let ctx = serde_json::from_str::<serde_json::Value>(&context_raw).ok()?;
    let phase_gate_ctx = ctx.get("phase_gate").and_then(|v| v.as_object())?;
    infer_phase_gate_verdict(dispatch_id, phase_gate_ctx, result)
}

/// Ensure a durable notify outbox row exists for a dispatch.
///
/// Used both by the authoritative dispatch creation transaction and by
/// fallback/backfill paths that must avoid duplicate notify entries.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn ensure_dispatch_notify_outbox_on_conn(
    conn: &sqlite_test::Connection,
    dispatch_id: &str,
    agent_id: &str,
    card_id: &str,
    title: &str,
) -> sqlite_test::Result<bool> {
    conn.execute_batch("SAVEPOINT dispatch_notify_outbox")?;
    let result = (|| -> sqlite_test::Result<bool> {
        let dispatch_status: Option<String> = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get(0),
            )
            .optional()?;
        if matches!(
            dispatch_status.as_deref(),
            Some("completed") | Some("failed") | Some("cancelled")
        ) {
            return Ok(false);
        }

        let inserted = conn.execute(
            "INSERT OR IGNORE INTO dispatch_outbox (dispatch_id, action, agent_id, card_id, title) \
             VALUES (?1, 'notify', ?2, ?3, ?4)",
            sqlite_test::params![dispatch_id, agent_id, card_id, title],
        )?;
        Ok(inserted > 0)
    })();
    match result {
        Ok(value) => {
            conn.execute_batch("RELEASE dispatch_notify_outbox")?;
            Ok(value)
        }
        Err(err) => {
            let _ = conn.execute_batch(
                "ROLLBACK TO dispatch_notify_outbox; RELEASE dispatch_notify_outbox;",
            );
            Err(err)
        }
    }
}

/// Ensure a pending status-reaction outbox row exists for a dispatch.
///
/// At most one in-flight status sync is needed: when the worker drains it, the
/// Discord side-effect reads the latest dispatch status from `task_dispatches`.
/// Once an older row is already `done` or `failed`, a later transition should
/// enqueue a fresh row.
///
/// #750: announce bot no longer writes ✅ on completed dispatches (command
/// bot's turn-lifecycle ✅ is the single source of truth for success). The
/// announce-bot path is preserved ONLY to write ❌ on failed/cancelled
/// dispatches, because command bot's turn_bridge unconditionally adds ✅ when
/// a response was delivered (see turn_bridge/mod.rs:1537) — a failed dispatch
/// that returned any text would otherwise show a false green check. This
/// enqueue is also the only repair path for status transitions that bypass
/// turn_bridge entirely (queue/API cancellation, orphan recovery).
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn ensure_dispatch_status_reaction_outbox_on_conn(
    conn: &sqlite_test::Connection,
    dispatch_id: &str,
) -> sqlite_test::Result<bool> {
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0
         FROM dispatch_outbox
         WHERE dispatch_id = ?1
           AND action = 'status_reaction'
           AND status IN ('pending', 'processing')",
        [dispatch_id],
        |row| row.get(0),
    )?;
    if exists {
        return Ok(false);
    }
    conn.execute(
        "INSERT INTO dispatch_outbox (dispatch_id, action) VALUES (?1, 'status_reaction')",
        [dispatch_id],
    )?;
    Ok(true)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn record_dispatch_status_event_on_conn(
    conn: &sqlite_test::Connection,
    dispatch_id: &str,
    from_status: Option<&str>,
    to_status: &str,
    transition_source: &str,
    payload: Option<&serde_json::Value>,
) -> sqlite_test::Result<()> {
    let (kanban_card_id, agent_id, dispatch_type): (
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT kanban_card_id, to_agent_id, dispatch_type FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
        .unwrap_or((None, None, None));

    conn.execute(
        "INSERT INTO dispatch_events (
            dispatch_id,
            kanban_card_id,
            dispatch_type,
            from_status,
            to_status,
            transition_source,
            payload_json
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        sqlite_test::params![
            dispatch_id,
            kanban_card_id,
            dispatch_type,
            from_status,
            to_status,
            transition_source,
            payload.map(|value| value.to_string()),
        ],
    )?;
    crate::services::observability::emit_dispatch_result(
        dispatch_id,
        kanban_card_id.as_deref(),
        dispatch_type.as_deref(),
        from_status,
        to_status,
        transition_source,
        payload,
    );
    emit_dispatch_quality_event(
        dispatch_id,
        agent_id.as_deref(),
        kanban_card_id.as_deref(),
        dispatch_type.as_deref(),
        from_status,
        to_status,
        transition_source,
        payload,
    );
    Ok(())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn set_dispatch_status_on_conn_with_sync(
    conn: &sqlite_test::Connection,
    dispatch_id: &str,
    to_status: &str,
    result: Option<&serde_json::Value>,
    transition_source: &str,
    allowed_from: Option<&[&str]>,
    touch_completed_at: bool,
    sync_auto_queue_terminal_entries: bool,
) -> Result<usize> {
    let current_row: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT status, dispatch_type FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((current_status, dispatch_type)) = current_row else {
        return Ok(0);
    };

    if let Some(allowed_from) = allowed_from {
        if !allowed_from
            .iter()
            .any(|status| *status == current_status.as_str())
        {
            return Ok(0);
        }
    }

    conn.execute_batch("SAVEPOINT dispatch_status_transition")?;
    let update_result = (|| -> Result<usize> {
        let changed = match (result, touch_completed_at) {
            (Some(result), true) => conn.execute(
                "UPDATE task_dispatches
                 SET status = ?1,
                     result = ?2,
                     updated_at = datetime('now'),
                     completed_at = CASE
                         WHEN ?1 = 'completed' THEN COALESCE(completed_at, datetime('now'))
                         ELSE completed_at
                     END
                 WHERE id = ?3 AND status = ?4",
                sqlite_test::params![to_status, result.to_string(), dispatch_id, current_status],
            )?,
            (Some(result), false) => conn.execute(
                "UPDATE task_dispatches
                 SET status = ?1,
                     result = ?2,
                     updated_at = datetime('now')
                 WHERE id = ?3 AND status = ?4",
                sqlite_test::params![to_status, result.to_string(), dispatch_id, current_status],
            )?,
            (None, true) => conn.execute(
                "UPDATE task_dispatches
                 SET status = ?1,
                     updated_at = datetime('now'),
                     completed_at = CASE
                         WHEN ?1 = 'completed' THEN COALESCE(completed_at, datetime('now'))
                         ELSE completed_at
                     END
                 WHERE id = ?2 AND status = ?3",
                sqlite_test::params![to_status, dispatch_id, current_status],
            )?,
            (None, false) => conn.execute(
                "UPDATE task_dispatches
                 SET status = ?1,
                     updated_at = datetime('now')
                 WHERE id = ?2 AND status = ?3",
                sqlite_test::params![to_status, dispatch_id, current_status],
            )?,
        };

        if changed > 0 && current_status != to_status {
            record_dispatch_status_event_on_conn(
                conn,
                dispatch_id,
                Some(current_status.as_str()),
                to_status,
                transition_source,
                result,
            )?;

            // #750: narrowed enqueue — the announce-bot reaction sync now runs
            // only when it actually has something to write:
            // - 'failed' / 'cancelled': always. Command bot's turn_bridge
            //   unconditionally adds ✅ when a response is delivered, so the
            //   announce-bot sync has to clean that ✅ and add ❌. Also covers
            //   queue/API cancellation + orphan recovery which bypass
            //   turn_bridge entirely.
            // - 'completed': only when the completion path is NOT the command
            //   bot's live reaction path. turn_bridge / tmux watcher already
            //   added ✅ on response delivery; re-adding it via the announce
            //   bot would just bump the reaction count. For non-live paths
            //   (api, recovery, supervisor orphan) the announce-bot sync is
            //   the ONLY source of the terminal ✅.
            // - pending / dispatched: skipped. Command bot is now the single
            //   source of ⏳ (see should_add_turn_pending_reaction).
            let enqueue = match to_status {
                "failed" | "cancelled" => true,
                "completed" => !transition_source_is_live_command_bot(transition_source),
                _ => false,
            };
            if enqueue {
                ensure_dispatch_status_reaction_outbox_on_conn(conn, dispatch_id)?;
            }

            // Sync any auto_queue_entry bound to this dispatch when the
            // dispatch reaches a terminal status. See PG twin in
            // set_dispatch_status_on_pg for the rationale.
            let auto_queue_review_disabled =
                matches!(dispatch_type.as_deref(), Some("implementation" | "rework"))
                    && auto_queue_review_disabled_for_dispatch_on_conn(conn, dispatch_id);
            let skip_auto_queue_terminal_sync = should_skip_auto_queue_terminal_sync(
                dispatch_type.as_deref(),
                to_status,
                result,
                sync_auto_queue_terminal_entries,
                auto_queue_review_disabled,
            );
            if matches!(to_status, "completed" | "failed" | "cancelled")
                && !skip_auto_queue_terminal_sync
            {
                let entry_status = match to_status {
                    "completed" => crate::db::auto_queue::ENTRY_STATUS_DONE,
                    "failed" => crate::db::auto_queue::ENTRY_STATUS_FAILED,
                    _ => crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
                };
                let completed_at_sql = if entry_status == crate::db::auto_queue::ENTRY_STATUS_DONE {
                    "COALESCE(completed_at, datetime('now'))"
                } else {
                    "datetime('now')"
                };
                conn.execute(
                    &format!(
                        "UPDATE auto_queue_entries
                         SET status = ?1,
                             completed_at = {completed_at_sql}
                         WHERE dispatch_id = ?2 AND status = 'dispatched'"
                    ),
                    sqlite_test::params![entry_status, dispatch_id],
                )?;
                let _ = transition_source;
            }

            if matches!(to_status, "completed" | "failed" | "cancelled") {
                let session_info = format!("Dispatch {to_status}");
                conn.execute(
                    "UPDATE sessions
                     SET status = CASE
                             WHEN status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working') THEN 'idle'
                             ELSE status
                         END,
                         active_dispatch_id = NULL,
                         session_info = ?1,
                         last_heartbeat = datetime('now')
                     WHERE active_dispatch_id = ?2",
                    sqlite_test::params![session_info, dispatch_id],
                )?;
            }
        }
        Ok(changed)
    })();

    match update_result {
        Ok(changed) => {
            conn.execute_batch("RELEASE dispatch_status_transition")?;
            Ok(changed)
        }
        Err(err) => {
            let _ = conn.execute_batch(
                "ROLLBACK TO dispatch_status_transition;
                 RELEASE dispatch_status_transition;",
            );
            Err(err)
        }
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn set_dispatch_status_on_conn(
    conn: &sqlite_test::Connection,
    dispatch_id: &str,
    to_status: &str,
    result: Option<&serde_json::Value>,
    transition_source: &str,
    allowed_from: Option<&[&str]>,
    touch_completed_at: bool,
) -> Result<usize> {
    set_dispatch_status_on_conn_with_sync(
        conn,
        dispatch_id,
        to_status,
        result,
        transition_source,
        allowed_from,
        touch_completed_at,
        true,
    )
}

/// Single authority for dispatch completion.
///
/// All dispatch completion paths — turn_bridge explicit, recovery, API PATCH,
/// session idle — MUST route through this function.  It performs:
///   1. DB status update  (task_dispatches → completed)
///   2. OnDispatchCompleted hook firing  (pipeline event hooks)
///   3. Side-effect draining  (intents, transitions, follow-up dispatches)
///   4. Safety-net re-fire of OnReviewEnter (#139)
pub fn finalize_dispatch(
    db: &Db,
    engine: &PolicyEngine,
    dispatch_id: &str,
    completion_source: &str,
    context: Option<&serde_json::Value>,
) -> Result<serde_json::Value> {
    finalize_dispatch_with_backends(Some(db), engine, dispatch_id, completion_source, context)
}

pub fn finalize_dispatch_with_backends(
    db: Option<&Db>,
    engine: &PolicyEngine,
    dispatch_id: &str,
    completion_source: &str,
    context: Option<&serde_json::Value>,
) -> Result<serde_json::Value> {
    let result = match context {
        Some(ctx) => {
            let mut merged = ctx.clone();
            if let Some(obj) = merged.as_object_mut() {
                obj.insert(
                    "completion_source".to_string(),
                    serde_json::Value::String(completion_source.to_string()),
                );
            }
            merged
        }
        None => json!({ "completion_source": completion_source }),
    };
    complete_dispatch_inner_with_backends(db, engine, dispatch_id, &result)
}

/// #143: DB-only dispatch completion — marks status='completed' without firing hooks.
///
/// Used by specialized paths (review_verdict, pm-decision) that fire their own
/// domain-specific hooks instead of the generic OnDispatchCompleted.
/// Returns the number of rows updated (0 = already completed/cancelled/not found).
pub fn mark_dispatch_completed_pg_first(
    db: &Db,
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<usize> {
    set_dispatch_status_pg_first(
        db,
        pg_pool,
        dispatch_id,
        "completed",
        Some(result),
        "mark_dispatch_completed",
        Some(&["pending", "dispatched"]),
        true,
    )
}

pub fn set_dispatch_status_pg_first(
    db: &Db,
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
    to_status: &str,
    result: Option<&serde_json::Value>,
    transition_source: &str,
    allowed_from: Option<&[&str]>,
    touch_completed_at: bool,
) -> Result<usize> {
    set_dispatch_status_with_backends(
        Some(db),
        pg_pool,
        dispatch_id,
        to_status,
        result,
        transition_source,
        allowed_from,
        touch_completed_at,
    )
}

pub fn set_dispatch_status_with_backends(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
    to_status: &str,
    result: Option<&serde_json::Value>,
    transition_source: &str,
    allowed_from: Option<&[&str]>,
    touch_completed_at: bool,
) -> Result<usize> {
    set_dispatch_status_with_backends_and_sync(
        db,
        pg_pool,
        dispatch_id,
        to_status,
        result,
        transition_source,
        allowed_from,
        touch_completed_at,
        true,
    )
}

/// #2045 Finding 4 (P0): async-friendly entry point for callers that already
/// run inside a tokio runtime (force-kill API, future axum handlers). The sync
/// `set_dispatch_status_with_backends` would otherwise try to call
/// `block_on_pg_result`, which panics when invoked from a multi-threaded
/// runtime. This wrapper runs the same canonical cleanup pipeline asynchronously
/// so callers get the full set of side effects (semaphore release, auto_queue
/// reconcile, phase-gate reconcile, sessions.active_dispatch_id clear,
/// observability emit, wait-queue wake).
pub async fn set_dispatch_status_on_pg_async(
    pool: &PgPool,
    dispatch_id: &str,
    to_status: &str,
    result: Option<&serde_json::Value>,
    transition_source: &str,
    allowed_from: Option<&[&str]>,
    touch_completed_at: bool,
) -> Result<usize> {
    set_dispatch_status_on_pg_with_sync(
        pool,
        dispatch_id,
        to_status,
        result,
        transition_source,
        allowed_from,
        touch_completed_at,
        true,
        false,
    )
    .await
}

fn set_dispatch_status_with_backends_and_sync(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
    to_status: &str,
    result: Option<&serde_json::Value>,
    transition_source: &str,
    allowed_from: Option<&[&str]>,
    touch_completed_at: bool,
    sync_auto_queue_terminal_entries: bool,
) -> Result<usize> {
    let Some(pool) = pg_pool else {
        #[cfg(all(test, feature = "legacy-sqlite-tests"))]
        if let Some(db) = db {
            let conn = db
                .lock()
                .map_err(|error| anyhow::anyhow!("legacy db lock poisoned: {error}"))?;
            return set_dispatch_status_on_conn_with_sync(
                &conn,
                dispatch_id,
                to_status,
                result,
                transition_source,
                allowed_from,
                touch_completed_at,
                sync_auto_queue_terminal_entries,
            );
        }
        let _ = db;
        return Err(anyhow::anyhow!(
            "Postgres pool required to set dispatch status for {dispatch_id}"
        ));
    };
    let dispatch_id = dispatch_id.to_string();
    let to_status = to_status.to_string();
    let transition_source = transition_source.to_string();
    let result_owned = result.cloned();
    let allowed_from_owned = allowed_from.map(|statuses| {
        statuses
            .iter()
            .map(|status| (*status).to_string())
            .collect::<Vec<_>>()
    });
    block_on_dispatch_pg(pool, move |pool| async move {
        let allowed_from_refs = allowed_from_owned
            .as_ref()
            .map(|statuses| statuses.iter().map(String::as_str).collect::<Vec<_>>());
        set_dispatch_status_on_pg_with_sync(
            &pool,
            &dispatch_id,
            &to_status,
            result_owned.as_ref(),
            &transition_source,
            allowed_from_refs.as_deref(),
            touch_completed_at,
            sync_auto_queue_terminal_entries,
            // The legacy `set_dispatch_status_with_backends*` family is used by
            // a wide variety of bypass callers (CRUD route, transition_executor,
            // supervisor, dispatch_cancel). None of them fire the JS hook, so
            // we always own the gate-row reconciliation here.
            false,
        )
        .await
    })
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn set_dispatch_status_sqlite_for_tests(
    db: &Db,
    dispatch_id: &str,
    to_status: &str,
    result: Option<&serde_json::Value>,
    allowed_from: Option<&[&str]>,
    touch_completed_at: bool,
) -> Result<usize> {
    let conn = db
        .separate_conn()
        .map_err(|error| anyhow::anyhow!("open sqlite dispatch status connection: {error}"))?;
    let current_status = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(current_status) = current_status else {
        return Ok(0);
    };
    if let Some(allowed_from) = allowed_from
        && !allowed_from
            .iter()
            .any(|allowed| *allowed == current_status.as_str())
    {
        return Ok(0);
    }

    let result_json = result.map(serde_json::Value::to_string);
    let changed = if touch_completed_at {
        conn.execute(
            "UPDATE task_dispatches
             SET status = ?1,
                 result = COALESCE(?2, result),
                 updated_at = datetime('now'),
                 completed_at = CASE WHEN ?1 = 'completed' THEN datetime('now') ELSE completed_at END
             WHERE id = ?3",
            sqlite_test::params![to_status, result_json, dispatch_id],
        )?
    } else {
        conn.execute(
            "UPDATE task_dispatches
             SET status = ?1,
                 result = COALESCE(?2, result),
                 updated_at = datetime('now')
             WHERE id = ?3",
            sqlite_test::params![to_status, result_json, dispatch_id],
        )?
    };
    Ok(changed)
}

pub(crate) fn set_dispatch_status_without_queue_sync_with_backends(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
    to_status: &str,
    result: Option<&serde_json::Value>,
    transition_source: &str,
    allowed_from: Option<&[&str]>,
    touch_completed_at: bool,
) -> Result<usize> {
    set_dispatch_status_with_backends_and_sync(
        db,
        pg_pool,
        dispatch_id,
        to_status,
        result,
        transition_source,
        allowed_from,
        touch_completed_at,
        false,
    )
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn set_dispatch_status_without_queue_sync_on_conn(
    conn: &sqlite_test::Connection,
    dispatch_id: &str,
    to_status: &str,
    result: Option<&serde_json::Value>,
    transition_source: &str,
    allowed_from: Option<&[&str]>,
    touch_completed_at: bool,
) -> Result<usize> {
    set_dispatch_status_on_conn_with_sync(
        conn,
        dispatch_id,
        to_status,
        result,
        transition_source,
        allowed_from,
        touch_completed_at,
        false,
    )
}

pub fn load_dispatch_row_pg_first(
    db: &Db,
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
) -> Result<Option<serde_json::Value>> {
    load_dispatch_row_with_backends(Some(db), pg_pool, dispatch_id)
}

pub fn load_dispatch_row_with_backends(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
) -> Result<Option<serde_json::Value>> {
    let Some(pool) = pg_pool else {
        #[cfg(all(test, feature = "legacy-sqlite-tests"))]
        {
            if let Some(db) = db {
                let conn = db
                    .lock()
                    .map_err(|error| anyhow::anyhow!("legacy db lock poisoned: {error}"))?;
                if !dispatch_exists_on_conn(&conn, dispatch_id)? {
                    return Ok(None);
                }
                return query_dispatch_row(&conn, dispatch_id).map(Some);
            }
        }
        #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
        let _ = db;
        return Err(anyhow::anyhow!(
            "Postgres pool required to load dispatch row {dispatch_id}"
        ));
    };
    let dispatch_id = dispatch_id.to_string();
    block_on_dispatch_pg(pool, move |pool| async move {
        if !dispatch_exists_pg(&pool, &dispatch_id).await? {
            return Ok(None);
        }
        query_dispatch_row_pg(&pool, &dispatch_id).await.map(Some)
    })
}

/// Legacy wrapper — delegates to [`finalize_dispatch`] for callers that already
/// have a fully-formed result JSON (e.g. API PATCH handler).
#[cfg_attr(not(test), allow(dead_code))]
pub fn complete_dispatch(
    db: &Db,
    engine: &PolicyEngine,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<serde_json::Value> {
    complete_dispatch_inner_with_backends(Some(db), engine, dispatch_id, result)
}

fn complete_dispatch_inner_with_backends(
    db: Option<&Db>,
    engine: &PolicyEngine,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<serde_json::Value> {
    let dispatch_span =
        crate::logging::dispatch_span("complete_dispatch", Some(dispatch_id), None, None);
    let _guard = dispatch_span.enter();
    let Some(pool) = engine.pg_pool() else {
        #[cfg(all(test, feature = "legacy-sqlite-tests"))]
        if let Some(db) = db {
            return complete_dispatch_inner_sqlite(db, engine, dispatch_id, result);
        }
        #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
        let _ = db;
        return Err(anyhow::anyhow!(
            "Postgres pool required to complete dispatch {dispatch_id}"
        ));
    };
    let db_owned = db.cloned();
    let dispatch_id_owned = dispatch_id.to_string();
    let input_result = result.clone();
    let (
        dispatch,
        kanban_card_id,
        needs_review_dispatch,
        effective_result,
        skip_dispatch_completed_hooks,
    ) = block_on_dispatch_pg(pool, move |pool| async move {
        validate_dispatch_completion_evidence_on_pg(
            &pool,
            db_owned.as_ref(),
            &dispatch_id_owned,
            &input_result,
        )
        .await?;

        let result_owned =
            maybe_inject_phase_gate_verdict_pg(&pool, &dispatch_id_owned, &input_result).await;
        let effective_result = result_owned.unwrap_or(input_result);

        // #1980: complete_dispatch fires the OnDispatchCompleted JS hook
        // immediately after this returns; that hook owns the phase-gate row
        // lifecycle plus run finalize/activate. Use the external-phase-gate
        // variant so the durable reconciler does not clear the gate row
        // out from under the hook.
        let changed = set_dispatch_status_on_pg_with_external_phase_gate(
            &pool,
            &dispatch_id_owned,
            "completed",
            Some(&effective_result),
            effective_result
                .get("completion_source")
                .and_then(|value| value.as_str())
                .unwrap_or("complete_dispatch"),
            Some(&["pending", "dispatched"]),
            true,
        )
        .await?;

        if changed == 0 {
            if dispatch_exists_pg(&pool, &dispatch_id_owned).await? {
                tracing::info!("skipping completion hooks because dispatch is already finalized");
                let dispatch = query_dispatch_row_pg(&pool, &dispatch_id_owned).await?;
                return Ok((dispatch, None, false, effective_result, true));
            }
            return Err(anyhow::anyhow!("Dispatch not found: {dispatch_id_owned}"));
        }

        let dispatch = query_dispatch_row_pg(&pool, &dispatch_id_owned).await?;
        let kanban_card_id = dispatch
            .get("kanban_card_id")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string());
        let dispatch_type = dispatch
            .get("dispatch_type")
            .and_then(|value| value.as_str());
        let skip_dispatch_completed_hooks =
            matches!(dispatch_type, Some("implementation" | "rework"))
                && auto_queue_review_disabled_for_dispatch_pg(&pool, &dispatch_id_owned).await?;
        let needs_review_dispatch = if skip_dispatch_completed_hooks {
            false
        } else if let Some(card_id) = kanban_card_id.as_deref() {
            card_needs_review_dispatch_pg(&pool, card_id).await?
        } else {
            false
        };

        if skip_dispatch_completed_hooks && let Some(card_id) = kanban_card_id.as_deref() {
            restore_auto_queue_mainline_after_review_skip_on_pg(&pool, card_id, &dispatch_id_owned)
                .await?;
        }

        Ok((
            dispatch,
            kanban_card_id,
            needs_review_dispatch,
            effective_result,
            skip_dispatch_completed_hooks,
        ))
    })?;

    // Auto-queue review_mode=disabled keeps implementation/rework completions on
    // the mainline path. The generic OnDispatchCompleted policy always routes
    // work completions into review, so skip that hook entirely for this narrow case.
    if skip_dispatch_completed_hooks {
        return Ok(dispatch);
    }

    crate::kanban::fire_event_hooks_with_backends(
        db,
        engine,
        "on_dispatch_completed",
        "OnDispatchCompleted",
        json!({
            "dispatch_id": dispatch_id,
            "kanban_card_id": kanban_card_id,
            "result": effective_result,
        }),
    );

    crate::kanban::drain_hook_side_effects_with_backends(db, engine);

    if needs_review_dispatch {
        let cid = kanban_card_id.as_deref().unwrap_or("unknown");
        tracing::warn!(
            "[dispatch] Card {} in review-like state but no review dispatch — re-firing OnReviewEnter with blocking lock (#220)",
            cid
        );
        let _ = engine.fire_hook_by_name_blocking("OnReviewEnter", json!({ "card_id": cid }));
        crate::kanban::drain_hook_side_effects_with_backends(db, engine);
    }

    Ok(dispatch)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn complete_dispatch_inner_sqlite(
    db: &Db,
    engine: &PolicyEngine,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<serde_json::Value> {
    let conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB lock error: {e}"))?;

    validate_dispatch_completion_evidence_on_conn(
        &conn,
        db,
        engine.pg_pool(),
        dispatch_id,
        result,
    )?;

    let result_owned = maybe_inject_phase_gate_verdict_sqlite(&conn, dispatch_id, result);
    let effective_result = result_owned.unwrap_or_else(|| result.clone());

    let changed = set_dispatch_status_on_conn_with_sync(
        &conn,
        dispatch_id,
        "completed",
        Some(&effective_result),
        effective_result
            .get("completion_source")
            .and_then(|value| value.as_str())
            .unwrap_or("complete_dispatch"),
        Some(&["pending", "dispatched"]),
        true,
        true,
    )?;

    if changed == 0 {
        if dispatch_exists_on_conn(&conn, dispatch_id)? {
            tracing::info!("skipping completion hooks because dispatch is already finalized");
            return query_dispatch_row(&conn, dispatch_id);
        }
        return Err(anyhow::anyhow!("Dispatch not found: {dispatch_id}"));
    }

    let dispatch = query_dispatch_row(&conn, dispatch_id)?;
    let kanban_card_id = dispatch
        .get("kanban_card_id")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let dispatch_type = dispatch
        .get("dispatch_type")
        .and_then(|value| value.as_str());
    let skip_dispatch_completed_hooks = matches!(dispatch_type, Some("implementation" | "rework"))
        && auto_queue_review_disabled_for_dispatch_on_conn(&conn, dispatch_id);

    let needs_review_dispatch = if skip_dispatch_completed_hooks {
        false
    } else if let Some(card_id) = kanban_card_id.as_deref() {
        card_needs_review_dispatch_on_conn(&conn, card_id)?
    } else {
        false
    };

    if skip_dispatch_completed_hooks && let Some(card_id) = kanban_card_id.as_deref() {
        restore_auto_queue_mainline_after_review_skip_on_conn(&conn, card_id, dispatch_id)?;
    }

    drop(conn);

    if skip_dispatch_completed_hooks {
        return Ok(dispatch);
    }

    crate::kanban::fire_event_hooks_with_backends(
        Some(db),
        engine,
        "on_dispatch_completed",
        "OnDispatchCompleted",
        json!({
            "dispatch_id": dispatch_id,
            "kanban_card_id": kanban_card_id,
            "result": effective_result,
        }),
    );

    crate::kanban::drain_hook_side_effects_with_backends(Some(db), engine);

    if needs_review_dispatch {
        let cid = kanban_card_id.as_deref().unwrap_or("unknown");
        tracing::warn!(
            "[dispatch] Card {} in review-like state but no review dispatch — re-firing OnReviewEnter with blocking lock (#220)",
            cid
        );
        let _ = engine.fire_hook_by_name_blocking("OnReviewEnter", json!({ "card_id": cid }));
        crate::kanban::drain_hook_side_effects_with_backends(Some(db), engine);
    }

    Ok(dispatch)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn maybe_inject_phase_gate_verdict_sqlite(
    conn: &sqlite_test::Connection,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Option<serde_json::Value> {
    let context_raw: Option<String> = conn
        .query_row(
            "SELECT context FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok()?;
    let ctx: serde_json::Value = context_raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())?;
    let phase_gate_ctx = ctx.get("phase_gate").and_then(|value| value.as_object())?;
    infer_phase_gate_verdict(dispatch_id, phase_gate_ctx, result)
}

/// #699: inject `verdict = context.phase_gate.pass_verdict` into a phase-gate
/// dispatch result when every declared `checks.*` entry passed but the caller
/// forgot the explicit verdict field.
///
/// Returns `Some(enriched)` only when an injection happened — callers should
/// fall back to the original `result` otherwise. Never overrides an explicit
/// verdict/decision (even `"fail"`) and never injects when any check is not
/// `pass`.
fn infer_phase_gate_verdict(
    dispatch_id: &str,
    phase_gate_ctx: &serde_json::Map<String, serde_json::Value>,
    result: &serde_json::Value,
) -> Option<serde_json::Value> {
    // Explicit verdict/decision already present — never override, even for
    // explicit "fail" cases.
    let has_verdict = result
        .get("verdict")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let has_decision = result
        .get("decision")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if has_verdict || has_decision {
        return None;
    }

    let checks_obj = result.get("checks").and_then(|v| v.as_object())?;
    if checks_obj.is_empty() {
        return None;
    }

    // Round-2 fix: when the dispatch context declares a list of required
    // checks, every one of those keys must be present in `result.checks` and
    // pass. Missing keys are treated as no-verdict/failure so a partial
    // payload cannot advance the gate.
    let declared_checks: Vec<String> = phase_gate_ctx
        .get("checks")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    for required in &declared_checks {
        match checks_obj.get(required) {
            Some(entry) if check_entry_is_pass(entry) => {}
            _ => return None,
        }
    }

    // Also require every *present* check entry to pass — never infer a pass
    // on the strength of partial "pass"es when some keys report fail/other.
    for (_name, entry) in checks_obj.iter() {
        if !check_entry_is_pass(entry) {
            return None;
        }
    }

    // Resolve `pass_verdict` from the dispatch's own phase_gate context, with
    // the system default as a last resort.
    let pass_verdict = phase_gate_ctx
        .get("pass_verdict")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "phase_gate_passed".to_string());

    let mut enriched = result.clone();
    if !enriched.is_object() {
        enriched = serde_json::Value::Object(serde_json::Map::new());
    }
    if let Some(obj) = enriched.as_object_mut() {
        obj.insert(
            "verdict".to_string(),
            serde_json::Value::String(pass_verdict.clone()),
        );
        obj.insert(
            "verdict_inferred".to_string(),
            serde_json::Value::Bool(true),
        );
    }

    tracing::info!(
        "[dispatch] #699 inferring phase-gate verdict '{}' for dispatch {} (all {} declared checks passed, {} entries total)",
        pass_verdict,
        dispatch_id,
        declared_checks.len(),
        checks_obj.len(),
    );

    Some(enriched)
}

fn check_entry_is_pass(entry: &serde_json::Value) -> bool {
    // Accept either `{"status": "pass"}` (canonical) or a bare string "pass".
    if let Some(status) = entry.get("status").and_then(|v| v.as_str()) {
        return status.eq_ignore_ascii_case("pass") || status.eq_ignore_ascii_case("passed");
    }
    if let Some(outcome) = entry.get("result").and_then(|v| v.as_str()) {
        return outcome.eq_ignore_ascii_case("pass") || outcome.eq_ignore_ascii_case("passed");
    }
    if let Some(s) = entry.as_str() {
        return s.eq_ignore_ascii_case("pass") || s.eq_ignore_ascii_case("passed");
    }
    false
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[path = "dispatch_status_relocated_tests.rs"]
mod relocated_tests;
