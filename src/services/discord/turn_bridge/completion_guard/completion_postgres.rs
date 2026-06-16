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

fn should_sync_runtime_auto_queue_terminal_entry(
    dispatch_type: Option<&str>,
    _result: &serde_json::Value,
    auto_queue_review_disabled: bool,
) -> bool {
    match dispatch_type {
        Some("consultation") => false,
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

pub(super) fn runtime_pg_reset_linked_auto_queue_entries(dispatch_id: &str) -> bool {
    let dispatch_id = dispatch_id.to_string();
    with_runtime_postgres_result(move |pool| {
        Box::pin(async move {
            let changed = sqlx::query(
                "UPDATE auto_queue_entries
                 SET status = 'pending',
                     dispatch_id = NULL,
                     slot_index = NULL,
                     dispatched_at = NULL,
                     completed_at = NULL
                 WHERE dispatch_id = $1
                   AND status IN ('pending', 'dispatched', 'failed')",
            )
            .bind(&dispatch_id)
            .execute(&pool)
            .await
            .map_err(|error| {
                format!("reset postgres auto_queue_entries for {dispatch_id}: {error}")
            })?
            .rows_affected();
            Ok(changed > 0)
        })
    })
    .unwrap_or(false)
}

pub(super) fn runtime_pg_fail_linked_auto_queue_entries(dispatch_id: &str) -> bool {
    let dispatch_id = dispatch_id.to_string();
    with_runtime_postgres_result(move |pool| {
        Box::pin(async move {
            let changed = sqlx::query(
                "UPDATE auto_queue_entries
                 SET status = 'failed',
                     dispatch_id = NULL,
                     slot_index = NULL,
                     dispatched_at = NULL,
                     completed_at = NOW()
                 WHERE dispatch_id = $1
                   AND status IN ('pending', 'dispatched')",
            )
            .bind(&dispatch_id)
            .execute(&pool)
            .await
            .map_err(|error| {
                format!("mark postgres auto_queue_entries failed for {dispatch_id}: {error}")
            })?
            .rows_affected();
            Ok(changed > 0)
        })
    })
    .unwrap_or(false)
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

pub(super) fn runtime_pg_fail_dispatch_with_result(
    dispatch_id: &str,
    error_msg: &str,
    error_code: Option<&str>,
    reset_auto_queue_entries: bool,
) -> bool {
    let dispatch_id = dispatch_id.to_string();
    let mut fallback_result = dispatch_failure_result(error_msg, error_code);
    fallback_result["fallback"] = serde_json::json!(true);
    let fallback_result = fallback_result.to_string();
    with_runtime_postgres_result(move |pool| {
        Box::pin(async move {
            let mut tx = pool
                .begin()
                .await
                .map_err(|error| format!("begin postgres failure fallback for {dispatch_id}: {error}"))?;

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
                 SET status = 'failed',
                     result = CAST($1 AS jsonb),
                     updated_at = NOW(),
                     last_stuck_alert_at = NULL
                 WHERE id = $2
                   AND status = $3",
            )
            .bind(&fallback_result)
            .bind(&dispatch_id)
            .bind(&current_status)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("update postgres dispatch {dispatch_id} to failed: {error}"))?
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

            sqlx::query(
                "INSERT INTO dispatch_events (
                    dispatch_id,
                    kanban_card_id,
                    dispatch_type,
                    from_status,
                    to_status,
                    transition_source,
                    payload_json
                ) VALUES ($1, $2, $3, $4, 'failed', 'turn_bridge_patch_failure_fallback', CAST($5 AS jsonb))",
            )
            .bind(&dispatch_id)
            .bind(kanban_card_id)
            .bind(dispatch_type)
            .bind(&current_status)
            .bind(&fallback_result)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("record postgres dispatch failure event for {dispatch_id}: {error}"))?;

            if reset_auto_queue_entries {
                sqlx::query(
                    "UPDATE auto_queue_entries
                     SET status = 'pending',
                         dispatch_id = NULL,
                         slot_index = NULL,
                         dispatched_at = NULL,
                         completed_at = NULL
                     WHERE dispatch_id = $1
                       AND status IN ('pending', 'dispatched')",
                )
                .bind(&dispatch_id)
                .execute(&mut *tx)
                .await
                .map_err(|error| format!("reset postgres auto_queue_entries for failed dispatch {dispatch_id}: {error}"))?;
            } else {
                sqlx::query(
                    "UPDATE auto_queue_entries
                     SET status = 'failed',
                         dispatch_id = NULL,
                         slot_index = NULL,
                         dispatched_at = NULL,
                         completed_at = NOW()
                     WHERE dispatch_id = $1
                       AND status IN ('pending', 'dispatched')",
                )
                .bind(&dispatch_id)
                .execute(&mut *tx)
                .await
                .map_err(|error| format!("mark postgres auto_queue_entries failed for dispatch {dispatch_id}: {error}"))?;
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
            .map_err(|error| format!("set postgres reconcile marker for failed dispatch {dispatch_id}: {error}"))?;

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
            .map_err(|error| format!("enqueue postgres failure status reaction for {dispatch_id}: {error}"))?;

            tx.commit()
                .await
                .map_err(|error| format!("commit postgres failure fallback for {dispatch_id}: {error}"))?;
            Ok(true)
        })
    })
    .unwrap_or(false)
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
    db: Option<&crate::db::Db>,
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

    let _ = db;

    false
}

#[cfg(test)]
mod failure_result_tests {
    use super::dispatch_failure_result;

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
    }
}
