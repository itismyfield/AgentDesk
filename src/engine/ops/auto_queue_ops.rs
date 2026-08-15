use crate::supervisor::BridgeHandle;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde::Deserialize;
use sqlx::PgPool;

pub(super) fn register_auto_queue_ops<'js>(
    ctx: &Ctx<'js>,
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
    let pg_pause_run = pg_pool.clone();
    auto_queue_obj.set(
        "__pauseRunRaw",
        Function::new(
            ctx.clone(),
            move |run_id: String, source: String| -> String {
                pause_run_raw(pg_pause_run.as_ref(), &run_id, &source)
            },
        )?,
    )?;
    let pg_resume_run = pg_pool.clone();
    auto_queue_obj.set(
        "__resumeRunRaw",
        Function::new(
            ctx.clone(),
            move |run_id: String, source: String| -> String {
                resume_run_raw(pg_resume_run.as_ref(), &run_id, &source)
            },
        )?,
    )?;
    let pg_complete_run = pg_pool.clone();
    auto_queue_obj.set(
        "__completeRunRaw",
        Function::new(
            ctx.clone(),
            move |run_id: String, source: String, opts_json: String| -> String {
                complete_run_raw(pg_complete_run.as_ref(), &run_id, &source, &opts_json)
            },
        )?,
    )?;
    let pg_save_phase_gate = pg_pool.clone();
    auto_queue_obj.set(
        "__savePhaseGateStateRaw",
        Function::new(
            ctx.clone(),
            move |run_id: String, phase: i64, state_json: String| -> String {
                save_phase_gate_state_raw(pg_save_phase_gate.as_ref(), &run_id, phase, &state_json)
            },
        )?,
    )?;
    let pg_clear_phase_gate = pg_pool.clone();
    auto_queue_obj.set(
        "__clearPhaseGateStateRaw",
        Function::new(ctx.clone(), move |run_id: String, phase: i64| -> String {
            clear_phase_gate_state_raw(pg_clear_phase_gate.as_ref(), &run_id, phase)
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
                match crate::server::routes::auto_queue::activate_with_bridge_pg(engine, body).await
                {
                    Ok((_status, response)) => Ok(response.0.to_string()),
                    Err(error) => Ok(error.to_json_value().to_string()),
                }
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

fn pause_run_raw(pg_pool: Option<&PgPool>, run_id: &str, source: &str) -> String {
    if source.trim().is_empty() {
        return r#"{"error":"source is required"}"#.to_string();
    }

    let Some(pool) = pg_pool else {
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

fn resume_run_raw(pg_pool: Option<&PgPool>, run_id: &str, source: &str) -> String {
    if source.trim().is_empty() {
        return r#"{"error":"source is required"}"#.to_string();
    }

    let Some(pool) = pg_pool else {
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
    pg_pool: Option<&PgPool>,
    run_id: &str,
    source: &str,
    opts_json: &str,
) -> String {
    if source.trim().is_empty() {
        return r#"{"error":"source is required"}"#.to_string();
    }

    // Options remain parse-validated for host API compatibility, but no option
    // controls slot release. Canonical completion always releases slots inside
    // the same transaction as the run status change.
    let _opts_value: serde_json::Value = match serde_json::from_str(opts_json) {
        Ok(value) => value,
        Err(error) => {
            return serde_json::json!({
                "error": format!("invalid opts JSON: {error}")
            })
            .to_string();
        }
    };
    let Some(pool) = pg_pool else {
        return r#"{"error":"postgres backend is required for autoQueue.completeRun"}"#.to_string();
    };
    let run_id_owned = run_id.to_string();
    let result = run_async_bridge_pg(pool, move |pool| async move {
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

fn clear_phase_gate_state_raw(pg_pool: Option<&PgPool>, run_id: &str, phase: i64) -> String {
    let Some(pool) = pg_pool else {
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

#[cfg(test)]
mod tests {
    use super::complete_run_raw;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use sqlx::{PgPool, Row};

    async fn setup_complete_wrapper_pool(pg_db: &TestPostgresDb) -> PgPool {
        let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('wrapper-agent', 'Wrapper Agent', 'claude', 'wrapper-channel')",
        )
        .execute(&pool)
        .await
        .expect("seed wrapper agent");
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES
                ('wrapper-refused', 'repo-wrapper', 'wrapper-agent', 'active'),
                ('wrapper-success', 'repo-wrapper', 'wrapper-agent', 'active')",
        )
        .execute(&pool)
        .await
        .expect("seed wrapper runs");
        sqlx::query(
            "INSERT INTO auto_queue_slots
                (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
             VALUES
                ('wrapper-agent', 0, 'wrapper-refused', 0, CAST('{}' AS jsonb)),
                ('wrapper-agent', 1, 'wrapper-success', 0, CAST('{}' AS jsonb))",
        )
        .execute(&pool)
        .await
        .expect("seed wrapper slots");
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES
                ('wrapper-card-refused', 'Wrapper Refused', 'in_progress', 'wrapper-agent'),
                ('wrapper-card-success', 'Wrapper Success', 'in_progress', 'wrapper-agent')",
        )
        .execute(&pool)
        .await
        .expect("seed wrapper cards");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status)
             VALUES
                ('wrapper-entry-refused', 'wrapper-refused', 'wrapper-card-refused',
                 'wrapper-agent', 'dispatched'),
                ('wrapper-entry-success', 'wrapper-success', 'wrapper-card-success',
                 'wrapper-agent', 'failed')",
        )
        .execute(&pool)
        .await
        .expect("seed wrapper entries");
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates (run_id, phase, status)
             VALUES
                ('wrapper-refused', 0, 'pending'),
                ('wrapper-success', 0, 'pending')",
        )
        .execute(&pool)
        .await
        .expect("seed wrapper phase gates");
        pool
    }

    async fn wrapper_run_state(
        pool: &PgPool,
        run_id: &str,
        slot_index: i64,
    ) -> (String, Option<String>, i64) {
        let row = sqlx::query(
            "SELECT r.status,
                    (SELECT assigned_run_id
                     FROM auto_queue_slots
                     WHERE agent_id = r.agent_id AND slot_index = $2) AS assigned_run_id,
                    (SELECT COUNT(*)::BIGINT
                     FROM auto_queue_phase_gates pg
                     WHERE pg.run_id = r.id) AS gate_count
             FROM auto_queue_runs r
             WHERE r.id = $1",
        )
        .bind(run_id)
        .bind(slot_index)
        .fetch_one(pool)
        .await
        .expect("load wrapper run state");
        (
            row.try_get("status").expect("wrapper run status"),
            row.try_get("assigned_run_id")
                .expect("wrapper assigned run"),
            row.try_get("gate_count").expect("wrapper gate count"),
        )
    }

    /// This exercises the synchronous engine wrapper used by the JS policy.
    /// The lane cannot observe an uncommitted intermediate state, so it pins
    /// both postconditions: refusal preserves slot/gate state, while success
    /// commits completion and slot release together.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn complete_run_wrapper_preserves_refused_slot_and_atomically_releases_success_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_complete_wrapper_pool(&pg_db).await;

        let refused = serde_json::from_str::<serde_json::Value>(&complete_run_raw(
            Some(&pool),
            "wrapper-refused",
            "test_wrapper_refused",
            r#"{"releaseSlots":true}"#,
        ))
        .expect("decode refused wrapper response");
        assert_eq!(
            refused.get("ok").and_then(|value| value.as_bool()),
            Some(true)
        );
        assert_eq!(
            refused.get("changed").and_then(|value| value.as_bool()),
            Some(false)
        );
        assert_eq!(
            wrapper_run_state(&pool, "wrapper-refused", 0).await,
            ("active".to_string(), Some("wrapper-refused".to_string()), 1,)
        );

        let completed = serde_json::from_str::<serde_json::Value>(&complete_run_raw(
            Some(&pool),
            "wrapper-success",
            "test_wrapper_success",
            r#"{"releaseSlots":true}"#,
        ))
        .expect("decode successful wrapper response");
        assert_eq!(
            completed.get("ok").and_then(|value| value.as_bool()),
            Some(true)
        );
        assert_eq!(
            completed.get("changed").and_then(|value| value.as_bool()),
            Some(true)
        );
        assert_eq!(
            wrapper_run_state(&pool, "wrapper-success", 1).await,
            ("completed".to_string(), None, 0)
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
