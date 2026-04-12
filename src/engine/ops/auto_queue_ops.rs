use crate::db::Db;
use crate::supervisor::BridgeHandle;
use rquickjs::{Ctx, Function, Object, Result as JsResult};

pub(super) fn register_auto_queue_ops<'js>(
    ctx: &Ctx<'js>,
    db: Db,
    bridge: BridgeHandle,
) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let auto_queue_obj = Object::new(ctx.clone())?;

    let db_update = db.clone();
    auto_queue_obj.set(
        "__updateEntryStatusRaw",
        Function::new(
            ctx.clone(),
            move |entry_id: String, status: String, source: String, opts_json: String| -> String {
                update_entry_status_raw(&db_update, &entry_id, &status, &source, &opts_json)
            },
        )?,
    )?;

    let db_activate = db;
    let bridge_activate = bridge.clone();
    auto_queue_obj.set(
        "__activateRaw",
        Function::new(ctx.clone(), move |body_json: String| -> String {
            activate_raw(&db_activate, &bridge_activate, &body_json)
        })?,
    )?;

    ad.set("autoQueue", auto_queue_obj)?;

    ctx.eval::<(), _>(
        r#"
        (function() {
            var aq = agentdesk.autoQueue;
            aq.activate = function(runIdOrBody, threadGroup) {
                var body;
                if (runIdOrBody && typeof runIdOrBody === "object" && !Array.isArray(runIdOrBody)) {
                    body = Object.assign({}, runIdOrBody);
                } else {
                    body = {
                        run_id: runIdOrBody || null,
                        active_only: true
                    };
                    if (threadGroup !== null && threadGroup !== undefined) {
                        body.thread_group = threadGroup;
                    }
                }
                if (body.active_only === undefined) {
                    body.active_only = true;
                }
                var result = JSON.parse(aq.__activateRaw(JSON.stringify(body)));
                if (result.error) throw new Error(result.error);
                return result;
            };
            aq.updateEntryStatus = function(entryId, status, source, opts) {
                var result = JSON.parse(
                    aq.__updateEntryStatusRaw(
                        entryId,
                        status,
                        source || "",
                        JSON.stringify(opts || {})
                    )
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
        })();
        "#,
    )?;

    Ok(())
}

fn activate_raw(db: &Db, bridge: &BridgeHandle, body_json: &str) -> String {
    let body: crate::server::routes::auto_queue::ActivateBody =
        match serde_json::from_str(body_json) {
            Ok(body) => body,
            Err(error) => {
                return serde_json::json!({
                    "error": format!("invalid activate body JSON: {error}")
                })
                .to_string();
            }
        };

    let engine = match bridge.upgrade_engine() {
        Ok(engine) => engine,
        Err(error) => {
            return serde_json::json!({
                "error": error
            })
            .to_string();
        }
    };

    let deps =
        crate::server::routes::auto_queue::AutoQueueActivateDeps::for_bridge(db.clone(), engine);
    let (_status, response) = crate::server::routes::auto_queue::activate_with_deps(&deps, body);
    response.0.to_string()
}

fn update_entry_status_raw(
    db: &Db,
    entry_id: &str,
    status: &str,
    source: &str,
    opts_json: &str,
) -> String {
    if source.trim().is_empty() {
        return r#"{"error":"source is required"}"#.to_string();
    }

    let opts_value: serde_json::Value = match serde_json::from_str(opts_json) {
        Ok(value) => value,
        Err(error) => {
            return serde_json::json!({
                "error": format!("invalid opts JSON: {error}")
            })
            .to_string();
        }
    };
    let options = crate::db::auto_queue::EntryStatusUpdateOptions {
        dispatch_id: opts_value
            .get("dispatchId")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        slot_index: opts_value.get("slotIndex").and_then(|value| value.as_i64()),
    };

    let conn = match db.separate_conn() {
        Ok(conn) => conn,
        Err(error) => {
            return serde_json::json!({
                "error": format!("DB: {error}")
            })
            .to_string();
        }
    };

    match crate::db::auto_queue::update_entry_status_on_conn(
        &conn, entry_id, status, source, &options,
    ) {
        Ok(result) => serde_json::json!({
            "ok": true,
            "changed": result.changed,
            "from": result.from_status,
            "to": result.to_status,
            "run_id": result.run_id,
        })
        .to_string(),
        Err(error) => serde_json::json!({
            "error": error.to_string()
        })
        .to_string(),
    }
}
