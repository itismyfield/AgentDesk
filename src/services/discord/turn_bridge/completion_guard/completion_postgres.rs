//! Runtime Postgres dispatch-completion operations split out of
//! `completion_guard.rs` (#3479). Last-resort completion/failure paths that
//! write directly to the canonical Postgres store, plus the auto-queue
//! reconciliation helpers and dispatch-followup/reconcile-marker plumbing.
//!
//! Behaviour-preserving verbatim extraction; visibility and the one relocated
//! `super::` path are the only adjustments.

use sqlx::Row;

fn transition_source_uses_live_command_bot(transition_source: &str) -> bool {
    let source = transition_source.trim();
    source.starts_with("turn_bridge") || source.starts_with("watcher")
}

fn with_runtime_postgres_result<T, F>(operation: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(
            sqlx::PgPool,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, String>> + Send>>
        + Send
        + 'static,
{
    let config = crate::config::load().map_err(|error| format!("load runtime config: {error}"))?;
    crate::utils::async_bridge::block_on_result(
        async move {
            let Some(pool) = crate::db::postgres::connect(&config).await? else {
                return Err("postgres is not configured".to_string());
            };
            operation(pool).await
        },
        |error| error,
    )
}

fn runtime_postgres_reconcile_key(dispatch_id: &str) -> String {
    format!("reconcile_dispatch:{dispatch_id}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DispatchFailureWriteOutcome {
    Updated,
    AlreadyTerminal,
    Missing,
    HardError(String),
}

#[derive(Debug, Clone)]
struct DispatchFailurePostCommit {
    dispatch_id: String,
    current_status: String,
    kanban_card_id: Option<String>,
    agent_id: Option<String>,
    dispatch_type: Option<String>,
    transition_source: String,
    result: serde_json::Value,
}

fn should_sync_runtime_auto_queue_terminal_entry(
    dispatch_type: Option<&str>,
    _result: &serde_json::Value,
    auto_queue_review_disabled: bool,
) -> bool {
    // #3605 (T2): inert side-paths (consultation, scope-assessment) must never
    // finalize a bound auto_queue entry on completion — mirror of
    // dispatch_status::should_skip_auto_queue_terminal_sync for the live
    // turn_bridge/watcher completion path. Without this, a scope-assessment
    // completing through the runtime path would mark the entry done and close
    // the card with no implementation dispatch.
    if crate::dispatch::dispatch_is_side_path(dispatch_type) {
        return false;
    }
    // #3594 (T3): plan / plan-review are multi-stage WORK dispatches whose
    // completion the kanban-rules JS fan-out consumes to RE-DISPATCH the bound
    // auto-queue entry to the next stage (plan → plan-review|impl, plan-review →
    // impl|re-plan). Finalizing the entry here (the live turn_bridge/watcher
    // completion path) would break that chain and close the card with no
    // implementation. Mirror of dispatch_status::should_skip_auto_queue_terminal_sync
    // — do NOT sync; JS owns the entry transition.
    if matches!(dispatch_type, Some("plan" | "plan-review")) {
        return false;
    }
    match dispatch_type {
        Some("implementation" | "rework") => auto_queue_review_disabled,
        _ => true,
    }
}

async fn auto_queue_review_disabled_for_runtime_dispatch_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
) -> Result<bool, String> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
            SELECT 1
            FROM auto_queue_entries e
            JOIN auto_queue_runs r ON r.id = e.run_id
            WHERE e.dispatch_id = $1
              AND e.status = 'dispatched'
              AND r.status IN ('active', 'paused')
              AND COALESCE(r.review_mode, 'enabled') = 'disabled'
        )",
    )
    .bind(dispatch_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| {
        format!("load auto-queue review_mode for runtime dispatch {dispatch_id}: {error}")
    })
}

fn runtime_pg_complete_dispatch_with_result(
    dispatch_id: &str,
    result: &serde_json::Value,
    transition_source: &str,
) -> bool {
    let dispatch_id = dispatch_id.to_string();
    let result_json = result.to_string();
    let result_value = result.clone();
    let transition_source = transition_source.to_string();
    with_runtime_postgres_result(move |pool| {
        Box::pin(async move {
            let mut tx = pool
                .begin()
                .await
                .map_err(|error| format!("begin postgres completion via {transition_source} for {dispatch_id}: {error}"))?;

            let current = sqlx::query(
                "SELECT status, kanban_card_id, dispatch_type
                 FROM task_dispatches
                 WHERE id = $1",
            )
            .bind(&dispatch_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| format!("load postgres dispatch {dispatch_id}: {error}"))?;
            let Some(current) = current else {
                return Ok(false);
            };

            let current_status = current
                .try_get::<Option<String>, _>("status")
                .ok()
                .flatten()
                .unwrap_or_default();
            if !matches!(current_status.as_str(), "pending" | "dispatched") {
                return Ok(false);
            }

            let changed = sqlx::query(
                "UPDATE task_dispatches
                 SET status = 'completed',
                     result = CAST($1 AS jsonb),
                     updated_at = NOW(),
                     completed_at = COALESCE(completed_at, NOW())
                 WHERE id = $2
                   AND status = $3",
            )
            .bind(&result_json)
            .bind(&dispatch_id)
            .bind(&current_status)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("update postgres dispatch {dispatch_id} to completed: {error}"))?
            .rows_affected();
            if changed == 0 {
                return Ok(false);
            }

            let kanban_card_id = current
                .try_get::<Option<String>, _>("kanban_card_id")
                .ok()
                .flatten();
            let dispatch_type = current
                .try_get::<Option<String>, _>("dispatch_type")
                .ok()
                .flatten();
            let auto_queue_review_disabled =
                if matches!(dispatch_type.as_deref(), Some("implementation" | "rework")) {
                    auto_queue_review_disabled_for_runtime_dispatch_pg(&mut tx, &dispatch_id)
                        .await?
                } else {
                    false
                };

            sqlx::query(
                "INSERT INTO dispatch_events (
                    dispatch_id,
                    kanban_card_id,
                    dispatch_type,
                    from_status,
                    to_status,
                    transition_source,
                    payload_json
                ) VALUES ($1, $2, $3, $4, 'completed', $5, CAST($6 AS jsonb))",
            )
            .bind(&dispatch_id)
            .bind(kanban_card_id)
            .bind(dispatch_type.clone())
            .bind(&current_status)
            .bind(&transition_source)
            .bind(&result_json)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("record postgres dispatch event for {dispatch_id}: {error}"))?;

            if should_sync_runtime_auto_queue_terminal_entry(
                dispatch_type.as_deref(),
                &result_value,
                auto_queue_review_disabled,
            ) {
                crate::db::auto_queue::finalize_completed_dispatch_terminal_entry_on_pg_tx(
                    &mut tx,
                    &dispatch_id,
                    &transition_source,
                    true,
                )
                .await
                .map_err(|error| {
                    format!(
                        "sync auto_queue_entries on runtime dispatch completion {dispatch_id}: {error}"
                    )
                })?;
            }

            sqlx::query(
                "INSERT INTO kv_meta (key, value)
                 VALUES ($1, $2)
                 ON CONFLICT (key) DO UPDATE
                     SET value = EXCLUDED.value",
            )
            .bind(runtime_postgres_reconcile_key(&dispatch_id))
            .bind(&dispatch_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("set postgres reconcile marker for {dispatch_id}: {error}"))?;

            if !transition_source_uses_live_command_bot(&transition_source) {
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
                .bind(&dispatch_id)
                .execute(&mut *tx)
                .await
                .map_err(|error| format!("enqueue postgres status reaction for {dispatch_id}: {error}"))?;
            }

            tx.commit()
                .await
                .map_err(|error| format!("commit postgres completion via {transition_source} for {dispatch_id}: {error}"))?;
            Ok(true)
        })
    })
    .unwrap_or(false)
}

async fn runtime_max_entry_retries_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<i64, String> {
    let persisted = sqlx::query_scalar::<_, Option<String>>(
        "SELECT value FROM kv_meta WHERE key = 'runtime-config'",
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("load runtime maxEntryRetries: {error}"))?
    .flatten()
    .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
    .and_then(|value| {
        value
            .get("maxEntryRetries")
            .and_then(serde_json::Value::as_u64)
    });
    let fallback = crate::config::load()
        .map_err(|error| format!("load runtime config: {error}"))?
        .runtime
        .max_entry_retries
        .unwrap_or(3);
    Ok(i64::try_from(persisted.unwrap_or(fallback))
        .unwrap_or(i64::MAX)
        .max(1))
}

async fn record_runtime_dispatch_failure_entries_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
    retry_limit: i64,
    trigger_source: &str,
) -> Result<Vec<crate::db::auto_queue::EntryDispatchFailureResult>, String> {
    let entry_ids = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM auto_queue_entries
         WHERE dispatch_id = $1
           AND status = 'dispatched'
         ORDER BY id",
    )
    .bind(dispatch_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| {
        format!("load auto-queue entries for failed dispatch {dispatch_id}: {error}")
    })?;
    let mut results = Vec::with_capacity(entry_ids.len());
    for entry_id in entry_ids {
        results.push(
            crate::db::auto_queue::EntryDispatchFailureResult::record_on_pg_tx(
                tx,
                &entry_id,
                retry_limit,
                trigger_source,
            )
            .await?,
        );
    }
    Ok(results)
}

async fn fail_runtime_dispatch_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
    result_json: &str,
    retryable: bool,
    transition_source: &str,
) -> Result<
    (
        DispatchFailureWriteOutcome,
        Option<DispatchFailurePostCommit>,
    ),
    String,
> {
    let current = sqlx::query(
        "SELECT status, kanban_card_id, to_agent_id, dispatch_type,
                context::TEXT AS context_text
         FROM task_dispatches
         WHERE id = $1
         FOR UPDATE",
    )
    .bind(dispatch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("lock postgres dispatch {dispatch_id}: {error}"))?;
    let Some(current) = current else {
        return Ok((DispatchFailureWriteOutcome::Missing, None));
    };
    let current_status = current
        .try_get::<Option<String>, _>("status")
        .ok()
        .flatten()
        .unwrap_or_default();
    if !matches!(current_status.as_str(), "pending" | "dispatched") {
        return Ok((DispatchFailureWriteOutcome::AlreadyTerminal, None));
    }
    let retry_limit = if retryable {
        runtime_max_entry_retries_pg(tx).await?
    } else {
        1
    };
    record_runtime_dispatch_failure_entries_on_pg_tx(
        tx,
        dispatch_id,
        retry_limit,
        transition_source,
    )
    .await?;

    let changed = sqlx::query(
        "UPDATE task_dispatches
         SET status = 'failed',
             result = CAST($1 AS jsonb),
             updated_at = NOW(),
             last_stuck_alert_at = NULL
         WHERE id = $2
           AND status = $3",
    )
    .bind(result_json)
    .bind(dispatch_id)
    .bind(&current_status)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("update postgres dispatch {dispatch_id} to failed: {error}"))?
    .rows_affected();
    if changed == 0 {
        return Ok((DispatchFailureWriteOutcome::AlreadyTerminal, None));
    }

    let kanban_card_id = current
        .try_get::<Option<String>, _>("kanban_card_id")
        .map_err(|error| format!("decode kanban card for dispatch {dispatch_id}: {error}"))?;
    let agent_id = current
        .try_get::<Option<String>, _>("to_agent_id")
        .map_err(|error| format!("decode target agent for dispatch {dispatch_id}: {error}"))?;
    let dispatch_type = current
        .try_get::<Option<String>, _>("dispatch_type")
        .map_err(|error| format!("decode type for dispatch {dispatch_id}: {error}"))?;
    let context_text = current
        .try_get::<Option<String>, _>("context_text")
        .map_err(|error| format!("decode context for dispatch {dispatch_id}: {error}"))?;
    sqlx::query(
        "INSERT INTO dispatch_events (
            dispatch_id,
            kanban_card_id,
            dispatch_type,
            from_status,
            to_status,
            transition_source,
            payload_json
        ) VALUES ($1, $2, $3, $4, 'failed', $5, CAST($6 AS jsonb))",
    )
    .bind(dispatch_id)
    .bind(&kanban_card_id)
    .bind(&dispatch_type)
    .bind(&current_status)
    .bind(transition_source)
    .bind(result_json)
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        format!("record postgres dispatch failure event for {dispatch_id}: {error}")
    })?;

    crate::db::auto_queue::reconcile_phase_gate_for_terminal_dispatch_on_pg_tx(
        tx,
        dispatch_id,
        "failed",
        context_text.as_deref(),
        Some(result_json),
    )
    .await
    .map_err(|error| format!("reconcile phase-gate for failed dispatch {dispatch_id}: {error}"))?;
    crate::db::dispatch_semaphores::release_dispatch_semaphores_on_pg_tx(tx, dispatch_id)
        .await
        .map_err(|error| {
            format!("release postgres dispatch semaphores for {dispatch_id}: {error}")
        })?;
    sqlx::query(
        "UPDATE sessions
         SET status = CASE
                 WHEN status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working') THEN 'idle'
                 ELSE status
             END,
             active_dispatch_id = NULL,
             session_info = 'Dispatch failed',
             last_heartbeat = NOW()
         WHERE active_dispatch_id = $1",
    )
    .bind(dispatch_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("clear postgres session dispatch link {dispatch_id}: {error}"))?;

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
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        format!("enqueue postgres failure status reaction for {dispatch_id}: {error}")
    })?;
    let result = serde_json::from_str(result_json)
        .map_err(|error| format!("decode failure result for dispatch {dispatch_id}: {error}"))?;
    Ok((
        DispatchFailureWriteOutcome::Updated,
        Some(DispatchFailurePostCommit {
            dispatch_id: dispatch_id.to_string(),
            current_status,
            kanban_card_id,
            agent_id,
            dispatch_type,
            transition_source: transition_source.to_string(),
            result,
        }),
    ))
}

pub(super) fn dispatch_failure_result(
    error_msg: &str,
    error_code: Option<&str>,
) -> serde_json::Value {
    let message = error_msg.chars().take(500).collect::<String>();
    match error_code {
        Some(code) => serde_json::json!({
            "error": code,
            "message": message,
        }),
        None => serde_json::json!({
            "error": message,
        }),
    }
}

fn emit_dispatch_failure_post_commit(effect: DispatchFailurePostCommit) {
    crate::services::observability::emit_dispatch_result(
        &effect.dispatch_id,
        effect.kanban_card_id.as_deref(),
        effect.dispatch_type.as_deref(),
        Some(&effect.current_status),
        "failed",
        &effect.transition_source,
        Some(&effect.result),
    );
    crate::dispatch::emit_dispatch_quality_event(
        &effect.dispatch_id,
        effect.agent_id.as_deref(),
        effect.kanban_card_id.as_deref(),
        effect.dispatch_type.as_deref(),
        Some(&effect.current_status),
        "failed",
        &effect.transition_source,
        Some(&effect.result),
    );
}

async fn fail_runtime_dispatch_with_pool(
    pool: &sqlx::PgPool,
    dispatch_id: &str,
    failure_result: &str,
    retryable: bool,
    transition_source: &str,
) -> Result<
    (
        DispatchFailureWriteOutcome,
        Option<DispatchFailurePostCommit>,
    ),
    String,
> {
    let mut tx = pool.begin().await.map_err(|error| {
        format!("begin postgres failure transaction for {dispatch_id}: {error}")
    })?;
    let (outcome, post_commit) = fail_runtime_dispatch_on_pg_tx(
        &mut tx,
        dispatch_id,
        failure_result,
        retryable,
        transition_source,
    )
    .await?;
    tx.commit().await.map_err(|error| {
        format!("commit postgres failure transaction for {dispatch_id}: {error}")
    })?;
    if matches!(
        outcome,
        DispatchFailureWriteOutcome::Updated | DispatchFailureWriteOutcome::AlreadyTerminal
    ) && let Err(error) =
        crate::services::dispatches::wait_queue::wake_cached_constraint_release_pg(
            pool,
            "constraint_release",
        )
        .await
    {
        tracing::warn!(
            %dispatch_id,
            %error,
            "post-commit constraint wait-queue wake failed"
        );
    }
    // AlreadyTerminal can be a replay after the original terminal transition
    // missed its immediate wake. The periodic leader sweep is the correctness
    // backstop, but replaying the wake here preserves the canonical writer's
    // low-latency contract instead of accepting up to the default 30s delay.
    Ok((outcome, post_commit))
}

fn finish_dispatch_failure_write(
    result: Result<
        (
            DispatchFailureWriteOutcome,
            Option<DispatchFailurePostCommit>,
        ),
        String,
    >,
) -> DispatchFailureWriteOutcome {
    match result {
        Ok((outcome, post_commit)) => {
            if let Some(effect) = post_commit {
                emit_dispatch_failure_post_commit(effect);
            }
            outcome
        }
        Err(error) => DispatchFailureWriteOutcome::HardError(error),
    }
}

fn runtime_pg_fail_dispatch_with_result_source(
    dispatch_id: &str,
    error_msg: &str,
    error_code: Option<&str>,
    retryable: bool,
    transition_source: &str,
) -> DispatchFailureWriteOutcome {
    let dispatch_id = dispatch_id.to_string();
    let failure_result = dispatch_failure_result(error_msg, error_code).to_string();
    let transition_source = transition_source.to_string();
    finish_dispatch_failure_write(with_runtime_postgres_result(move |pool| {
        Box::pin(async move {
            fail_runtime_dispatch_with_pool(
                &pool,
                &dispatch_id,
                &failure_result,
                retryable,
                &transition_source,
            )
            .await
        })
    }))
}

pub(super) fn runtime_pg_fail_dispatch_with_result(
    dispatch_id: &str,
    error_msg: &str,
    error_code: Option<&str>,
    retryable: bool,
) -> DispatchFailureWriteOutcome {
    runtime_pg_fail_dispatch_with_result_source(
        dispatch_id,
        error_msg,
        error_code,
        retryable,
        "turn_bridge_dispatch_failure",
    )
}

/// Explicitly complete implementation/rework dispatches at turn end.
/// Last-resort dispatch completion via the canonical Postgres store.
pub(in crate::services::discord) fn runtime_db_fallback_complete_with_result(
    dispatch_id: &str,
    result: &serde_json::Value,
) -> bool {
    runtime_pg_complete_dispatch_with_result(dispatch_id, result, "turn_bridge_runtime_db_fallback")
}

pub(in crate::services::discord) fn streaming_final_complete_dispatch_with_result(
    dispatch_id: &str,
    result: &serde_json::Value,
) -> bool {
    runtime_pg_complete_dispatch_with_result(dispatch_id, result, "watcher_streaming_final")
}

pub(in crate::services::discord) async fn queue_dispatch_followup_with_handles(
    pg_pool: Option<&sqlx::PgPool>,
    dispatch_id: &str,
    source: &str,
) -> bool {
    if let Some(pool) = pg_pool {
        if let Err(error) =
            crate::services::dispatches_followup::queue_dispatch_followup_pg(pool, dispatch_id)
                .await
        {
            tracing::warn!(
                "[{source}] failed to enqueue postgres dispatch followup for {dispatch_id}: {error}"
            );
            return false;
        }
        return true;
    }

    tracing::warn!(
        "[{source}] no postgres pool available to enqueue dispatch followup for {dispatch_id}"
    );
    false
}

pub(super) async fn store_reconcile_marker_with_handles(
    pg_pool: Option<&sqlx::PgPool>,
    dispatch_id: &str,
    source: &str,
) -> bool {
    let reconcile_key = runtime_postgres_reconcile_key(dispatch_id);
    if super::super::super::internal_api::set_kv_value(&reconcile_key, dispatch_id).is_ok() {
        return true;
    }

    if let Some(pool) = pg_pool {
        if let Err(error) = sqlx::query(
            "INSERT INTO kv_meta (key, value)
             VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE
                 SET value = EXCLUDED.value",
        )
        .bind(&reconcile_key)
        .bind(dispatch_id)
        .execute(pool)
        .await
        {
            tracing::warn!(
                "[{source}] failed to persist postgres reconcile marker for {dispatch_id}: {error}"
            );
            return false;
        }
        return true;
    }

    false
}

#[cfg(test)]
mod dispatch_failure_pg_tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;

    struct RuntimeConfigFixture {
        _root_guard: crate::config::TestEnvVarGuard,
        _root: tempfile::TempDir,
    }

    fn runtime_config_fixture() -> RuntimeConfigFixture {
        let root = tempfile::tempdir().expect("create runtime config root");
        let config_dir = root.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("create runtime config directory");
        std::fs::write(config_dir.join("agentdesk.yaml"), "server: {}\n")
            .expect("write minimal runtime config");
        let root_guard = crate::config::set_agentdesk_root_for_test(root.path());
        RuntimeConfigFixture {
            _root_guard: root_guard,
            _root: root,
        }
    }

    async fn seed_failure_fixture(
        pool: &sqlx::PgPool,
        suffix: &str,
        dispatch_status: &str,
        retry_limit: i64,
    ) -> (String, String, String) {
        let run_id = format!("run-{suffix}");
        let entry_id = format!("entry-{suffix}");
        let dispatch_id = format!("dispatch-{suffix}");
        sqlx::query("INSERT INTO auto_queue_runs (id, status) VALUES ($1, 'active')")
            .bind(&run_id)
            .execute(pool)
            .await
            .expect("seed dispatch failure run");
        sqlx::query(
            "INSERT INTO task_dispatches (id, status, dispatch_type)
             VALUES ($1, $2, 'implementation')",
        )
        .bind(&dispatch_id)
        .bind(dispatch_status)
        .execute(pool)
        .await
        .expect("seed failed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                 id, run_id, agent_id, status, retry_count, dispatch_id
             ) VALUES ($1, $2, 'agent-1', 'dispatched', 0, $3)",
        )
        .bind(&entry_id)
        .bind(&run_id)
        .bind(&dispatch_id)
        .execute(pool)
        .await
        .expect("seed linked entry");
        sqlx::query(
            "INSERT INTO kv_meta (key, value)
             VALUES ('runtime-config', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(serde_json::json!({"maxEntryRetries": retry_limit}).to_string())
        .execute(pool)
        .await
        .expect("seed retry policy");
        (run_id, entry_id, dispatch_id)
    }

    #[tokio::test]
    async fn fallback_failure_reduces_dispatch_entry_and_run_in_one_transaction_pg() {
        let _config = runtime_config_fixture();
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, entry_id, dispatch_id) =
            seed_failure_fixture(&pool, "fallback", "dispatched", 3).await;
        let result_json = dispatch_failure_result("transport failure", None).to_string();
        let mut tx = pool.begin().await.expect("begin fallback test tx");
        let (outcome, _) = fail_runtime_dispatch_on_pg_tx(
            &mut tx,
            &dispatch_id,
            &result_json,
            true,
            "test_fallback",
        )
        .await
        .expect("reduce fallback failure");
        tx.commit().await.expect("commit fallback test tx");
        assert_eq!(outcome, DispatchFailureWriteOutcome::Updated);

        let state = sqlx::query_as::<_, (String, String, i64, String, i64, i64)>(
            "SELECT d.status, e.status, e.retry_count, r.status,
                    (SELECT COUNT(*) FROM dispatch_events
                     WHERE dispatch_id = d.id AND to_status = 'failed'),
                    (SELECT COUNT(*) FROM auto_queue_entry_transitions
                     WHERE entry_id = e.id AND to_status = 'pending')
             FROM task_dispatches d
             JOIN auto_queue_entries e ON e.id = $2
             JOIN auto_queue_runs r ON r.id = $3
             WHERE d.id = $1",
        )
        .bind(&dispatch_id)
        .bind(&entry_id)
        .bind(&run_id)
        .fetch_one(&pool)
        .await
        .expect("load fallback state");
        assert_eq!(
            state,
            (
                "failed".to_string(),
                "pending".to_string(),
                1,
                "active".to_string(),
                1,
                1,
            )
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn cancelled_run_window_terminalizes_dispatch_and_entry_pg() {
        let _config = runtime_config_fixture();
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, entry_id, dispatch_id) =
            seed_failure_fixture(&pool, "cancel-window", "dispatched", 3).await;
        sqlx::query("UPDATE auto_queue_runs SET status = 'cancelled' WHERE id = $1")
            .bind(&run_id)
            .execute(&pool)
            .await
            .expect("enter real run-cancel window");

        let result_json =
            dispatch_failure_result("failure during run cancellation", None).to_string();
        let mut tx = pool.begin().await.expect("begin cancel-window tx");
        let (outcome, _) = fail_runtime_dispatch_on_pg_tx(
            &mut tx,
            &dispatch_id,
            &result_json,
            true,
            "test_cancel_window",
        )
        .await
        .expect("cancel window must not block dispatch termination");
        tx.commit().await.expect("commit cancel-window failure");

        let state = sqlx::query_as::<_, (String, String, i64, String, i64, i64)>(
            "SELECT d.status, e.status, e.retry_count, r.status,
                    (SELECT COUNT(*) FROM dispatch_events WHERE dispatch_id = d.id),
                    (SELECT COUNT(*) FROM auto_queue_entry_transitions WHERE entry_id = e.id)
             FROM task_dispatches d
             JOIN auto_queue_entries e ON e.id = $2
             JOIN auto_queue_runs r ON r.id = $3
             WHERE d.id = $1",
        )
        .bind(&dispatch_id)
        .bind(&entry_id)
        .bind(&run_id)
        .fetch_one(&pool)
        .await
        .expect("load cancel-window state");
        assert_eq!(outcome, DispatchFailureWriteOutcome::Updated);
        assert_eq!(
            state,
            (
                "failed".to_string(),
                "failed".to_string(),
                1,
                "cancelled".to_string(),
                1,
                1,
            )
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn restoring_retry_failure_terminalizes_dispatch_and_requeues_entry_pg() {
        let _config = runtime_config_fixture();
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, entry_id, dispatch_id) =
            seed_failure_fixture(&pool, "restoring-retry", "dispatched", 3).await;
        sqlx::query("UPDATE auto_queue_runs SET status = 'restoring' WHERE id = $1")
            .bind(&run_id)
            .execute(&pool)
            .await
            .expect("move run into restore window");

        let result_json = dispatch_failure_result("restore-window failure", None).to_string();
        let mut tx = pool.begin().await.expect("begin restoring retry tx");
        let (outcome, _) = fail_runtime_dispatch_on_pg_tx(
            &mut tx,
            &dispatch_id,
            &result_json,
            true,
            "test_restoring_retry_dispatch",
        )
        .await
        .expect("reduce restoring dispatch failure");
        tx.commit().await.expect("commit restoring retry tx");

        let state = sqlx::query_as::<_, (String, String, i64, String, i64, i64)>(
            "SELECT d.status, e.status, e.retry_count, r.status,
                    (SELECT COUNT(*) FROM dispatch_events WHERE dispatch_id = d.id),
                    (SELECT COUNT(*) FROM auto_queue_entry_transitions WHERE entry_id = e.id)
             FROM task_dispatches d
             JOIN auto_queue_entries e ON e.id = $2
             JOIN auto_queue_runs r ON r.id = $3
             WHERE d.id = $1",
        )
        .bind(&dispatch_id)
        .bind(&entry_id)
        .bind(&run_id)
        .fetch_one(&pool)
        .await
        .expect("load restoring dispatch failure state");
        assert_eq!(outcome, DispatchFailureWriteOutcome::Updated);
        assert_eq!(
            state,
            (
                "failed".to_string(),
                "pending".to_string(),
                1,
                "restoring".to_string(),
                1,
                1,
            )
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn terminal_failure_reconciles_phase_gate_and_releases_semaphore_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, _entry_id, dispatch_id) =
            seed_failure_fixture(&pool, "side-effects", "dispatched", 1).await;
        let context = serde_json::json!({
            "phase_gate": {
                "run_id": run_id,
                "phase": 1,
                "pass_verdict": "pass",
                "final_phase": false
            }
        });
        sqlx::query("UPDATE task_dispatches SET context = $1 WHERE id = $2")
            .bind(context.to_string())
            .bind(&dispatch_id)
            .execute(&pool)
            .await
            .expect("seed phase-gate context");
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates (
                 run_id, phase, dispatch_id, status, pass_verdict, final_phase
             ) VALUES ($1, 1, $2, 'pending', 'pass', FALSE)",
        )
        .bind(&run_id)
        .bind(&dispatch_id)
        .execute(&pool)
        .await
        .expect("seed phase-gate row");
        sqlx::query(
            "INSERT INTO dispatch_semaphore_holdings (
                 semaphore_name, scope, scope_key, slot_index,
                 holder_instance_id, dispatch_id, expires_at
             ) VALUES ('gpu', 'per-cluster', 'global', 0, 'worker-1', $1, NOW() + INTERVAL '1 hour')",
        )
        .bind(&dispatch_id)
        .execute(&pool)
        .await
        .expect("seed semaphore holding");

        let result_json = dispatch_failure_result("phase gate failed", None).to_string();
        let mut tx = pool.begin().await.expect("begin side-effect tx");
        let (outcome, _) = fail_runtime_dispatch_on_pg_tx(
            &mut tx,
            &dispatch_id,
            &result_json,
            false,
            "test_terminal_side_effects",
        )
        .await
        .expect("reduce terminal side effects");
        tx.commit().await.expect("commit terminal side effects");

        let state = sqlx::query_as::<_, (String, String, i64)>(
            "SELECT pg.status, r.status,
                    (SELECT COUNT(*) FROM dispatch_semaphore_holdings WHERE dispatch_id = $2)
             FROM auto_queue_phase_gates pg
             JOIN auto_queue_runs r ON r.id = pg.run_id
             WHERE pg.run_id = $1 AND pg.dispatch_id = $2",
        )
        .bind(&run_id)
        .bind(&dispatch_id)
        .fetch_one(&pool)
        .await
        .expect("load phase-gate and semaphore state");
        assert_eq!(outcome, DispatchFailureWriteOutcome::Updated);
        assert_eq!(state, ("failed".to_string(), "paused".to_string(), 0));

        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn runtime_writer_preserves_hard_error_outcome() {
        let outcome = finish_dispatch_failure_write(Err("forced writer failure".to_string()));
        assert_eq!(
            outcome,
            DispatchFailureWriteOutcome::HardError("forced writer failure".to_string())
        );
        assert_ne!(outcome, DispatchFailureWriteOutcome::Missing);
    }

    #[tokio::test]
    async fn committed_failure_wakes_waiting_dispatch_outbox_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (_run_id, _entry_id, dispatch_id) =
            seed_failure_fixture(&pool, "wait-wake", "dispatched", 1).await;
        sqlx::query(
            "INSERT INTO worker_nodes (
                 instance_id, hostname, process_id, role, effective_role, status,
                 labels, capabilities, last_heartbeat_at, started_at, updated_at
             ) VALUES (
                 'worker-wake', 'worker', 100, 'auto', 'leader', 'online',
                 '[]'::jsonb, '{}'::jsonb, NOW(), NOW(), NOW()
             )",
        )
        .execute(&pool)
        .await
        .expect("seed wake worker");
        sqlx::query(
            "INSERT INTO dispatch_outbox (
                 dispatch_id, action, status, wait_reason, wait_started_at, created_at
             ) VALUES (
                 'waiting-dispatch', 'notify', 'pending', 'no worker before release', NOW(), NOW()
             )",
        )
        .execute(&pool)
        .await
        .expect("seed waiting dispatch");

        let result_json = dispatch_failure_result("release constraints", None).to_string();
        let (outcome, post_commit) = fail_runtime_dispatch_with_pool(
            &pool,
            &dispatch_id,
            &result_json,
            false,
            "test_wait_wake",
        )
        .await
        .expect("commit failure and wake wait queue");
        assert_eq!(outcome, DispatchFailureWriteOutcome::Updated);
        assert!(post_commit.is_some());
        let wait_state = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT claim_owner, wait_reason FROM dispatch_outbox
             WHERE dispatch_id = 'waiting-dispatch'",
        )
        .fetch_one(&pool)
        .await
        .expect("load woken wait row");
        assert_eq!(wait_state.0.as_deref(), Some("worker-wake"));
        assert!(wait_state.1.is_none());

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn post_commit_failure_emits_result_and_quality_observability() {
        let dispatch_id = format!("dispatch-observability-{}", uuid::Uuid::new_v4());
        emit_dispatch_failure_post_commit(DispatchFailurePostCommit {
            dispatch_id: dispatch_id.clone(),
            current_status: "dispatched".to_string(),
            kanban_card_id: Some("card-observability".to_string()),
            agent_id: Some("agent-observability".to_string()),
            dispatch_type: Some("implementation".to_string()),
            transition_source: "test_observability".to_string(),
            result: dispatch_failure_result("observed failure", None),
        });

        let events = crate::services::observability::events::recent(32);
        let dispatch_result = events.iter().find(|event| {
            event.event_type == "dispatch_result" && event.payload["dispatch_id"] == dispatch_id
        });
        let quality = events.iter().find(|event| {
            event.event_type == "agent_quality_event" && event.payload["dispatch_id"] == dispatch_id
        });
        assert!(dispatch_result.is_some(), "dispatch result event must fire");
        assert_eq!(
            quality.expect("quality event must fire").payload["quality_event_type"],
            "dispatch_failed"
        );
    }

    #[tokio::test]
    async fn concurrent_failure_has_one_dispatch_and_entry_cas_winner_pg() {
        let _config = runtime_config_fixture();
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, entry_id, dispatch_id) =
            seed_failure_fixture(&pool, "concurrent", "dispatched", 3).await;
        let result_json = dispatch_failure_result("concurrent failure", None).to_string();

        let invoke = |pool: sqlx::PgPool, source: &'static str| {
            let dispatch_id = dispatch_id.clone();
            let result_json = result_json.clone();
            tokio::spawn(async move {
                let mut tx = pool.begin().await.expect("begin concurrent failure tx");
                let (outcome, _) = fail_runtime_dispatch_on_pg_tx(
                    &mut tx,
                    &dispatch_id,
                    &result_json,
                    true,
                    source,
                )
                .await
                .expect("reduce concurrent failure");
                tx.commit().await.expect("commit concurrent failure tx");
                outcome
            })
        };
        let first = invoke(pool.clone(), "test_concurrent_first");
        let second = invoke(pool.clone(), "test_concurrent_second");
        let outcomes = [
            first.await.expect("join first failure"),
            second.await.expect("join second failure"),
        ];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == DispatchFailureWriteOutcome::Updated)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == DispatchFailureWriteOutcome::AlreadyTerminal)
                .count(),
            1
        );

        let state = sqlx::query_as::<_, (String, String, i64, String, i64, i64)>(
            "SELECT d.status, e.status, e.retry_count, r.status,
                    (SELECT COUNT(*) FROM dispatch_events WHERE dispatch_id = d.id),
                    (SELECT COUNT(*) FROM auto_queue_entry_transitions WHERE entry_id = e.id)
             FROM task_dispatches d
             JOIN auto_queue_entries e ON e.id = $2
             JOIN auto_queue_runs r ON r.id = $3
             WHERE d.id = $1",
        )
        .bind(&dispatch_id)
        .bind(&entry_id)
        .bind(&run_id)
        .fetch_one(&pool)
        .await
        .expect("load concurrent failure state");
        assert_eq!(
            state,
            (
                "failed".to_string(),
                "pending".to_string(),
                1,
                "active".to_string(),
                1,
                1,
            )
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn terminal_fallback_finalizes_run_once_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, entry_id, dispatch_id) =
            seed_failure_fixture(&pool, "terminal", "dispatched", 1).await;
        let result_json = dispatch_failure_result("terminal failure", None).to_string();
        let mut tx = pool.begin().await.expect("begin terminal test tx");
        let (first, _) = fail_runtime_dispatch_on_pg_tx(
            &mut tx,
            &dispatch_id,
            &result_json,
            false,
            "test_terminal_fallback",
        )
        .await
        .expect("reduce terminal fallback");
        tx.commit().await.expect("commit terminal test tx");
        let first_completed_at = sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>(
            "SELECT completed_at FROM auto_queue_runs WHERE id = $1",
        )
        .bind(&run_id)
        .fetch_one(&pool)
        .await
        .expect("load terminal completion timestamp");

        let mut duplicate_tx = pool.begin().await.expect("begin duplicate test tx");
        let (duplicate, _) = fail_runtime_dispatch_on_pg_tx(
            &mut duplicate_tx,
            &dispatch_id,
            &result_json,
            false,
            "test_terminal_duplicate",
        )
        .await
        .expect("duplicate terminal fallback");
        duplicate_tx
            .commit()
            .await
            .expect("commit duplicate test tx");
        let state =
            sqlx::query_as::<_, (String, i64, String, chrono::DateTime<chrono::Utc>, i64, i64)>(
                "SELECT e.status, e.retry_count, r.status, r.completed_at,
                    (SELECT COUNT(*) FROM dispatch_events WHERE dispatch_id = $2),
                    (SELECT COUNT(*) FROM auto_queue_entry_transitions WHERE entry_id = e.id)
             FROM auto_queue_entries e
             JOIN auto_queue_runs r ON r.id = e.run_id
             WHERE e.id = $1",
            )
            .bind(&entry_id)
            .bind(&dispatch_id)
            .fetch_one(&pool)
            .await
            .expect("load terminal duplicate state");
        assert_eq!(first, DispatchFailureWriteOutcome::Updated);
        assert_eq!(duplicate, DispatchFailureWriteOutcome::AlreadyTerminal);
        assert_eq!(
            state,
            (
                "failed".to_string(),
                1,
                "completed".to_string(),
                first_completed_at,
                1,
                1,
            )
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn patch_conflict_preserves_completed_dispatch_and_linked_entry_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, entry_id, dispatch_id) =
            seed_failure_fixture(&pool, "conflict", "completed", 3).await;
        let result_json = dispatch_failure_result("late failure", None).to_string();
        let mut tx = pool.begin().await.expect("begin conflict test tx");
        let (outcome, _) = fail_runtime_dispatch_on_pg_tx(
            &mut tx,
            &dispatch_id,
            &result_json,
            true,
            "test_patch_conflict",
        )
        .await
        .expect("reconcile completed conflict");
        tx.commit().await.expect("commit conflict test tx");
        let state = sqlx::query_as::<_, (String, String, i64, String, i64, i64)>(
            "SELECT d.status, e.status, e.retry_count, r.status,
                    (SELECT COUNT(*) FROM dispatch_events WHERE dispatch_id = d.id),
                    (SELECT COUNT(*) FROM auto_queue_entry_transitions WHERE entry_id = e.id)
             FROM task_dispatches d
             JOIN auto_queue_entries e ON e.id = $2
             JOIN auto_queue_runs r ON r.id = $3
             WHERE d.id = $1",
        )
        .bind(&dispatch_id)
        .bind(&entry_id)
        .bind(&run_id)
        .fetch_one(&pool)
        .await
        .expect("load conflict state");
        assert_eq!(outcome, DispatchFailureWriteOutcome::AlreadyTerminal);
        assert_eq!(
            state,
            (
                "completed".to_string(),
                "dispatched".to_string(),
                0,
                "active".to_string(),
                0,
                0,
            )
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn dispatch_failure_result_preserves_legacy_error_shape() {
        let result = dispatch_failure_result("plain transport failure", None);

        assert_eq!(result["error"], "plain transport failure");
        assert!(result.get("message").is_none());
    }

    #[test]
    fn dispatch_failure_result_uses_auth_token_expired_code() {
        let result = dispatch_failure_result(
            "authentication expired; re-authentication required",
            Some("auth_token_expired"),
        );

        assert_eq!(result["error"], "auth_token_expired");
        assert_eq!(
            result["message"],
            "authentication expired; re-authentication required"
        );
    }
}

#[cfg(test)]
mod runtime_completion_policy_tests {
    use super::should_sync_runtime_auto_queue_terminal_entry;

    #[test]
    fn runtime_auto_queue_terminal_sync_matches_dispatch_completion_policy() {
        let normal_result = serde_json::json!({"completion_source": "watcher_streaming_final"});
        let noop_result = serde_json::json!({
            "completion_source": "watcher_streaming_final",
            "work_outcome": "noop",
            "completed_without_changes": true
        });

        assert!(!should_sync_runtime_auto_queue_terminal_entry(
            Some("implementation"),
            &normal_result,
            false
        ));
        assert!(!should_sync_runtime_auto_queue_terminal_entry(
            Some("implementation"),
            &noop_result,
            false
        ));
        assert!(should_sync_runtime_auto_queue_terminal_entry(
            Some("rework"),
            &normal_result,
            true
        ));
        assert!(should_sync_runtime_auto_queue_terminal_entry(
            Some("implementation"),
            &noop_result,
            true
        ));
        assert!(!should_sync_runtime_auto_queue_terminal_entry(
            Some("consultation"),
            &normal_result,
            false
        ));
        // #3605 (T2): scope-assessment must NOT sync the runtime terminal entry,
        // exactly like consultation — otherwise the live turn_bridge/watcher
        // completion path would finalize the bound entry and close the card with
        // no implementation dispatch. review_disabled must not change this.
        for review_disabled in [false, true] {
            assert!(
                !should_sync_runtime_auto_queue_terminal_entry(
                    Some("scope-assessment"),
                    &normal_result,
                    review_disabled,
                ),
                "scope-assessment must not sync runtime terminal entry (review_disabled={review_disabled})"
            );
        }
        // #3594 (T3): plan / plan-review completion (via the live turn_bridge/watcher
        // path) must NOT sync the bound auto-queue entry — the kanban-rules JS
        // fan-out re-dispatches it to the next stage. review_disabled must not change
        // this. Mirror of dispatch_status::should_skip_auto_queue_terminal_sync.
        for dispatch_type in ["plan", "plan-review"] {
            for review_disabled in [false, true] {
                assert!(
                    !should_sync_runtime_auto_queue_terminal_entry(
                        Some(dispatch_type),
                        &normal_result,
                        review_disabled,
                    ),
                    "{dispatch_type} must not sync runtime terminal entry (review_disabled={review_disabled})"
                );
            }
        }
    }
}
