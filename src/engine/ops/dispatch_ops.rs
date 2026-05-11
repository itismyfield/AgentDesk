use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde_json::json;
use sqlx::PgPool;

// ── Dispatch ops ────────────────────────────────────────────────
//
// agentdesk.dispatch.create(cardId, agentId, dispatchType, title, context?) → dispatchId
// Creates a task_dispatch row + updates kanban card to "requested".
// Discord notification is handled by posting to the local /api/discord/send endpoint.

pub(super) fn register_dispatch_ops<'js>(
    ctx: &Ctx<'js>,
    #[cfg(all(test, feature = "legacy-sqlite-tests"))] db: Option<Db>,
    pg_pool: Option<PgPool>,
) -> JsResult<()> {
    #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
    let db: Option<Db> = None;

    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let dispatch_obj = Object::new(ctx.clone())?;

    // #248: __dispatch_create_raw(card_id, agent_id, dispatch_type, title, context_json)
    // -> json_string. Synchronous PG INSERT — no deferred intent.
    let db_create = db.clone();
    let pg_create = pg_pool.clone();
    dispatch_obj.set(
        "__create_raw",
        Function::new(
            ctx.clone(),
            rquickjs::function::MutFn::from(
                move |card_id: String,
                      agent_id: String,
                      dispatch_type: String,
                      title: String,
                      context_json: String|
                      -> String {
                    dispatch_create_raw(
                        db_create.as_ref(),
                        pg_create.as_ref(),
                        &card_id,
                        &agent_id,
                        &dispatch_type,
                        &title,
                        &context_json,
                    )
                },
            ),
        )?,
    )?;

    // __mark_failed_raw(dispatch_id, reason) → json_string
    // Marks a dispatch as failed. Used by timeout handlers.
    let db_mf = db.clone();
    let pg_mf = pg_pool.clone();
    dispatch_obj.set(
        "__mark_failed_raw",
        Function::new(
            ctx.clone(),
            move |dispatch_id: String, reason: String| -> String {
                if let Some(pool) = pg_mf.as_ref() {
                    let reason_json = json!({ "reason": reason });
                    return dispatch_set_status_raw_pg(
                        pool,
                        &dispatch_id,
                        "failed",
                        Some(reason_json),
                        "js_dispatch_mark_failed_raw",
                        false,
                    );
                }
                if let Some(db) = db_mf.as_ref() {
                    let reason_json = json!({ "reason": reason });
                    return dispatch_set_status_raw_sqlite_test(
                        db,
                        &dispatch_id,
                        "failed",
                        Some(reason_json),
                        "js_dispatch_mark_failed_raw",
                        false,
                    );
                }
                r#"{"error":"backend unavailable for dispatch.markFailed"}"#.to_string()
            },
        )?,
    )?;

    // __mark_completed_raw(dispatch_id, result_json) → json_string
    // Marks a dispatch as completed. Used by orphan recovery.
    let db_mc = db.clone();
    let pg_mc = pg_pool.clone();
    dispatch_obj.set(
        "__mark_completed_raw",
        Function::new(
            ctx.clone(),
            move |dispatch_id: String, result_json: String| -> String {
                if let Some(pool) = pg_mc.as_ref() {
                    let parsed_result = serde_json::from_str::<serde_json::Value>(&result_json)
                        .unwrap_or_else(|_| serde_json::json!({ "raw_result": result_json }));
                    return dispatch_set_status_raw_pg(
                        pool,
                        &dispatch_id,
                        "completed",
                        Some(parsed_result),
                        "js_dispatch_mark_completed_raw",
                        true,
                    );
                }
                if let Some(db) = db_mc.as_ref() {
                    let parsed_result = serde_json::from_str::<serde_json::Value>(&result_json)
                        .unwrap_or_else(|_| serde_json::json!({ "raw_result": result_json }));
                    return dispatch_set_status_raw_sqlite_test(
                        db,
                        &dispatch_id,
                        "completed",
                        Some(parsed_result),
                        "js_dispatch_mark_completed_raw",
                        true,
                    );
                }
                r#"{"error":"backend unavailable for dispatch.markCompleted"}"#.to_string()
            },
        )?,
    )?;

    // __has_active_work_raw(card_id) → json_string {"count":N}
    // Checks if a card has active implementation/rework dispatches.
    let db_aw = db.clone();
    let pg_aw = pg_pool.clone();
    dispatch_obj.set(
        "__has_active_work_raw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            if let Some(pool) = pg_aw.as_ref() {
                return dispatch_has_active_work_raw_pg(pool, &card_id);
            }
            if let Some(db) = db_aw.as_ref() {
                return dispatch_has_active_work_raw_sqlite_test(db, &card_id);
            }
            r#"{"error":"backend unavailable for dispatch.hasActiveWork"}"#.to_string()
        })?,
    )?;

    // __set_retry_count_raw(dispatch_id, count) → json_string
    // Updates retry_count for auto-retry tracking.
    let db_rc = db;
    let pg_rc = pg_pool;
    dispatch_obj.set(
        "__set_retry_count_raw",
        Function::new(
            ctx.clone(),
            move |dispatch_id: String, count: i32| -> String {
                if let Some(pool) = pg_rc.as_ref() {
                    return dispatch_set_retry_count_raw_pg(pool, &dispatch_id, count);
                }
                if let Some(db) = db_rc.as_ref() {
                    return dispatch_set_retry_count_raw_sqlite_test(db, &dispatch_id, count);
                }
                r#"{"error":"backend unavailable for dispatch.setRetryCount"}"#.to_string()
            },
        )?,
    )?;

    ad.set("dispatch", dispatch_obj)?;

    // JS wrapper — synchronous dispatch creation with Rust-side validation/INSERT
    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            agentdesk.dispatch.create = function(cardId, agentId, dispatchType, title, context) {
                var dt = dispatchType || "implementation";
                var t = title || "Dispatch";
                var ctxJson = JSON.stringify(context || {});
                // #248: Synchronous DB INSERT — no deferred intent.
                // Validation + INSERT happen atomically in Rust.
                var result = JSON.parse(
                    agentdesk.dispatch.__create_raw(cardId, agentId, dt, t, ctxJson)
                );
                if (result.error) throw new Error(result.error);
                var dispatchId = result.dispatch_id;
                return dispatchId;
            };
            agentdesk.dispatch.markFailed = function(dispatchId, reason) {
                var result = JSON.parse(
                    agentdesk.dispatch.__mark_failed_raw(dispatchId, reason || "")
                );
                if (result.error) throw new Error(result.error);
                if (result.rows_affected === 0) {
                    agentdesk.log.warn("[dispatch.markFailed] no rows affected for " + dispatchId + " — already terminal or missing");
                }
                return result;
            };
            agentdesk.dispatch.markCompleted = function(dispatchId, resultJson) {
                var result = JSON.parse(
                    agentdesk.dispatch.__mark_completed_raw(dispatchId, resultJson || "{}")
                );
                if (result.error) throw new Error(result.error);
                if (result.rows_affected === 0) {
                    agentdesk.log.warn("[dispatch.markCompleted] no rows affected for " + dispatchId + " — already terminal or missing");
                }
                return result;
            };
            agentdesk.dispatch.setRetryCount = function(dispatchId, count) {
                var result = JSON.parse(
                    agentdesk.dispatch.__set_retry_count_raw(dispatchId, count)
                );
                if (result.error) throw new Error(result.error);
                if (result.rows_affected === 0) {
                    agentdesk.log.warn("[dispatch.setRetryCount] no rows affected for " + dispatchId + " — missing");
                }
                return result;
            };
            agentdesk.dispatch.hasActiveWork = function(cardId) {
                var result = JSON.parse(agentdesk.dispatch.__has_active_work_raw(cardId));
                if (result.error) throw new Error(result.error);
                return result.count > 0;
            };
        })();
    "#,
    )?;

    Ok(())
}

/// #248/#249: Synchronous dispatch creation — validates and inserts into DB
/// immediately. The notify outbox row is now inserted atomically inside
/// the dispatch-core path, so no JS-side outbox buffering is needed.
fn dispatch_create_raw_pg(
    pg_pool: &PgPool,
    card_id: &str,
    agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context_json: &str,
) -> String {
    let context: serde_json::Value = match serde_json::from_str(context_json) {
        Ok(value) => value,
        Err(e) => {
            return format!(
                r#"{{"error":"invalid dispatch context JSON: {}"}}"#,
                e.to_string().replace('"', "'")
            );
        }
    };
    let options = crate::dispatch::DispatchCreateOptions {
        skip_outbox: false,
        sidecar_dispatch: context
            .get("sidecar_dispatch")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
            || context
                .get("phase_gate")
                .and_then(|value| value.as_object())
                .is_some(),
    };
    let card_id_owned = card_id.to_string();
    let agent_id_owned = agent_id.to_string();
    let dispatch_type_owned = dispatch_type.to_string();
    let title_owned = title.to_string();
    match crate::utils::async_bridge::block_on_pg_result(
        pg_pool,
        move |bridge_pool| async move {
            crate::dispatch::create_dispatch_core_with_options(
                &bridge_pool,
                &card_id_owned,
                &agent_id_owned,
                &dispatch_type_owned,
                &title_owned,
                &context,
                options,
            )
            .await
        },
        |error| anyhow::anyhow!(error),
    ) {
        Ok((dispatch_id, _old_status, reused)) => {
            if reused {
                return format!(r#"{{"dispatch_id":"{dispatch_id}","reused":true}}"#);
            }
            if dispatch_type == "review-decision" {
                let card_id_state = card_id.to_string();
                let dispatch_id_state = dispatch_id.clone();
                if let Err(error) = crate::utils::async_bridge::block_on_pg_result(
                    pg_pool,
                    move |bridge_pool| async move {
                        sqlx::query(
                            "INSERT INTO card_review_state (
                                card_id,
                                state,
                                pending_dispatch_id,
                                updated_at
                             ) VALUES ($1, 'suggestion_pending', $2, NOW())
                             ON CONFLICT (card_id) DO UPDATE
                             SET state = 'suggestion_pending',
                                 pending_dispatch_id = EXCLUDED.pending_dispatch_id,
                                 updated_at = NOW()",
                        )
                        .bind(&card_id_state)
                        .bind(&dispatch_id_state)
                        .execute(&bridge_pool)
                        .await
                        .map(|_| ())
                        .map_err(anyhow::Error::from)
                    },
                    |error| anyhow::anyhow!(error),
                ) {
                    return format!(r#"{{"error":"{}"}}"#, error.to_string().replace('"', "'"));
                }
            }
            format!(r#"{{"dispatch_id":"{dispatch_id}"}}"#)
        }
        Err(e) => {
            format!(r#"{{"error":"{}"}}"#, e.to_string().replace('"', "'"))
        }
    }
}

fn dispatch_create_raw(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    card_id: &str,
    agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context_json: &str,
) -> String {
    if let Some(pool) = pg_pool {
        return dispatch_create_raw_pg(pool, card_id, agent_id, dispatch_type, title, context_json);
    }
    if let Some(db) = db {
        return dispatch_create_raw_sqlite_test(
            db,
            card_id,
            agent_id,
            dispatch_type,
            title,
            context_json,
        );
    }
    r#"{"error":"backend unavailable for dispatch.create in JS hook"}"#.to_string()
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn dispatch_create_raw_sqlite_test(
    db: &Db,
    card_id: &str,
    agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context_json: &str,
) -> String {
    let context: serde_json::Value = match serde_json::from_str(context_json) {
        Ok(value) => value,
        Err(error) => {
            return format!(
                r#"{{"error":"invalid dispatch context JSON: {}"}}"#,
                error.to_string().replace('"', "'")
            );
        }
    };
    let options = crate::dispatch::DispatchCreateOptions {
        skip_outbox: false,
        sidecar_dispatch: context
            .get("sidecar_dispatch")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
            || context
                .get("phase_gate")
                .and_then(|value| value.as_object())
                .is_some(),
    };
    match crate::dispatch::create_dispatch_record_sqlite_test(
        db,
        card_id,
        agent_id,
        dispatch_type,
        title,
        &context,
        options,
    ) {
        Ok((dispatch_id, _old_status, reused)) => {
            if dispatch_type == "review-decision" {
                let conn = match db.separate_conn() {
                    Ok(conn) => conn,
                    Err(error) => {
                        return format!(
                            r#"{{"error":"open sqlite connection for dispatch.create: {}"}}"#,
                            error.to_string().replace('"', "'")
                        );
                    }
                };
                if let Err(error) = conn.execute(
                    "INSERT INTO card_review_state (
                        card_id,
                        state,
                        pending_dispatch_id,
                        updated_at
                     ) VALUES (?1, 'suggestion_pending', ?2, datetime('now'))
                     ON CONFLICT(card_id) DO UPDATE
                     SET state = 'suggestion_pending',
                         pending_dispatch_id = excluded.pending_dispatch_id,
                         updated_at = datetime('now')",
                    sqlite_test::params![card_id, dispatch_id],
                ) {
                    return format!(r#"{{"error":"{}"}}"#, error.to_string().replace('"', "'"));
                }
            }
            if reused {
                return format!(r#"{{"dispatch_id":"{dispatch_id}","reused":true}}"#);
            }
            format!(r#"{{"dispatch_id":"{dispatch_id}"}}"#)
        }
        Err(error) => {
            format!(r#"{{"error":"{}"}}"#, error.to_string().replace('"', "'"))
        }
    }
}

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
fn dispatch_create_raw_sqlite_test(
    _db: &Db,
    _card_id: &str,
    _agent_id: &str,
    _dispatch_type: &str,
    _title: &str,
    _context_json: &str,
) -> String {
    r#"{"error":"sqlite backend is unavailable for dispatch.create in production"}"#.to_string()
}

fn dispatch_has_active_work_raw_pg(pool: &PgPool, card_id: &str) -> String {
    let card_id = card_id.to_string();
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*)
                 FROM task_dispatches
                 WHERE kanban_card_id = $1
                   AND dispatch_type IN ('implementation', 'rework')
                   AND status IN ('pending', 'dispatched')",
            )
            .bind(&card_id)
            .fetch_one(&bridge_pool)
            .await
            .map_err(|error| format!("count postgres active dispatches for {card_id}: {error}"))
        },
        |error| format!(r#"{{"error":"{error}"}}"#),
    ) {
        Ok(count) => format!(r#"{{"count":{count}}}"#),
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn dispatch_has_active_work_raw_sqlite_test(db: &Db, card_id: &str) -> String {
    let conn = match db.separate_conn() {
        Ok(conn) => conn,
        Err(error) => {
            return format!(r#"{{"error":"{}"}}"#, error.to_string().replace('"', "'"));
        }
    };
    match conn.query_row(
        "SELECT COUNT(*)
         FROM task_dispatches
         WHERE kanban_card_id = ?1
           AND dispatch_type IN ('implementation', 'rework')
           AND status IN ('pending', 'dispatched')",
        [card_id],
        |row| row.get::<_, i64>(0),
    ) {
        Ok(count) => format!(r#"{{"count":{count}}}"#),
        Err(error) => format!(r#"{{"error":"{}"}}"#, error.to_string().replace('"', "'")),
    }
}

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
fn dispatch_has_active_work_raw_sqlite_test(_db: &Db, _card_id: &str) -> String {
    r#"{"error":"sqlite backend is unavailable for dispatch.hasActiveWork in production"}"#
        .to_string()
}

fn dispatch_set_retry_count_raw_pg(pool: &PgPool, dispatch_id: &str, count: i32) -> String {
    let dispatch_id = dispatch_id.to_string();
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            sqlx::query("UPDATE task_dispatches SET retry_count = $1 WHERE id = $2")
                .bind(count)
                .bind(&dispatch_id)
                .execute(&bridge_pool)
                .await
                .map(|result| result.rows_affected())
                .map_err(|error| {
                    format!("update postgres dispatch retry_count for {dispatch_id}: {error}")
                })
        },
        |error| format!(r#"{{"error":"{error}"}}"#),
    ) {
        Ok(rows_affected) => format!(r#"{{"ok":true,"rows_affected":{rows_affected}}}"#),
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn dispatch_set_retry_count_raw_sqlite_test(db: &Db, dispatch_id: &str, count: i32) -> String {
    let conn = match db.separate_conn() {
        Ok(conn) => conn,
        Err(error) => {
            return format!(r#"{{"error":"{}"}}"#, error.to_string().replace('"', "'"));
        }
    };
    match conn.execute(
        "UPDATE task_dispatches SET retry_count = ?1 WHERE id = ?2",
        sqlite_test::params![count, dispatch_id],
    ) {
        Ok(rows_affected) => format!(r#"{{"ok":true,"rows_affected":{rows_affected}}}"#),
        Err(error) => format!(r#"{{"error":"{}"}}"#, error.to_string().replace('"', "'")),
    }
}

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
fn dispatch_set_retry_count_raw_sqlite_test(_db: &Db, _dispatch_id: &str, _count: i32) -> String {
    r#"{"error":"sqlite backend is unavailable for dispatch.setRetryCount in production"}"#
        .to_string()
}

// #2045 Finding 3 (P0): JS-bridge mark_failed/mark_completed used to run an ad-hoc
// `UPDATE task_dispatches + dispatch_events + dispatch_outbox` transaction that
// skipped the full cleanup pipeline (semaphore release, auto_queue_entries
// reconcile, phase-gate reconcile, sessions.active_dispatch_id clear,
// observability emit, wait-queue wake). Delegate to the canonical
// `set_dispatch_status_with_backends` which owns all of those side effects in
// the same transactional unit.
fn dispatch_set_status_raw_pg(
    pool: &PgPool,
    dispatch_id: &str,
    to_status: &str,
    result: Option<serde_json::Value>,
    transition_source: &str,
    touch_completed_at: bool,
) -> String {
    let allowed_from: &[&str] = &["pending", "dispatched"];
    match crate::dispatch::set_dispatch_status_with_backends(
        None,
        Some(pool),
        dispatch_id,
        to_status,
        result.as_ref(),
        transition_source,
        Some(allowed_from),
        touch_completed_at,
    ) {
        Ok(rows_affected) => format!(r#"{{"ok":true,"rows_affected":{rows_affected}}}"#),
        Err(error) => crate::engine::ops::ensure_js_error_json(error.to_string()),
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn dispatch_set_status_raw_sqlite_test(
    db: &Db,
    dispatch_id: &str,
    to_status: &str,
    result: Option<serde_json::Value>,
    transition_source: &str,
    touch_completed_at: bool,
) -> String {
    let conn = match db.separate_conn() {
        Ok(conn) => conn,
        Err(error) => {
            return format!(r#"{{"error":"{}"}}"#, error.to_string().replace('"', "'"));
        }
    };
    match crate::dispatch::set_dispatch_status_on_conn(
        &conn,
        dispatch_id,
        to_status,
        result.as_ref(),
        transition_source,
        Some(&["pending", "dispatched"]),
        touch_completed_at,
    ) {
        Ok(rows_affected) => format!(r#"{{"ok":true,"rows_affected":{rows_affected}}}"#),
        Err(error) => format!(r#"{{"error":"{}"}}"#, error.to_string().replace('"', "'")),
    }
}

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
fn dispatch_set_status_raw_sqlite_test(
    _db: &Db,
    _dispatch_id: &str,
    _to_status: &str,
    _result: Option<serde_json::Value>,
    _transition_source: &str,
    _touch_completed_at: bool,
) -> String {
    r#"{"error":"sqlite backend is unavailable for dispatch status updates in production"}"#
        .to_string()
}
