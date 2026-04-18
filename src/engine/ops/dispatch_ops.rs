use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};

// ── Dispatch ops ────────────────────────────────────────────────
//
// agentdesk.dispatch.create(cardId, agentId, dispatchType, title, context?) → dispatchId
// Creates a task_dispatch row + updates kanban card to "requested".
// Discord notification is handled by posting to the local /api/send endpoint.

pub(super) fn register_dispatch_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let dispatch_obj = Object::new(ctx.clone())?;

    // #248: __dispatch_create_sync(card_id, agent_id, dispatch_type, title, context_json) → json_string
    // Synchronous DB INSERT — no deferred intent.
    let db_d = db.clone();
    dispatch_obj.set(
        "__create_sync",
        Function::new(
            ctx.clone(),
            rquickjs::function::MutFn::from(
                move |card_id: String,
                      agent_id: String,
                      dispatch_type: String,
                      title: String,
                      context_json: String|
                      -> String {
                    dispatch_create_sync(
                        &db_d,
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
    dispatch_obj.set(
        "__mark_failed_raw",
        Function::new(
            ctx.clone(),
            move |dispatch_id: String, reason: String| -> String {
                let conn = match db_mf.separate_conn() {
                    Ok(c) => c,
                    Err(e) => return format!(r#"{{"error":"DB: {}"}}"#, e),
                };
                let reason_json = serde_json::json!({ "reason": reason });
                match crate::dispatch::set_dispatch_status_on_conn(
                    &conn,
                    &dispatch_id,
                    "failed",
                    Some(&reason_json),
                    "js_dispatch_mark_failed_raw",
                    Some(&["pending", "dispatched"]),
                    false,
                ) {
                    Ok(n) => format!(r#"{{"ok":true,"rows_affected":{n}}}"#),
                    Err(e) => format!(r#"{{"error":"sql: {}"}}"#, e),
                }
            },
        )?,
    )?;

    // __mark_completed_raw(dispatch_id, result_json) → json_string
    // Marks a dispatch as completed. Used by orphan recovery.
    let db_mc = db.clone();
    dispatch_obj.set(
        "__mark_completed_raw",
        Function::new(
            ctx.clone(),
            move |dispatch_id: String, result_json: String| -> String {
                let conn = match db_mc.separate_conn() {
                    Ok(c) => c,
                    Err(e) => return format!(r#"{{"error":"DB: {}"}}"#, e),
                };
                let parsed_result = serde_json::from_str::<serde_json::Value>(&result_json)
                    .unwrap_or_else(|_| serde_json::json!({ "raw_result": result_json }));
                match crate::dispatch::set_dispatch_status_on_conn(
                    &conn,
                    &dispatch_id,
                    "completed",
                    Some(&parsed_result),
                    "js_dispatch_mark_completed_raw",
                    Some(&["pending", "dispatched"]),
                    true,
                ) {
                    Ok(n) => format!(r#"{{"ok":true,"rows_affected":{n}}}"#),
                    Err(e) => format!(r#"{{"error":"sql: {}"}}"#, e),
                }
            },
        )?,
    )?;

    // __has_active_work_raw(card_id) → json_string {"count":N}
    // Checks if a card has active implementation/rework dispatches.
    let db_aw = db.clone();
    dispatch_obj.set(
        "__has_active_work_raw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            let conn = match db_aw.separate_conn() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"DB: {}"}}"#, e),
            };
            match conn.query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                     WHERE kanban_card_id = ?1 AND dispatch_type IN ('implementation', 'rework') \
                     AND status IN ('pending', 'dispatched')",
                libsql_rusqlite::params![card_id],
                |row| row.get::<_, i64>(0),
            ) {
                Ok(cnt) => format!(r#"{{"count":{cnt}}}"#),
                Err(e) => format!(r#"{{"error":"sql: {}"}}"#, e),
            }
        })?,
    )?;

    // __set_retry_count_raw(dispatch_id, count) → json_string
    // Updates retry_count for auto-retry tracking.
    let db_rc = db;
    dispatch_obj.set(
        "__set_retry_count_raw",
        Function::new(
            ctx.clone(),
            move |dispatch_id: String, count: i32| -> String {
                let conn = match db_rc.separate_conn() {
                    Ok(c) => c,
                    Err(e) => return format!(r#"{{"error":"DB: {}"}}"#, e),
                };
                match conn.execute(
                    "UPDATE task_dispatches SET retry_count = ?1 WHERE id = ?2",
                    libsql_rusqlite::params![count, dispatch_id],
                ) {
                    Ok(n) => format!(r#"{{"ok":true,"rows_affected":{n}}}"#),
                    Err(e) => format!(r#"{{"error":"sql: {}"}}"#, e),
                }
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
                    agentdesk.dispatch.__create_sync(cardId, agentId, dt, t, ctxJson)
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
/// `create_dispatch_core`, so no JS-side outbox buffering is needed.
fn dispatch_create_sync(
    db: &Db,
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
    match crate::dispatch::create_dispatch_core_with_options(
        db,
        card_id,
        agent_id,
        dispatch_type,
        title,
        &context,
        options,
    ) {
        Ok((dispatch_id, _old_status, reused)) => {
            if reused {
                return format!(r#"{{"dispatch_id":"{dispatch_id}","reused":true}}"#);
            }
            // #117/#158: Update card_review_state for review-decision dispatches
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
            format!(r#"{{"dispatch_id":"{dispatch_id}"}}"#)
        }
        Err(e) => {
            format!(r#"{{"error":"{}"}}"#, e.to_string().replace('"', "'"))
        }
    }
}
