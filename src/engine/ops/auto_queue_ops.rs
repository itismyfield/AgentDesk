use crate::supervisor::BridgeHandle;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde::Deserialize;
use sqlx::PgPool;

pub(super) fn register_auto_queue_ops<'js>(
    ctx: &Ctx<'js>,
    db: Option<crate::db::Db>,
    pg_pool: Option<PgPool>,
    bridge: BridgeHandle,
) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let auto_queue_obj = Object::new(ctx.clone())?;

    let pg_update = pg_pool.clone();
    auto_queue_obj.set(
        "__updateEntryStatusRaw",
        Function::new(
            ctx.clone(),
            move |entry_id: String, status: String, source: String, opts_json: String| -> String {
                update_entry_status_raw(pg_update.as_ref(), &entry_id, &status, &source, &opts_json)
            },
        )?,
    )?;

    let pg_activate = pg_pool.clone();
    let bridge_activate = bridge.clone();
    auto_queue_obj.set(
        "__activateRaw",
        Function::new(ctx.clone(), move |body_json: String| -> String {
            activate_raw(pg_activate.as_ref(), &bridge_activate, &body_json)
        })?,
    )?;
    let db_pause_run = db.clone();
    let pg_pause_run = pg_pool.clone();
    auto_queue_obj.set(
        "__pauseRunRaw",
        Function::new(
            ctx.clone(),
            move |run_id: String, source: String| -> String {
                pause_run_raw(
                    db_pause_run.as_ref(),
                    pg_pause_run.as_ref(),
                    &run_id,
                    &source,
                )
            },
        )?,
    )?;
    let db_resume_run = db.clone();
    let pg_resume_run = pg_pool.clone();
    auto_queue_obj.set(
        "__resumeRunRaw",
        Function::new(
            ctx.clone(),
            move |run_id: String, source: String| -> String {
                resume_run_raw(
                    db_resume_run.as_ref(),
                    pg_resume_run.as_ref(),
                    &run_id,
                    &source,
                )
            },
        )?,
    )?;
    let db_complete_run = db.clone();
    let pg_complete_run = pg_pool.clone();
    auto_queue_obj.set(
        "__completeRunRaw",
        Function::new(
            ctx.clone(),
            move |run_id: String, source: String, opts_json: String| -> String {
                complete_run_raw(
                    db_complete_run.as_ref(),
                    pg_complete_run.as_ref(),
                    &run_id,
                    &source,
                    &opts_json,
                )
            },
        )?,
    )?;
    let db_save_phase_gate = db.clone();
    let pg_save_phase_gate = pg_pool.clone();
    auto_queue_obj.set(
        "__savePhaseGateStateRaw",
        Function::new(
            ctx.clone(),
            move |run_id: String, phase: i64, state_json: String| -> String {
                save_phase_gate_state_raw(
                    db_save_phase_gate.as_ref(),
                    pg_save_phase_gate.as_ref(),
                    &run_id,
                    phase,
                    &state_json,
                )
            },
        )?,
    )?;
    let db_clear_phase_gate = db;
    let pg_clear_phase_gate = pg_pool.clone();
    auto_queue_obj.set(
        "__clearPhaseGateStateRaw",
        Function::new(ctx.clone(), move |run_id: String, phase: i64| -> String {
            clear_phase_gate_state_raw(
                db_clear_phase_gate.as_ref(),
                pg_clear_phase_gate.as_ref(),
                &run_id,
                phase,
            )
        })?,
    )?;
    let pg_record_consultation = pg_pool.clone();
    auto_queue_obj.set(
        "__recordConsultationDispatchRaw",
        Function::new(
            ctx.clone(),
            move |entry_id: String,
                  card_id: String,
                  dispatch_id: String,
                  source: String,
                  metadata_json: String|
                  -> String {
                record_consultation_dispatch_raw(
                    pg_record_consultation.as_ref(),
                    &entry_id,
                    &card_id,
                    &dispatch_id,
                    &source,
                    &metadata_json,
                )
            },
        )?,
    )?;
    let pg_record_dispatch_failure = pg_pool.clone();
    auto_queue_obj.set(
        "__recordEntryDispatchFailureRaw",
        Function::new(
            ctx.clone(),
            move |entry_id: String, max_retries: i64, source: String| -> String {
                record_entry_dispatch_failure_raw(
                    pg_record_dispatch_failure.as_ref(),
                    &entry_id,
                    max_retries,
                    &source,
                )
            },
        )?,
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
            agentdesk.autoQueue.activate = function(runIdOrBody, threadGroup) {
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
                if (agentdesk.autoQueue.__shouldDeferActivateRaw()) {
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
                var result = JSON.parse(agentdesk.autoQueue.__activateRaw(JSON.stringify(body)));
                if (result.error) throw new Error(result.error);
                return result;
            };
            agentdesk.autoQueue.updateEntryStatus = function(entryId, status, source, opts) {
                var result = JSON.parse(
                    agentdesk.autoQueue.__updateEntryStatusRaw(
                        entryId,
                        status,
                        source || "",
                        JSON.stringify(opts || {})
                    )
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            agentdesk.autoQueue.pauseRun = function(runId, source) {
                var result = JSON.parse(
                    agentdesk.autoQueue.__pauseRunRaw(runId, source || "")
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            agentdesk.autoQueue.resumeRun = function(runId, source) {
                var result = JSON.parse(
                    agentdesk.autoQueue.__resumeRunRaw(runId, source || "")
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            agentdesk.autoQueue.completeRun = function(runId, source, opts) {
                var result = JSON.parse(
                    agentdesk.autoQueue.__completeRunRaw(
                        runId,
                        source || "",
                        JSON.stringify(opts || {})
                    )
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            agentdesk.autoQueue.savePhaseGateState = function(runId, phase, state) {
                var result = JSON.parse(
                    agentdesk.autoQueue.__savePhaseGateStateRaw(
                        runId,
                        phase,
                        JSON.stringify(state || {})
                    )
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            agentdesk.autoQueue.clearPhaseGateState = function(runId, phase) {
                var result = JSON.parse(
                    agentdesk.autoQueue.__clearPhaseGateStateRaw(runId, phase)
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            agentdesk.autoQueue.recordConsultationDispatch = function(entryId, cardId, dispatchId, source, metadata) {
                var result = JSON.parse(
                    agentdesk.autoQueue.__recordConsultationDispatchRaw(
                        entryId,
                        cardId,
                        dispatchId,
                        source || "",
                        JSON.stringify(metadata || {})
                    )
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            agentdesk.autoQueue.recordDispatchFailure = function(entryId, maxRetries, source) {
                var result = JSON.parse(
                    agentdesk.autoQueue.__recordEntryDispatchFailureRaw(
                        entryId,
                        maxRetries,
                        source || ""
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

fn activate_raw(pg_pool: Option<&PgPool>, bridge: &BridgeHandle, body_json: &str) -> String {
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

    let Some(pool) = pg_pool.or_else(|| engine.pg_pool()) else {
        return serde_json::json!({
            "error": "postgres backend is required for autoQueue.activate"
        })
        .to_string();
    };
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        {
            let body = body;
            let engine = engine.clone();
            move |_bridge_pool| async move {
                let (_status, response) =
                    crate::server::routes::auto_queue::activate_with_bridge_pg(None, engine, body)
                        .await;
                Ok(response.0.to_string())
            }
        },
        |error| serde_json::json!({ "error": error }).to_string(),
    ) {
        Ok(json) => json,
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}

fn should_defer_activate(bridge: &BridgeHandle) -> bool {
    bridge
        .upgrade_engine()
        .map(|engine| engine.is_actor_thread())
        .unwrap_or(false)
}

fn pause_run_raw(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    run_id: &str,
    source: &str,
) -> String {
    if source.trim().is_empty() {
        return r#"{"error":"source is required"}"#.to_string();
    }

    let Some(pool) = pg_pool else {
        #[cfg(test)]
        if let Some(db) = db {
            return update_run_status_raw_sqlite(db, run_id, "paused");
        }
        let _ = db;
        return r#"{"error":"postgres backend is required for autoQueue.pauseRun"}"#.to_string();
    };
    let run_id_owned = run_id.to_string();
    let result = run_async_bridge_pg(pool, move |pool| async move {
        crate::db::auto_queue::pause_run_on_pg(&pool, &run_id_owned).await
    });

    match result {
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

fn resume_run_raw(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    run_id: &str,
    source: &str,
) -> String {
    if source.trim().is_empty() {
        return r#"{"error":"source is required"}"#.to_string();
    }

    let Some(pool) = pg_pool else {
        #[cfg(test)]
        if let Some(db) = db {
            return update_run_status_raw_sqlite(db, run_id, "active");
        }
        let _ = db;
        return r#"{"error":"postgres backend is required for autoQueue.resumeRun"}"#.to_string();
    };
    let run_id_owned = run_id.to_string();
    let result = run_async_bridge_pg(pool, move |pool| async move {
        crate::db::auto_queue::resume_run_on_pg(&pool, &run_id_owned).await
    });

    match result {
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

fn complete_run_raw(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    run_id: &str,
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
    let release_slots = opts_value
        .get("releaseSlots")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    let Some(pool) = pg_pool else {
        #[cfg(test)]
        if let Some(db) = db {
            return update_run_status_raw_sqlite(db, run_id, "completed");
        }
        let _ = db;
        return r#"{"error":"postgres backend is required for autoQueue.completeRun"}"#.to_string();
    };
    let run_id_owned = run_id.to_string();
    let result = run_async_bridge_pg(pool, move |pool| async move {
        if release_slots {
            crate::db::auto_queue::release_run_slots_pg(&pool, &run_id_owned)
                .await
                .map_err(|error| format!("release postgres auto-queue slots: {error}"))?;
        }
        crate::db::auto_queue::complete_run_on_pg(&pool, &run_id_owned).await
    });

    match result {
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

#[cfg(test)]
fn update_run_status_raw_sqlite(db: &crate::db::Db, run_id: &str, status: &str) -> String {
    let result = db
        .separate_conn()
        .map_err(|error| format!("open sqlite auto-queue run connection: {error}"))
        .and_then(|conn| {
            conn.execute(
                "UPDATE auto_queue_runs SET status = ?1 WHERE id = ?2",
                rusqlite::params![status, run_id],
            )
            .map_err(|error| format!("update sqlite auto-queue run {run_id}: {error}"))
        });
    match result {
        Ok(changed) => serde_json::json!({ "ok": true, "changed": changed }).to_string(),
        Err(error) => serde_json::json!({ "error": error }).to_string(),
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

fn save_phase_gate_state_raw(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    run_id: &str,
    phase: i64,
    state_json: &str,
) -> String {
    let payload: PhaseGateStatePayload = match serde_json::from_str(state_json) {
        Ok(value) => value,
        Err(error) => {
            return serde_json::json!({
                "error": format!("invalid phase gate state JSON: {error}")
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

    let Some(pool) = pg_pool else {
        #[cfg(test)]
        if let Some(db) = db {
            return save_phase_gate_state_raw_sqlite(db, run_id, phase, &write);
        }
        let _ = db;
        return r#"{"error":"postgres backend is required for autoQueue.savePhaseGateState"}"#
            .to_string();
    };
    let run_id_owned = run_id.to_string();
    let result = run_async_bridge_pg(pool, move |pool| async move {
        crate::db::auto_queue::save_phase_gate_state_on_pg(&pool, &run_id_owned, phase, &write)
            .await
    });

    match result {
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

fn clear_phase_gate_state_raw(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    run_id: &str,
    phase: i64,
) -> String {
    let Some(pool) = pg_pool else {
        #[cfg(test)]
        if let Some(db) = db {
            return clear_phase_gate_state_raw_sqlite(db, run_id, phase);
        }
        let _ = db;
        return r#"{"error":"postgres backend is required for autoQueue.clearPhaseGateState"}"#
            .to_string();
    };
    let run_id_owned = run_id.to_string();
    let result = run_async_bridge_pg(pool, move |pool| async move {
        crate::db::auto_queue::clear_phase_gate_state_on_pg(&pool, &run_id_owned, phase).await
    });

    match result {
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

#[cfg(test)]
fn save_phase_gate_state_raw_sqlite(
    db: &crate::db::Db,
    run_id: &str,
    phase: i64,
    state: &crate::db::auto_queue::PhaseGateStateWrite,
) -> String {
    let result = (|| {
        let conn = db
            .separate_conn()
            .map_err(|error| format!("open sqlite phase gate connection: {error}"))?;
        let dispatch_ids = state
            .dispatch_ids
            .iter()
            .filter_map(|dispatch_id| {
                conn.query_row(
                    "SELECT id FROM task_dispatches WHERE id = ?1",
                    [dispatch_id],
                    |row| row.get::<_, String>(0),
                )
                .ok()
            })
            .collect::<Vec<_>>();
        let removed_stale_rows = conn
            .execute(
                "DELETE FROM auto_queue_phase_gates WHERE run_id = ?1 AND phase = ?2",
                rusqlite::params![run_id, phase],
            )
            .map_err(|error| format!("delete sqlite phase gate rows for {run_id}: {error}"))?;
        let final_phase = if state.final_phase { 1 } else { 0 };
        if dispatch_ids.is_empty() {
            conn.execute(
                "INSERT INTO auto_queue_phase_gates (
                    run_id, phase, status, verdict, dispatch_id, pass_verdict,
                    next_phase, final_phase, anchor_card_id, failure_reason, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, ?8, ?9, COALESCE(?10, datetime('now')), datetime('now'))",
                rusqlite::params![
                    run_id,
                    phase,
                    state.status,
                    state.verdict,
                    state.pass_verdict,
                    state.next_phase,
                    final_phase,
                    state.anchor_card_id,
                    state.failure_reason,
                    state.created_at,
                ],
            )
            .map_err(|error| format!("insert sqlite phase gate row for {run_id}: {error}"))?;
        } else {
            for dispatch_id in &dispatch_ids {
                conn.execute(
                    "INSERT INTO auto_queue_phase_gates (
                        run_id, phase, status, verdict, dispatch_id, pass_verdict,
                        next_phase, final_phase, anchor_card_id, failure_reason, created_at, updated_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, COALESCE(?11, datetime('now')), datetime('now'))",
                    rusqlite::params![
                        run_id,
                        phase,
                        state.status,
                        state.verdict,
                        dispatch_id,
                        state.pass_verdict,
                        state.next_phase,
                        final_phase,
                        state.anchor_card_id,
                        state.failure_reason,
                        state.created_at,
                    ],
                )
                .map_err(|error| {
                    format!("insert sqlite phase gate row for dispatch {dispatch_id}: {error}")
                })?;
            }
        }
        Ok::<_, String>(
            serde_json::json!({
                "ok": true,
                "dispatch_ids": dispatch_ids,
                "removed_stale_rows": removed_stale_rows,
            })
            .to_string(),
        )
    })();
    result.unwrap_or_else(|error| serde_json::json!({ "error": error }).to_string())
}

#[cfg(test)]
fn clear_phase_gate_state_raw_sqlite(db: &crate::db::Db, run_id: &str, phase: i64) -> String {
    let result = db
        .separate_conn()
        .map_err(|error| format!("open sqlite clear phase gate connection: {error}"))
        .and_then(|conn| {
            conn.execute(
                "DELETE FROM auto_queue_phase_gates WHERE run_id = ?1 AND phase = ?2",
                rusqlite::params![run_id, phase],
            )
            .map_err(|error| format!("clear sqlite phase gate rows for {run_id}: {error}"))
        });
    match result {
        Ok(changed) => serde_json::json!({ "ok": true, "changed": changed }).to_string(),
        Err(error) => serde_json::json!({ "error": error }).to_string(),
    }
}

fn record_consultation_dispatch_raw(
    pg_pool: Option<&PgPool>,
    entry_id: &str,
    card_id: &str,
    dispatch_id: &str,
    source: &str,
    metadata_json: &str,
) -> String {
    let Some(pool) = pg_pool else {
        return r#"{"error":"postgres backend is required for autoQueue.recordConsultationDispatch"}"#
            .to_string();
    };
    let entry_id_owned = entry_id.to_string();
    let card_id_owned = card_id.to_string();
    let dispatch_id_owned = dispatch_id.to_string();
    let source_owned = source.to_string();
    let metadata_json_owned = metadata_json.to_string();
    let result = run_async_bridge_pg(pool, move |pool| async move {
        crate::db::auto_queue::record_consultation_dispatch_on_pg(
            &pool,
            &entry_id_owned,
            &card_id_owned,
            &dispatch_id_owned,
            &source_owned,
            &metadata_json_owned,
        )
        .await
    });

    match result {
        Ok(result) => serde_json::json!({
            "ok": true,
            "changed": result.entry_status_changed,
            "metadata": serde_json::from_str::<serde_json::Value>(&result.metadata_json)
                .unwrap_or_else(|_| serde_json::json!({})),
        })
        .to_string(),
        Err(error) => serde_json::json!({
            "error": error.to_string()
        })
        .to_string(),
    }
}

fn record_entry_dispatch_failure_raw(
    pg_pool: Option<&PgPool>,
    entry_id: &str,
    max_retries: i64,
    source: &str,
) -> String {
    if source.trim().is_empty() {
        return r#"{"error":"source is required"}"#.to_string();
    }

    let Some(pool) = pg_pool else {
        return r#"{"error":"postgres backend is required for autoQueue.recordDispatchFailure"}"#
            .to_string();
    };
    let entry_id_owned = entry_id.to_string();
    let source_owned = source.to_string();
    let result = run_async_bridge_pg(pool, move |pool| async move {
        crate::db::auto_queue::record_entry_dispatch_failure_on_pg(
            &pool,
            &entry_id_owned,
            max_retries,
            &source_owned,
        )
        .await
    });

    match result {
        Ok(result) => serde_json::json!({
            "ok": true,
            "changed": result.changed,
            "from": result.from_status,
            "to": result.to_status,
            "run_id": result.run_id,
            "retryCount": result.retry_count,
            "retryLimit": result.retry_limit,
        })
        .to_string(),
        Err(error) => serde_json::json!({
            "error": error.to_string()
        })
        .to_string(),
    }
}

fn update_entry_status_raw(
    pg_pool: Option<&PgPool>,
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

    let Some(pool) = pg_pool else {
        return r#"{"error":"postgres backend is required for autoQueue.updateEntryStatus"}"#
            .to_string();
    };
    let entry_id_owned = entry_id.to_string();
    let status_owned = status.to_string();
    let source_owned = source.to_string();
    let result = run_async_bridge_pg(pool, move |pool| async move {
        crate::db::auto_queue::update_entry_status_on_pg(
            &pool,
            &entry_id_owned,
            &status_owned,
            &source_owned,
            &options,
        )
        .await
    });

    match result {
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

fn run_async_bridge_pg<F, T>(
    pool: &PgPool,
    future_factory: impl FnOnce(PgPool) -> F + Send + 'static,
) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>> + Send + 'static,
    T: Send + 'static,
{
    crate::utils::async_bridge::block_on_pg_result(pool, future_factory, |error| error)
}
