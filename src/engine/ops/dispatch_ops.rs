use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};

// ── Dispatch ops ────────────────────────────────────────────────
//
// agentdesk.dispatch.create(cardId, agentId, dispatchType, title) → dispatchId
// Creates a task_dispatch row + updates kanban card to "requested".
// Discord notification is handled by posting to the local /api/send endpoint.

pub(super) fn register_dispatch_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let dispatch_obj = Object::new(ctx.clone())?;

    // #248: __dispatch_create_sync(card_id, agent_id, dispatch_type, title) → json_string
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
                      title: String|
                      -> String {
                    dispatch_create_sync(&db_d, &card_id, &agent_id, &dispatch_type, &title)
                },
            ),
        )?,
    )?;

    // __mark_failed_raw(dispatch_id, reason) → json_string
    // Marks a dispatch as failed. Used by timeout handlers.
    let db_mf = db.clone();
    dispatch_obj.set(
        "__mark_failed_raw",
        Function::new(ctx.clone(), move |dispatch_id: String, reason: String| -> String {
            let conn = match db_mf.separate_conn() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"DB: {}"}}"#, e),
            };
            match conn.execute(
                "UPDATE task_dispatches SET status = 'failed', result = ?1, updated_at = datetime('now') \
                 WHERE id = ?2 AND status IN ('pending', 'dispatched')",
                rusqlite::params![reason, dispatch_id],
            ) {
                Ok(n) => format!(r#"{{"ok":true,"rows_affected":{n}}}"#),
                Err(e) => format!(r#"{{"error":"sql: {}"}}"#, e),
            }
        })?,
    )?;

    // __mark_completed_raw(dispatch_id, result_json) → json_string
    // Marks a dispatch as completed. Used by orphan recovery.
    let db_mc = db.clone();
    dispatch_obj.set(
        "__mark_completed_raw",
        Function::new(ctx.clone(), move |dispatch_id: String, result_json: String| -> String {
            let conn = match db_mc.separate_conn() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"DB: {}"}}"#, e),
            };
            match conn.execute(
                "UPDATE task_dispatches SET status = 'completed', result = ?1, updated_at = datetime('now') \
                 WHERE id = ?2 AND status IN ('pending', 'dispatched')",
                rusqlite::params![result_json, dispatch_id],
            ) {
                Ok(n) => format!(r#"{{"ok":true,"rows_affected":{n}}}"#),
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
                    rusqlite::params![count, dispatch_id],
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
            var sync = agentdesk.dispatch.__create_sync;
            agentdesk.dispatch.create = function(cardId, agentId, dispatchType, title) {
                var dt = dispatchType || "implementation";
                var t = title || "Dispatch";
                // #248: Synchronous DB INSERT — no deferred intent.
                // Validation + INSERT happen atomically in Rust.
                var result = JSON.parse(sync(cardId, agentId, dt, t));
                if (result.error) throw new Error(result.error);
                var dispatchId = result.dispatch_id;
                return dispatchId;
            };
            var rawFail = agentdesk.dispatch.__mark_failed_raw;
            agentdesk.dispatch.markFailed = function(dispatchId, reason) {
                var result = JSON.parse(rawFail(dispatchId, reason || ""));
                if (result.error) throw new Error(result.error);
                if (result.rows_affected === 0) {
                    agentdesk.log.warn("[dispatch.markFailed] no rows affected for " + dispatchId + " — already terminal or missing");
                }
                return result;
            };
            var rawComplete = agentdesk.dispatch.__mark_completed_raw;
            agentdesk.dispatch.markCompleted = function(dispatchId, resultJson) {
                var result = JSON.parse(rawComplete(dispatchId, resultJson || "{}"));
                if (result.error) throw new Error(result.error);
                if (result.rows_affected === 0) {
                    agentdesk.log.warn("[dispatch.markCompleted] no rows affected for " + dispatchId + " — already terminal or missing");
                }
                return result;
            };
            var rawRetry = agentdesk.dispatch.__set_retry_count_raw;
            agentdesk.dispatch.setRetryCount = function(dispatchId, count) {
                var result = JSON.parse(rawRetry(dispatchId, count));
                if (result.error) throw new Error(result.error);
                if (result.rows_affected === 0) {
                    agentdesk.log.warn("[dispatch.setRetryCount] no rows affected for " + dispatchId + " — missing");
                }
                return result;
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
) -> String {
    let context = serde_json::json!({});
    match crate::dispatch::create_dispatch_core(
        db,
        card_id,
        agent_id,
        dispatch_type,
        title,
        &context,
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
