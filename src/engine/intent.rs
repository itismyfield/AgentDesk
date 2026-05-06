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

/// Test-only legacy convenience wrapper for executing intents with a SQLite
/// handle. Production callers must use `execute_intents_with_backends` so PG is
/// explicit and transition intents can route through the PG executor.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn execute_intents(
    db: &crate::db::Db,
    engine: Option<&crate::engine::PolicyEngine>,
    intents: Vec<Intent>,
) -> IntentExecutionResult {
    execute_intents_with_backends(
        Some(db),
        engine.and_then(|engine| engine.pg_pool()),
        engine,
        intents,
    )
}

pub fn execute_intents_with_backends(
    db: Option<&crate::db::Db>,
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
                if let Err(e) = execute_transition(db, pg_pool, engine, &card_id, &from, &to) {
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
                match execute_activate_auto_queue(db, pg_pool, engine, body) {
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "auto-queue activate intent failed");
                        result.errors += 1;
                    }
                }
            }
            Intent::ExecuteSQL { sql, params } => {
                if let Err(e) = execute_sql(db, pg_pool, &sql, &params) {
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
                if let Err(e) = execute_queue_message(db, engine, &target, &content, &bot, &source)
                {
                    tracing::warn!(target, bot, source, error = %e, "queue-message intent failed");
                    result.errors += 1;
                }
            }
            Intent::EmitSupervisorSignal {
                signal_name,
                evidence,
            } => {
                if let Err(e) =
                    execute_emit_supervisor_signal(db, pg_pool, engine, &signal_name, evidence)
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
                if let Err(e) = execute_set_kv(db, pg_pool, &key, &value, ttl_seconds) {
                    tracing::warn!(key, ttl_seconds, error = %e, "set-kv intent failed");
                    result.errors += 1;
                }
            }
            Intent::DeleteKV { key } => {
                if let Err(e) = execute_delete_kv(db, pg_pool, &key) {
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
    db: Option<&crate::db::Db>,
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
            let db = db.cloned();
            let pool = pool.clone();
            let engine = engine.clone();
            let card_id = card_id.to_string();
            let to = to.to_string();
            move |_bridge_pool| async move {
                crate::kanban::transition_status_with_opts_pg(
                    db.as_ref(),
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
    db: Option<&crate::db::Db>,
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
    let dispatch_id = if let Some(pg_pool) =
        pg_pool.or_else(|| engine.and_then(|value| value.pg_pool()))
    {
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
        dispatch_id
    } else {
        let Some(db) = db else {
            anyhow::bail!("sqlite backend is unavailable for create_dispatch intent");
        };
        let engine =
            engine.ok_or_else(|| anyhow::anyhow!("create_dispatch intent requires engine"))?;
        let dispatch = crate::dispatch::create_dispatch(
            db,
            engine,
            card_id,
            agent_id,
            dispatch_type,
            title,
            &context,
        )?;
        dispatch
            .get("id")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("sqlite create_dispatch did not return id"))?
    };

    // #117/#158: Update card_review_state via unified entrypoint
    if dispatch_type == "review-decision" {
        crate::engine::ops::review_state_sync_with_backends(
            db,
            pg_pool.or_else(|| engine.and_then(|value| value.pg_pool())),
            &serde_json::json!({
                "card_id": card_id,
                "state": "suggestion_pending",
                "pending_dispatch_id": dispatch_id,
            })
            .to_string(),
        );
    }

    // Get issue URL for Discord notification
    let issue_url: Option<String> =
        if let Some(pg_pool) = pg_pool.or_else(|| engine.and_then(|value| value.pg_pool())) {
            crate::utils::async_bridge::block_on_pg_result(
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
                        .map_err(|error| {
                            format!("load postgres github_issue_url for {card_id}: {error}")
                        })
                    }
                },
                |error| error,
            )
            .ok()
            .flatten()
        } else {
            #[cfg(all(test, feature = "legacy-sqlite-tests"))]
            {
                db.and_then(|db| {
                    db.separate_conn().ok().and_then(|conn| {
                        conn.query_row(
                            "SELECT github_issue_url FROM kanban_cards WHERE id = ?1",
                            [card_id],
                            |row| row.get(0),
                        )
                        .ok()
                        .flatten()
                    })
                })
            }
            #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
            {
                let _ = db;
                None
            }
        };

    Ok(CreatedDispatch {
        dispatch_id,
        card_id: card_id.to_string(),
        agent_id: agent_id.to_string(),
        dispatch_type: dispatch_type.to_string(),
        issue_url,
    })
}

fn execute_activate_auto_queue(
    db: Option<&crate::db::Db>,
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
            let db = db.cloned();
            let engine = engine.clone();
            let body = body;
            move |_bridge_pool| async move {
                Ok(
                    crate::server::routes::auto_queue::activate_with_bridge_pg(db, engine, body)
                        .await,
                )
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
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    sql: &str,
    params: &[serde_json::Value],
) -> anyhow::Result<()> {
    crate::engine::ops::execute_policy_sql(db, pg_pool, sql, params).map_err(anyhow::Error::msg)?;
    Ok(())
}

fn execute_queue_message(
    db: Option<&crate::db::Db>,
    engine: Option<&crate::engine::PolicyEngine>,
    target: &str,
    content: &str,
    bot: &str,
    source: &str,
) -> anyhow::Result<()> {
    let id = crate::engine::ops::message_ops::queue_message(
        db,
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
    _db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    engine: Option<&crate::engine::PolicyEngine>,
    signal_name: &str,
    evidence: serde_json::Value,
) -> anyhow::Result<()> {
    let engine =
        engine.ok_or_else(|| anyhow::anyhow!("supervisor signal intent requires engine"))?;
    let signal =
        crate::supervisor::SupervisorSignal::try_from(signal_name).map_err(anyhow::Error::msg)?;
    let supervisor = crate::supervisor::RuntimeSupervisor::new(pg_pool.cloned(), engine.clone());
    supervisor
        .emit_signal(signal, evidence)
        .map(|_| ())
        .map_err(anyhow::Error::msg)
}

fn execute_set_kv(
    _db: Option<&crate::db::Db>,
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

fn execute_delete_kv(
    _db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    key: &str,
) -> anyhow::Result<()> {
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;

    fn test_db() -> crate::db::Db {
        let conn = sqlite_test::Connection::open_in_memory().unwrap();
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

    // Removed: `test_execute_set_kv_intent` (SQLite-fallback test).
    // PG-only after #1239: `execute_set_kv` now requires a `pg_pool`. PG-backed
    // coverage lives in the integration test suite that boots a Postgres pool.

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

    fn insert_test_card(conn: &sqlite_test::Connection, card_id: &str) {
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

    fn legacy_review_sync_for_tests(conn: &sqlite_test::Connection, json_str: &str) -> String {
        let params: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let card_id = params["card_id"].as_str().unwrap_or("");
        let state = params["state"].as_str().unwrap_or("");
        let last_verdict = params["last_verdict"].as_str();
        let pending_dispatch_id = params["pending_dispatch_id"].as_str();
        let result = conn.execute(
            "INSERT INTO card_review_state (card_id, state, last_verdict, pending_dispatch_id, updated_at) \
             VALUES (?1, ?2, ?3, ?4, datetime('now')) \
             ON CONFLICT(card_id) DO UPDATE SET \
             state = ?2, \
             last_verdict = COALESCE(?3, last_verdict), \
             pending_dispatch_id = CASE WHEN ?4 IS NOT NULL THEN ?4 WHEN ?2 = 'suggestion_pending' THEN pending_dispatch_id ELSE NULL END, \
             updated_at = datetime('now')",
            sqlite_test::params![card_id, state, last_verdict, pending_dispatch_id],
        );
        match result {
            Ok(n) => format!(r#"{{"ok":true,"rows_affected":{n}}}"#),
            Err(e) => format!(r#"{{"error":"sql error: {}"}}"#, e),
        }
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
    fn test_review_sync_legacy_basic() {
        let db = test_db();
        let conn = db.lock().unwrap();
        insert_test_card(&conn, "card-sync-1");

        let result = legacy_review_sync_for_tests(
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
    fn test_review_sync_legacy_upsert_preserves_fields() {
        let db = test_db();
        let conn = db.lock().unwrap();
        insert_test_card(&conn, "card-upsert");

        // First write: set state + last_verdict
        legacy_review_sync_for_tests(
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
        legacy_review_sync_for_tests(
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
        legacy_review_sync_for_tests(
            &conn,
            &serde_json::json!({
                "card_id": "card-idle",
                "state": "suggestion_pending",
                "pending_dispatch_id": "d-99",
            })
            .to_string(),
        );

        // Transition to idle — pending_dispatch_id must be cleared
        legacy_review_sync_for_tests(
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
