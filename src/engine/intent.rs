//! Intent types for the JS policy → Rust executor pipeline (#121).
//!
//! JS policy hooks push intents to `agentdesk.__pendingIntents`.
//! After hook returns, Rust drains the array and executes intents in order.
//!
//! Read-only operations (db.query, kanban.getCard) remain synchronous.
//! Mutation operations (setStatus, dispatch.create, db.execute) are deferred.

use serde::{Deserialize, Serialize};
use tracing::warn;

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

/// Execute a batch of intents against the database.
///
/// Intents are applied in order. Failures are logged and skipped (fail-soft)
/// to prevent one bad intent from blocking the rest.
pub fn execute_intents(
    db: &crate::db::Db,
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
                if let Err(e) = execute_transition(db, &card_id, &from, &to) {
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
                    db,
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
                match execute_activate_auto_queue(db, engine, body) {
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "auto-queue activate intent failed");
                        result.errors += 1;
                    }
                }
            }
            Intent::ExecuteSQL { sql, params } => {
                if let Err(e) = execute_sql(db, &sql, &params) {
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
                if let Err(e) = execute_queue_message(db, &target, &content, &bot, &source) {
                    tracing::warn!(target, bot, source, error = %e, "queue-message intent failed");
                    result.errors += 1;
                }
            }
            Intent::EmitSupervisorSignal {
                signal_name,
                evidence,
            } => {
                if let Err(e) = execute_emit_supervisor_signal(db, engine, &signal_name, evidence) {
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
                if let Err(e) = execute_set_kv(db, &key, &value, ttl_seconds) {
                    tracing::warn!(key, ttl_seconds, error = %e, "set-kv intent failed");
                    result.errors += 1;
                }
            }
            Intent::DeleteKV { key } => {
                if let Err(e) = execute_delete_kv(db, &key) {
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
    db: &crate::db::Db,
    card_id: &str,
    expected_from: &str,
    to: &str,
) -> anyhow::Result<()> {
    let transition_span =
        crate::logging::dispatch_span("execute_transition", None, Some(card_id), None);
    let _guard = transition_span.enter();
    let conn = db.separate_conn()?;

    // Verify current status matches expected
    let current: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .map_err(|_| anyhow::anyhow!("card not found: {card_id}"))?;

    if current != expected_from {
        // Status changed between intent push and execution — skip
        tracing::info!(
            expected_from,
            current,
            to,
            "skipping transition intent due to stale source status"
        );
        return Ok(());
    }

    if current == to {
        return Ok(()); // no-op
    }

    // Pipeline-driven validation and clock fields
    crate::pipeline::ensure_loaded();
    let pipeline =
        crate::pipeline::try_get().ok_or_else(|| anyhow::anyhow!("pipeline not loaded"))?;

    // Terminal guard
    if pipeline.is_terminal(&current) {
        return Err(anyhow::anyhow!(
            "cannot revert terminal card {card_id} from {current} to {to}"
        ));
    }

    // Clock fields
    let clock_extra = match pipeline.clock_for_state(to) {
        Some(clock) if clock.mode.as_deref() == Some("coalesce") => {
            format!(", {} = COALESCE({}, datetime('now'))", clock.set, clock.set)
        }
        Some(clock) => format!(", {} = datetime('now')", clock.set),
        None => String::new(),
    };

    // Terminal cleanup
    let terminal_cleanup = if pipeline.is_terminal(to) {
        ", review_status = NULL, suggestion_pending_at = NULL, review_entered_at = NULL, awaiting_dod_at = NULL, blocked_reason = NULL, review_round = NULL, deferred_dod_json = NULL"
    } else {
        ""
    };

    let extra = format!("{clock_extra}{terminal_cleanup}");
    let sql = format!(
        "UPDATE kanban_cards SET status = ?1, updated_at = datetime('now'){extra} WHERE id = ?2"
    );
    conn.execute(&sql, libsql_rusqlite::params![to, card_id])?;

    // Auto-queue sync for terminal states
    if pipeline.is_terminal(to) {
        crate::engine::ops::sync_auto_queue_terminal_on_conn(&conn, card_id);
    }

    // #117/#158: Sync canonical review state via unified entrypoint
    let has_hooks = pipeline
        .hooks_for_state(to)
        .map_or(false, |h| !h.on_enter.is_empty() || !h.on_exit.is_empty());
    let is_review_enter = pipeline
        .hooks_for_state(to)
        .map_or(false, |h| h.on_enter.iter().any(|n| n == "OnReviewEnter"));
    if pipeline.is_terminal(to) || !has_hooks {
        crate::engine::ops::review_state_sync_on_conn(
            &conn,
            &serde_json::json!({"card_id": card_id, "state": "idle"}).to_string(),
        );
    } else if is_review_enter {
        crate::engine::ops::review_state_sync_on_conn(
            &conn,
            &serde_json::json!({"card_id": card_id, "state": "reviewing"}).to_string(),
        );
    }

    tracing::info!(from = expected_from, to, "applied transition intent");
    Ok(())
}

fn execute_create_dispatch(
    db: &crate::db::Db,
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
    // Delegate to the authoritative dispatch creation path.
    // create_dispatch_core generates its own UUID — we override by using a
    // variant that accepts a pre-assigned ID.
    let context = serde_json::json!({});
    let (dispatch_id, _old_status, _reused) = crate::dispatch::create_dispatch_core_with_id(
        db,
        pre_id,
        card_id,
        agent_id,
        dispatch_type,
        title,
        &context,
    )?;

    // #117/#158: Update card_review_state via unified entrypoint
    if dispatch_type == "review-decision" {
        crate::engine::ops::review_state_sync(
            db,
            &serde_json::json!({
                "card_id": card_id,
                "state": "suggestion_pending",
                "pending_dispatch_id": dispatch_id,
            })
            .to_string(),
        );
    }

    // Get issue URL for Discord notification
    let issue_url: Option<String> = db.separate_conn().ok().and_then(|conn| {
        conn.query_row(
            "SELECT github_issue_url FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
        .flatten()
    });

    Ok(CreatedDispatch {
        dispatch_id,
        card_id: card_id.to_string(),
        agent_id: agent_id.to_string(),
        dispatch_type: dispatch_type.to_string(),
        issue_url,
    })
}

fn execute_activate_auto_queue(
    db: &crate::db::Db,
    engine: Option<&crate::engine::PolicyEngine>,
    body: serde_json::Value,
) -> anyhow::Result<()> {
    let engine =
        engine.ok_or_else(|| anyhow::anyhow!("auto-queue activation intent requires engine"))?;
    let body: crate::server::routes::auto_queue::ActivateBody = serde_json::from_value(body)?;
    let deps = crate::server::routes::auto_queue::AutoQueueActivateDeps::for_bridge(
        db.clone(),
        engine.clone(),
    );
    let (_status, response) = crate::server::routes::auto_queue::activate_with_deps(&deps, body);
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

fn json_to_sqlite(val: &serde_json::Value) -> libsql_rusqlite::types::Value {
    match val {
        serde_json::Value::Null => libsql_rusqlite::types::Value::Null,
        serde_json::Value::Bool(b) => {
            libsql_rusqlite::types::Value::Integer(if *b { 1 } else { 0 })
        }
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                libsql_rusqlite::types::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                libsql_rusqlite::types::Value::Real(f)
            } else {
                libsql_rusqlite::types::Value::Null
            }
        }
        serde_json::Value::String(s) => libsql_rusqlite::types::Value::Text(s.clone()),
        _ => libsql_rusqlite::types::Value::Text(val.to_string()),
    }
}

fn execute_sql(db: &crate::db::Db, sql: &str, params: &[serde_json::Value]) -> anyhow::Result<()> {
    if let Some(violation) = crate::engine::sql_guard::detect_core_table_write(sql) {
        warn!("{}", violation.warning_message("ExecuteSQL intent", sql));
        return Err(anyhow::anyhow!(violation.error_message()));
    }

    let conn = db.separate_conn()?;
    let bind: Vec<libsql_rusqlite::types::Value> = params.iter().map(json_to_sqlite).collect();
    let params_ref: Vec<&dyn libsql_rusqlite::types::ToSql> = bind
        .iter()
        .map(|v| v as &dyn libsql_rusqlite::types::ToSql)
        .collect();
    conn.execute(sql, params_ref.as_slice())?;
    Ok(())
}

fn execute_queue_message(
    db: &crate::db::Db,
    target: &str,
    content: &str,
    bot: &str,
    source: &str,
) -> anyhow::Result<()> {
    let conn = db.separate_conn()?;
    conn.execute(
        "INSERT INTO message_outbox (target, content, bot, source) VALUES (?1, ?2, ?3, ?4)",
        libsql_rusqlite::params![target, content, bot, source],
    )?;
    let id = conn.last_insert_rowid();
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
    db: &crate::db::Db,
    engine: Option<&crate::engine::PolicyEngine>,
    signal_name: &str,
    evidence: serde_json::Value,
) -> anyhow::Result<()> {
    let engine =
        engine.ok_or_else(|| anyhow::anyhow!("supervisor signal intent requires engine"))?;
    let signal =
        crate::supervisor::SupervisorSignal::try_from(signal_name).map_err(anyhow::Error::msg)?;
    let supervisor = crate::supervisor::RuntimeSupervisor::new(db.clone(), engine.clone());
    supervisor
        .emit_signal(signal, evidence)
        .map(|_| ())
        .map_err(anyhow::Error::msg)
}

fn execute_set_kv(
    db: &crate::db::Db,
    key: &str,
    value: &str,
    ttl_seconds: i64,
) -> anyhow::Result<()> {
    let conn = db.separate_conn()?;
    if ttl_seconds > 0 {
        conn.execute(
            &format!(
                "INSERT OR REPLACE INTO kv_meta (key, value, expires_at) VALUES (?1, ?2, datetime('now', '+{ttl_seconds} seconds'))"
            ),
            libsql_rusqlite::params![key, value],
        )?;
    } else {
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value, expires_at) VALUES (?1, ?2, NULL)",
            libsql_rusqlite::params![key, value],
        )?;
    }
    Ok(())
}

fn execute_delete_kv(db: &crate::db::Db, key: &str) -> anyhow::Result<()> {
    let conn = db.separate_conn()?;
    conn.execute("DELETE FROM kv_meta WHERE key = ?1", [key])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> crate::db::Db {
        let conn = libsql_rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    #[test]
    fn test_execute_empty_intents() {
        let db = test_db();
        let result = execute_intents(&db, None, vec![]);
        assert!(result.transitions.is_empty());
        assert!(result.created_dispatches.is_empty());
        assert_eq!(result.errors, 0);
    }

    #[test]
    fn test_execute_sql_intent() {
        let db = test_db();
        let intents = vec![Intent::ExecuteSQL {
            sql: "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('test', 'hello')".into(),
            params: vec![],
        }];
        let result = execute_intents(&db, None, intents);
        assert_eq!(result.errors, 0);

        let conn = db.lock().unwrap();
        let val: String = conn
            .query_row("SELECT value FROM kv_meta WHERE key = 'test'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(val, "hello");
    }

    #[test]
    fn test_execute_set_kv_intent() {
        let db = test_db();
        let intents = vec![Intent::SetKV {
            key: "mykey".into(),
            value: "myval".into(),
            ttl_seconds: 0,
        }];
        let result = execute_intents(&db, None, intents);
        assert_eq!(result.errors, 0);

        let conn = db.lock().unwrap();
        let val: String = conn
            .query_row("SELECT value FROM kv_meta WHERE key = 'mykey'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(val, "myval");
    }

    #[test]
    fn test_execute_queue_message_intent() {
        let db = test_db();
        let intents = vec![Intent::QueueMessage {
            target: "channel:123".into(),
            content: "hello".into(),
            bot: "announce".into(),
            source: "system".into(),
        }];
        let result = execute_intents(&db, None, intents);
        assert_eq!(result.errors, 0);

        let conn = db.lock().unwrap();
        let content: String = conn
            .query_row(
                "SELECT content FROM message_outbox ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(content, "hello");
    }

    #[test]
    fn test_blocked_status_update_sql() {
        let db = test_db();
        let intents = vec![Intent::ExecuteSQL {
            sql: "UPDATE kanban_cards SET status = 'done' WHERE id = 'x'".into(),
            params: vec![],
        }];
        let result = execute_intents(&db, None, intents);
        assert_eq!(result.errors, 1);
    }

    #[test]
    fn test_transition_card_not_found() {
        let db = test_db();
        let intents = vec![Intent::TransitionCard {
            card_id: "nonexistent".into(),
            from: "requested".into(),
            to: "in_progress".into(),
        }];
        let result = execute_intents(&db, None, intents);
        assert_eq!(result.errors, 1);
    }

    #[test]
    fn test_blocked_review_status_update_sql() {
        let db = test_db();
        let intents = vec![Intent::ExecuteSQL {
            sql: "UPDATE kanban_cards SET review_status = 'rework' WHERE id = 'x'".into(),
            params: vec![],
        }];
        let result = execute_intents(&db, None, intents);
        assert_eq!(result.errors, 1);
    }

    #[test]
    fn test_blocked_latest_dispatch_id_update_sql() {
        let db = test_db();
        let intents = vec![Intent::ExecuteSQL {
            sql: "UPDATE kanban_cards SET latest_dispatch_id = 'abc' WHERE id = 'x'".into(),
            params: vec![],
        }];
        let result = execute_intents(&db, None, intents);
        assert_eq!(result.errors, 1);
    }

    #[test]
    fn test_blocked_task_dispatches_delete_sql() {
        let db = test_db();
        let intents = vec![Intent::ExecuteSQL {
            sql: "DELETE FROM task_dispatches WHERE id = 'dispatch-1'".into(),
            params: vec![],
        }];
        let result = execute_intents(&db, None, intents);
        assert_eq!(result.errors, 1);
    }

    // ── #158: card_review_state guard tests ─────────────────────

    fn insert_test_card(conn: &libsql_rusqlite::Connection, card_id: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Test', '111')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at) \
             VALUES (?1, 'Test', 'in_progress', 'agent-1', datetime('now'), datetime('now'))",
            [card_id],
        ).unwrap();
    }

    #[test]
    fn test_blocked_card_review_state_insert_sql() {
        let db = test_db();
        let intents = vec![Intent::ExecuteSQL {
            sql: "INSERT INTO card_review_state (card_id, state) VALUES ('x', 'idle')".into(),
            params: vec![],
        }];
        let result = execute_intents(&db, None, intents);
        assert_eq!(result.errors, 1);
    }

    #[test]
    fn test_blocked_card_review_state_update_sql() {
        let db = test_db();
        let intents = vec![Intent::ExecuteSQL {
            sql: "UPDATE card_review_state SET state = 'reviewing' WHERE card_id = 'x'".into(),
            params: vec![],
        }];
        let result = execute_intents(&db, None, intents);
        assert_eq!(result.errors, 1);
    }

    #[test]
    fn test_review_state_sync_on_conn_basic() {
        let db = test_db();
        let conn = db.lock().unwrap();
        insert_test_card(&conn, "card-sync-1");

        let result = crate::engine::ops::review_state_sync_on_conn(
            &conn,
            &serde_json::json!({"card_id": "card-sync-1", "state": "reviewing"}).to_string(),
        );
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["ok"].as_bool().unwrap());

        let state: String = conn
            .query_row(
                "SELECT state FROM card_review_state WHERE card_id = 'card-sync-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "reviewing");
    }

    #[test]
    fn test_review_state_sync_on_conn_upsert_preserves_fields() {
        let db = test_db();
        let conn = db.lock().unwrap();
        insert_test_card(&conn, "card-upsert");

        // First write: set state + last_verdict
        crate::engine::ops::review_state_sync_on_conn(
            &conn,
            &serde_json::json!({
                "card_id": "card-upsert",
                "state": "suggestion_pending",
                "last_verdict": "improve",
                "pending_dispatch_id": "d-1",
            })
            .to_string(),
        );

        // Second write: update state only — last_verdict should be preserved via COALESCE
        crate::engine::ops::review_state_sync_on_conn(
            &conn,
            &serde_json::json!({"card_id": "card-upsert", "state": "rework_pending"}).to_string(),
        );

        let (state, verdict): (String, String) = conn
            .query_row(
                "SELECT state, last_verdict FROM card_review_state WHERE card_id = 'card-upsert'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "rework_pending");
        assert_eq!(verdict, "improve");
    }

    #[test]
    fn test_review_state_sync_idle_clears_pending_dispatch() {
        let db = test_db();
        let conn = db.lock().unwrap();
        insert_test_card(&conn, "card-idle");

        // Set up a suggestion_pending state with dispatch
        crate::engine::ops::review_state_sync_on_conn(
            &conn,
            &serde_json::json!({
                "card_id": "card-idle",
                "state": "suggestion_pending",
                "pending_dispatch_id": "d-99",
            })
            .to_string(),
        );

        // Transition to idle — pending_dispatch_id must be cleared
        crate::engine::ops::review_state_sync_on_conn(
            &conn,
            &serde_json::json!({"card_id": "card-idle", "state": "idle"}).to_string(),
        );

        let dispatch_id: Option<String> = conn
            .query_row(
                "SELECT pending_dispatch_id FROM card_review_state WHERE card_id = 'card-idle'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            dispatch_id.is_none(),
            "idle state must clear pending_dispatch_id"
        );
    }

    // #158: ExecuteSQL guard blocks direct card_review_state INSERT OR REPLACE
    #[test]
    fn test_blocked_card_review_state_insert_or_replace_sql() {
        let db = test_db();
        let intents = vec![Intent::ExecuteSQL {
            sql: "INSERT OR REPLACE INTO card_review_state (card_id, state) VALUES ('c1', 'idle')"
                .into(),
            params: vec![],
        }];
        let result = execute_intents(&db, None, intents);
        assert_eq!(result.errors, 1);
    }

    // #158: ExecuteSQL guard blocks direct card_review_state REPLACE INTO
    #[test]
    fn test_blocked_card_review_state_replace_into_sql() {
        let db = test_db();
        let intents = vec![Intent::ExecuteSQL {
            sql: "REPLACE INTO card_review_state (card_id, state) VALUES ('c1', 'idle')".into(),
            params: vec![],
        }];
        let result = execute_intents(&db, None, intents);
        assert_eq!(result.errors, 1);
    }

    // #158: ExecuteSQL guard blocks direct card_review_state DELETE
    #[test]
    fn test_blocked_card_review_state_delete_sql() {
        let db = test_db();
        let intents = vec![Intent::ExecuteSQL {
            sql: "DELETE FROM card_review_state WHERE card_id = 'c1'".into(),
            params: vec![],
        }];
        let result = execute_intents(&db, None, intents);
        assert_eq!(result.errors, 1);
    }
}
