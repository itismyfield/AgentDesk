//! Intent types for the JS policy → Rust executor pipeline (#121).
//!
//! JS policy hooks push intents to `agentdesk.__pendingIntents`.
//! After hook returns, Rust drains the array and executes intents in order.
//!
//! Read-only operations (db.query, kanban.getCard) remain synchronous.
//! Mutation operations (setStatus, dispatch.create, db.execute) are deferred.

use serde::{Deserialize, Serialize};

/// A single intent produced by a JS policy hook.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Intent {
    /// Card status transition (replaces agentdesk.kanban.setStatus)
    #[serde(rename = "transition")]
    TransitionCard {
        card_id: String,
        from: String,
        to: String,
    },
    /// Dispatch creation (replaces agentdesk.dispatch.create)
    #[serde(rename = "create_dispatch")]
    CreateDispatch {
        dispatch_id: String,
        card_id: String,
        agent_id: String,
        dispatch_type: String,
        title: String,
    },
    /// Auto-queue activation (used to defer bridge calls out of hook execution).
    #[serde(rename = "activate_auto_queue")]
    ActivateAutoQueue { body: serde_json::Value },
    /// Raw SQL execution (replaces agentdesk.db.execute)
    /// Retained as escape hatch; prefer typed intents above.
    #[serde(rename = "execute_sql")]
    ExecuteSQL {
        sql: String,
        params: Vec<serde_json::Value>,
    },
    /// Enqueue async message (replaces agentdesk.message.queue)
    #[serde(rename = "queue_message")]
    QueueMessage {
        target: String,
        content: String,
        bot: String,
        source: String,
    },
    /// Emit a runtime supervisor signal after the current hook completes.
    #[serde(rename = "emit_supervisor_signal")]
    EmitSupervisorSignal {
        signal_name: String,
        evidence: serde_json::Value,
    },
    /// KV store set (replaces agentdesk.kv.set)
    #[serde(rename = "set_kv")]
    SetKV {
        key: String,
        value: String,
        ttl_seconds: i64,
    },
    /// KV store delete (replaces agentdesk.kv.delete)
    #[serde(rename = "delete_kv")]
    DeleteKV { key: String },
}

/// Result of executing a batch of intents.
pub struct IntentExecutionResult {
    /// Card transitions that were applied (card_id, from, to).
    /// Callers use these to fire transition hooks.
    pub transitions: Vec<(String, String, String)>,
    /// Dispatch IDs that were created. Callers use these for Discord notifications.
    pub created_dispatches: Vec<CreatedDispatch>,
    /// Number of intents that failed (logged, not fatal).
    pub errors: usize,
}

/// Info about a dispatch created by intent execution.
#[allow(dead_code)]
pub struct CreatedDispatch {
    pub dispatch_id: String,
    pub card_id: String,
    pub agent_id: String,
    pub dispatch_type: String,
    pub issue_url: Option<String>,
}

pub fn execute_intents_with_backends(
    pg_pool: Option<&sqlx::PgPool>,
    engine: Option<&crate::engine::PolicyEngine>,
    intents: Vec<Intent>,
) -> IntentExecutionResult {
    let mut result = IntentExecutionResult {
        transitions: Vec::new(),
        created_dispatches: Vec::new(),
        errors: 0,
    };

    if intents.is_empty() {
        return result;
    }

    tracing::info!(intent_count = intents.len(), "executing queued intents");

    for intent in intents {
        match intent {
            Intent::TransitionCard { card_id, from, to } => {
                let intent_span =
                    crate::logging::dispatch_span("intent_transition", None, Some(&card_id), None);
                let _guard = intent_span.enter();
                if let Err(e) = execute_transition(pg_pool, engine, &card_id, &from, &to) {
                    tracing::warn!(from, to, error = %e, "transition intent failed");
                    result.errors += 1;
                } else {
                    result.transitions.push((card_id, from, to));
                }
            }
            Intent::CreateDispatch {
                dispatch_id,
                card_id,
                agent_id,
                dispatch_type,
                title,
            } => {
                let intent_span = crate::logging::dispatch_span(
                    "intent_create_dispatch",
                    Some(&dispatch_id),
                    Some(&card_id),
                    Some(&agent_id),
                );
                let _guard = intent_span.enter();
                match execute_create_dispatch(
                    pg_pool,
                    engine,
                    &dispatch_id,
                    &card_id,
                    &agent_id,
                    &dispatch_type,
                    &title,
                ) {
                    Ok(created) => result.created_dispatches.push(created),
                    Err(e) => {
                        tracing::warn!(dispatch_type, title, error = %e, "create-dispatch intent failed");
                        result.errors += 1;
                    }
                }
            }
            Intent::ActivateAutoQueue { body } => {
                match execute_activate_auto_queue(pg_pool, engine, body) {
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "auto-queue activate intent failed");
                        result.errors += 1;
                    }
                }
            }
            Intent::ExecuteSQL { sql, params } => {
                if let Err(e) = execute_sql(pg_pool, &sql, &params) {
                    tracing::warn!(error = %e, sql, "execute-sql intent failed");
                    result.errors += 1;
                }
            }
            Intent::QueueMessage {
                target,
                content,
                bot,
                source,
            } => {
                if let Err(e) = execute_queue_message(engine, &target, &content, &bot, &source) {
                    tracing::warn!(target, bot, source, error = %e, "queue-message intent failed");
                    result.errors += 1;
                }
            }
            Intent::EmitSupervisorSignal {
                signal_name,
                evidence,
            } => {
                if let Err(e) =
                    execute_emit_supervisor_signal(pg_pool, engine, &signal_name, evidence)
                {
                    tracing::warn!(
                        signal_name,
                        error = %e,
                        "emit-supervisor-signal intent failed"
                    );
                    result.errors += 1;
                }
            }
            Intent::SetKV {
                key,
                value,
                ttl_seconds,
            } => {
                if let Err(e) = execute_set_kv(pg_pool, &key, &value, ttl_seconds) {
                    tracing::warn!(key, ttl_seconds, error = %e, "set-kv intent failed");
                    result.errors += 1;
                }
            }
            Intent::DeleteKV { key } => {
                if let Err(e) = execute_delete_kv(pg_pool, &key) {
                    tracing::warn!(key, error = %e, "delete-kv intent failed");
                    result.errors += 1;
                }
            }
        }
    }

    tracing::info!(
        transition_count = result.transitions.len(),
        created_dispatch_count = result.created_dispatches.len(),
        error_count = result.errors,
        "finished executing queued intents"
    );

    result
}

// ── Individual intent executors ─────────────────────────────────

fn execute_transition(
    pg_pool: Option<&sqlx::PgPool>,
    engine: Option<&crate::engine::PolicyEngine>,
    card_id: &str,
    _expected_from: &str,
    to: &str,
) -> anyhow::Result<()> {
    let pool = pg_pool
        .or_else(|| engine.and_then(|engine| engine.pg_pool()))
        .ok_or_else(|| anyhow::anyhow!("postgres backend is required for transition intent"))?;
    let engine =
        engine.ok_or_else(|| anyhow::anyhow!("transition intent requires a policy engine"))?;
    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        {
            let pool = pool.clone();
            let engine = engine.clone();
            let card_id = card_id.to_string();
            let to = to.to_string();
            move |_bridge_pool| async move {
                crate::kanban::transition_status_with_opts_pg(
                    &pool,
                    &engine,
                    &card_id,
                    &to,
                    "intent_transition",
                    crate::engine::transition::ForceIntent::None,
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
            }
        },
        |error| error,
    );
    result.map_err(anyhow::Error::msg)
}

fn execute_create_dispatch(
    pg_pool: Option<&sqlx::PgPool>,
    engine: Option<&crate::engine::PolicyEngine>,
    pre_id: &str,
    card_id: &str,
    agent_id: &str,
    dispatch_type: &str,
    title: &str,
) -> anyhow::Result<CreatedDispatch> {
    let dispatch_span = crate::logging::dispatch_span(
        "execute_create_dispatch",
        Some(pre_id),
        Some(card_id),
        Some(agent_id),
    );
    let _guard = dispatch_span.enter();
    let context = serde_json::json!({});
    let pg_pool = pg_pool
        .or_else(|| engine.and_then(|value| value.pg_pool()))
        .ok_or_else(|| {
            anyhow::anyhow!("postgres backend is required for create_dispatch intent")
        })?;
    let dispatch_id_input = pre_id.to_string();
    let card_id_input = card_id.to_string();
    let agent_id_input = agent_id.to_string();
    let dispatch_type_input = dispatch_type.to_string();
    let title_input = title.to_string();
    let (dispatch_id, _old_status, _reused) = crate::utils::async_bridge::block_on_pg_result(
        pg_pool,
        move |bridge_pool| async move {
            crate::dispatch::create_dispatch_core_with_id(
                &bridge_pool,
                &dispatch_id_input,
                &card_id_input,
                &agent_id_input,
                &dispatch_type_input,
                &title_input,
                &context,
            )
            .await
        },
        |error| anyhow::anyhow!(error),
    )?;

    // #117/#158: Update card_review_state via unified entrypoint
    if dispatch_type == "review-decision" {
        crate::engine::ops::review_state_sync_with_backends(
            Some(pg_pool),
            &serde_json::json!({
                "card_id": card_id,
                "state": "suggestion_pending",
                "pending_dispatch_id": dispatch_id,
            })
            .to_string(),
        );
    }

    // Get issue URL for Discord notification
    let issue_url: Option<String> = crate::utils::async_bridge::block_on_pg_result(
        pg_pool,
        {
            let card_id = card_id.to_string();
            move |bridge_pool| async move {
                sqlx::query_scalar::<_, Option<String>>(
                    "SELECT github_issue_url
                         FROM kanban_cards
                         WHERE id = $1",
                )
                .bind(&card_id)
                .fetch_optional(&bridge_pool)
                .await
                .map(|value| value.flatten())
                .map_err(|error| format!("load postgres github_issue_url for {card_id}: {error}"))
            }
        },
        |error| error,
    )
    .ok()
    .flatten();

    Ok(CreatedDispatch {
        dispatch_id,
        card_id: card_id.to_string(),
        agent_id: agent_id.to_string(),
        dispatch_type: dispatch_type.to_string(),
        issue_url,
    })
}

fn execute_activate_auto_queue(
    pg_pool: Option<&sqlx::PgPool>,
    engine: Option<&crate::engine::PolicyEngine>,
    body: serde_json::Value,
) -> anyhow::Result<()> {
    let engine =
        engine.ok_or_else(|| anyhow::anyhow!("auto-queue activation intent requires engine"))?;
    let body: crate::server::routes::auto_queue::ActivateBody = serde_json::from_value(body)?;
    let pool = pg_pool.or_else(|| engine.pg_pool()).ok_or_else(|| {
        anyhow::anyhow!("postgres backend is required for auto-queue activation intent")
    })?;
    let (_status, response) = crate::utils::async_bridge::block_on_pg_result(
        pool,
        {
            let engine = engine.clone();
            let body = body;
            move |_bridge_pool| async move {
                match crate::server::routes::auto_queue::activate_with_bridge_pg(engine, body).await
                {
                    Ok(response) => Ok(response),
                    Err(error) => Ok(error.into_json_response()),
                }
            }
        },
        |error| anyhow::anyhow!(error),
    )?;
    if response.0.get("error").is_some() {
        return Err(anyhow::anyhow!(
            "{}",
            response.0["error"]
                .as_str()
                .unwrap_or("auto-queue activation failed")
        ));
    }
    Ok(())
}

fn execute_sql(
    pg_pool: Option<&sqlx::PgPool>,
    sql: &str,
    params: &[serde_json::Value],
) -> anyhow::Result<()> {
    crate::engine::ops::execute_policy_sql(pg_pool, sql, params).map_err(anyhow::Error::msg)?;
    Ok(())
}

fn execute_queue_message(
    engine: Option<&crate::engine::PolicyEngine>,
    target: &str,
    content: &str,
    bot: &str,
    source: &str,
) -> anyhow::Result<()> {
    let id = crate::engine::ops::message_ops::queue_message(
        engine.and_then(|engine| engine.pg_pool()),
        target,
        content,
        bot,
        source,
    )
    .map_err(anyhow::Error::msg)?;
    tracing::info!(
        target,
        bot,
        source,
        message_id = id,
        "queued message intent"
    );
    Ok(())
}

fn execute_emit_supervisor_signal(
    pg_pool: Option<&sqlx::PgPool>,
    engine: Option<&crate::engine::PolicyEngine>,
    signal_name: &str,
    evidence: serde_json::Value,
) -> anyhow::Result<()> {
    let signal =
        crate::supervisor::SupervisorSignal::try_from(signal_name).map_err(anyhow::Error::msg)?;
    signal
        .validate_emit_evidence(&evidence)
        .map_err(anyhow::Error::msg)?;
    let engine =
        engine.ok_or_else(|| anyhow::anyhow!("supervisor signal intent requires engine"))?;
    let supervisor = crate::supervisor::RuntimeSupervisor::new(pg_pool.cloned(), engine.clone());
    supervisor
        .emit_signal(signal, evidence)
        .map(|_| ())
        .map_err(anyhow::Error::msg)
}

fn execute_set_kv(
    pg_pool: Option<&sqlx::PgPool>,
    key: &str,
    value: &str,
    ttl_seconds: i64,
) -> anyhow::Result<()> {
    let pool =
        pg_pool.ok_or_else(|| anyhow::anyhow!("postgres backend is required for set_kv intent"))?;
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        {
            let key = key.to_string();
            let value = value.to_string();
            move |bridge_pool| async move {
                let query = if ttl_seconds > 0 {
                    sqlx::query(
                        "INSERT INTO kv_meta (key, value, expires_at)
                         VALUES ($1, $2, NOW() + ($3 * INTERVAL '1 second'))
                         ON CONFLICT (key) DO UPDATE
                         SET value = EXCLUDED.value,
                             expires_at = EXCLUDED.expires_at",
                    )
                    .bind(&key)
                    .bind(&value)
                    .bind(ttl_seconds)
                    .execute(&bridge_pool)
                    .await
                } else {
                    sqlx::query(
                        "INSERT INTO kv_meta (key, value, expires_at)
                         VALUES ($1, $2, NULL)
                         ON CONFLICT (key) DO UPDATE
                         SET value = EXCLUDED.value,
                             expires_at = EXCLUDED.expires_at",
                    )
                    .bind(&key)
                    .bind(&value)
                    .execute(&bridge_pool)
                    .await
                };
                query
                    .map(|_| ())
                    .map_err(|error| format!("upsert postgres kv_meta {key}: {error}"))
            }
        },
        |error| error,
    )
    .map_err(anyhow::Error::msg)
}

fn execute_delete_kv(pg_pool: Option<&sqlx::PgPool>, key: &str) -> anyhow::Result<()> {
    let pool = pg_pool
        .ok_or_else(|| anyhow::anyhow!("postgres backend is required for delete_kv intent"))?;
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        {
            let key = key.to_string();
            move |bridge_pool| async move {
                sqlx::query("DELETE FROM kv_meta WHERE key = $1")
                    .bind(&key)
                    .execute(&bridge_pool)
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("delete postgres kv_meta {key}: {error}"))
            }
        },
        |error| error,
    )
    .map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn emit_supervisor_signal_intent_rejects_audit_only_without_engine_lookup() {
        let err = execute_emit_supervisor_signal(
            None,
            None,
            "StaleInflight",
            json!({ "session_key": "session-1" }),
        )
        .expect_err("audit-only signal without acknowledgement should fail");
        let msg = err.to_string();

        assert!(msg.contains("audit-only"));
        assert!(msg.contains("supervisor_audit_only"));
        assert!(!msg.contains("requires engine"));
    }
}
