use crate::db::Db;
use crate::supervisor::BridgeHandle;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde::Deserialize;

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

    let db_activate = db.clone();
    let bridge_activate = bridge.clone();
    auto_queue_obj.set(
        "__activateRaw",
        Function::new(ctx.clone(), move |body_json: String| -> String {
            activate_raw(&db_activate, &bridge_activate, &body_json)
        })?,
    )?;
    let db_pause_run = db.clone();
    auto_queue_obj.set(
        "__pauseRunRaw",
        Function::new(
            ctx.clone(),
            move |run_id: String, source: String| -> String {
                pause_run_raw(&db_pause_run, &run_id, &source)
            },
        )?,
    )?;
    let db_resume_run = db.clone();
    auto_queue_obj.set(
        "__resumeRunRaw",
        Function::new(
            ctx.clone(),
            move |run_id: String, source: String| -> String {
                resume_run_raw(&db_resume_run, &run_id, &source)
            },
        )?,
    )?;
    let db_complete_run = db.clone();
    auto_queue_obj.set(
        "__completeRunRaw",
        Function::new(
            ctx.clone(),
            move |run_id: String, source: String, opts_json: String| -> String {
                complete_run_raw(&db_complete_run, &run_id, &source, &opts_json)
            },
        )?,
    )?;
    let db_save_phase_gate = db.clone();
    auto_queue_obj.set(
        "__savePhaseGateStateRaw",
        Function::new(
            ctx.clone(),
            move |run_id: String, phase: i64, state_json: String| -> String {
                save_phase_gate_state_raw(&db_save_phase_gate, &run_id, phase, &state_json)
            },
        )?,
    )?;
    let db_clear_phase_gate = db.clone();
    auto_queue_obj.set(
        "__clearPhaseGateStateRaw",
        Function::new(ctx.clone(), move |run_id: String, phase: i64| -> String {
            clear_phase_gate_state_raw(&db_clear_phase_gate, &run_id, phase)
        })?,
    )?;
    let bridge_should_defer = bridge.clone();
    auto_queue_obj.set(
        "__shouldDeferActivateRaw",
        Function::new(ctx.clone(), move || -> bool {
            should_defer_activate(&bridge_should_defer)
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
                if (aq.__shouldDeferActivateRaw()) {
                    agentdesk.__pendingIntents.push({
                        type: "activate_auto_queue",
                        body: body
                    });
                    return {
                        ok: true,
                        deferred: true,
                        count: 0,
                        dispatched: []
                    };
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
            aq.pauseRun = function(runId, source) {
                var result = JSON.parse(
                    aq.__pauseRunRaw(runId, source || "")
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            aq.resumeRun = function(runId, source) {
                var result = JSON.parse(
                    aq.__resumeRunRaw(runId, source || "")
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            aq.completeRun = function(runId, source, opts) {
                var result = JSON.parse(
                    aq.__completeRunRaw(
                        runId,
                        source || "",
                        JSON.stringify(opts || {})
                    )
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            aq.savePhaseGateState = function(runId, phase, state) {
                var result = JSON.parse(
                    aq.__savePhaseGateStateRaw(
                        runId,
                        phase,
                        JSON.stringify(state || {})
                    )
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            aq.clearPhaseGateState = function(runId, phase) {
                var result = JSON.parse(
                    aq.__clearPhaseGateStateRaw(runId, phase)
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

fn should_defer_activate(bridge: &BridgeHandle) -> bool {
    bridge
        .upgrade_engine()
        .map(|engine| engine.is_actor_thread())
        .unwrap_or(false)
}

fn pause_run_raw(db: &Db, run_id: &str, source: &str) -> String {
    update_run_status_raw(db, source, |conn| {
        crate::db::auto_queue::pause_run_on_conn(conn, run_id)
    })
}

fn resume_run_raw(db: &Db, run_id: &str, source: &str) -> String {
    update_run_status_raw(db, source, |conn| {
        crate::db::auto_queue::resume_run_on_conn(conn, run_id)
    })
}

fn complete_run_raw(db: &Db, run_id: &str, source: &str, opts_json: &str) -> String {
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
    let release_slots = opts_value
        .get("releaseSlots")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    let conn = match db.separate_conn() {
        Ok(conn) => conn,
        Err(error) => {
            return serde_json::json!({
                "error": format!("DB: {error}")
            })
            .to_string();
        }
    };

    if release_slots {
        crate::db::auto_queue::release_run_slots(&conn, run_id);
    }

    match crate::db::auto_queue::complete_run_on_conn(&conn, run_id) {
        Ok(changed) => serde_json::json!({
            "ok": true,
            "changed": changed,
        })
        .to_string(),
        Err(error) => serde_json::json!({
            "error": error.to_string()
        })
        .to_string(),
    }
}

#[derive(Debug, Deserialize)]
struct PhaseGateStatePayload {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    verdict: Option<String>,
    #[serde(default)]
    dispatch_ids: Vec<String>,
    #[serde(default)]
    pass_verdict: Option<String>,
    #[serde(default)]
    next_phase: Option<i64>,
    #[serde(default)]
    final_phase: bool,
    #[serde(default)]
    anchor_card_id: Option<String>,
    #[serde(default)]
    failure_reason: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

fn save_phase_gate_state_raw(db: &Db, run_id: &str, phase: i64, state_json: &str) -> String {
    let payload: PhaseGateStatePayload = match serde_json::from_str(state_json) {
        Ok(value) => value,
        Err(error) => {
            return serde_json::json!({
                "error": format!("invalid phase gate state JSON: {error}")
            })
            .to_string();
        }
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

    let write = crate::db::auto_queue::PhaseGateStateWrite {
        status: payload.status.unwrap_or_else(|| "pending".to_string()),
        verdict: payload.verdict,
        dispatch_ids: payload.dispatch_ids,
        pass_verdict: payload
            .pass_verdict
            .unwrap_or_else(|| "phase_gate_passed".to_string()),
        next_phase: payload.next_phase,
        final_phase: payload.final_phase,
        anchor_card_id: payload.anchor_card_id,
        failure_reason: payload.failure_reason,
        created_at: payload.created_at,
    };

    match crate::db::auto_queue::save_phase_gate_state_on_conn(&conn, run_id, phase, &write) {
        Ok(result) => serde_json::json!({
            "ok": true,
            "dispatch_ids": result.persisted_dispatch_ids,
            "removed_stale_rows": result.removed_stale_rows,
        })
        .to_string(),
        Err(error) => serde_json::json!({
            "error": error.to_string()
        })
        .to_string(),
    }
}

fn clear_phase_gate_state_raw(db: &Db, run_id: &str, phase: i64) -> String {
    let conn = match db.separate_conn() {
        Ok(conn) => conn,
        Err(error) => {
            return serde_json::json!({
                "error": format!("DB: {error}")
            })
            .to_string();
        }
    };

    match crate::db::auto_queue::clear_phase_gate_state_on_conn(&conn, run_id, phase) {
        Ok(changed) => serde_json::json!({
            "ok": true,
            "changed": changed,
        })
        .to_string(),
        Err(error) => serde_json::json!({
            "error": error.to_string()
        })
        .to_string(),
    }
}

fn update_run_status_raw<F>(db: &Db, source: &str, update: F) -> String
where
    F: FnOnce(&rusqlite::Connection) -> rusqlite::Result<bool>,
{
    if source.trim().is_empty() {
        return r#"{"error":"source is required"}"#.to_string();
    }

    let conn = match db.separate_conn() {
        Ok(conn) => conn,
        Err(error) => {
            return serde_json::json!({
                "error": format!("DB: {error}")
            })
            .to_string();
        }
    };

    match update(&conn) {
        Ok(changed) => serde_json::json!({
            "ok": true,
            "changed": changed,
        })
        .to_string(),
        Err(error) => serde_json::json!({
            "error": error.to_string()
        })
        .to_string(),
    }
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
