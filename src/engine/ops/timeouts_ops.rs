use chrono::{DateTime, Utc};
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde::Deserialize;
use serde_json::json;
use sqlx::{PgPool, Row as SqlxRow};

// ── Timeout policy typed facade (#3733) ─────────────────────────────
//
// Typed I/O for the stale-turn owner and compatibility operations. Candidate timing
// and maintenance decisions belong to services::stale_turn_reconciler.

pub(super) mod host_repair;

pub(super) fn register_timeouts_ops<'js>(ctx: &Ctx<'js>, pg_pool: Option<PgPool>) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let obj = Object::new(ctx.clone())?;

    let pg_maintenance = pg_pool.clone();
    obj.set(
        "__maintainStaleSessionsRaw",
        Function::new(ctx.clone(), move |ctx: Ctx<'js>| -> String {
            let mut io = PolicyIo {
                pool: pg_maintenance.as_ref(),
                ctx,
            };
            match crate::services::stale_turn_reconciler::maintain_policy_sessions(&mut io) {
                Ok(()) => json!({"ok": true}).to_string(),
                Err(error) => json!({"error": error}).to_string(),
            }
        })?,
    )?;

    let pg_clear_fresh = pg_pool.clone();
    obj.set(
        "__clearDeadlockCountersForFreshSessionsRaw",
        Function::new(ctx.clone(), move |stale_scan_minutes: i32| -> String {
            clear_deadlock_counters_for_fresh_sessions_raw(
                pg_clear_fresh.as_ref(),
                stale_scan_minutes,
            )
        })?,
    )?;

    let pg_stale_working = pg_pool.clone();
    obj.set(
        "__listStaleWorkingSessionsRaw",
        Function::new(ctx.clone(), move |grace_minutes: i32| -> String {
            list_stale_working_sessions_raw(pg_stale_working.as_ref(), grace_minutes)
        })?,
    )?;

    let pg_candidates = pg_pool.clone();
    obj.set(
        "__listDeadlockCandidatesRaw",
        Function::new(
            ctx.clone(),
            move |stale_scan_minutes: i32, limit: i32| -> String {
                list_deadlock_candidates_raw(pg_candidates.as_ref(), stale_scan_minutes, limit)
            },
        )?,
    )?;

    let pg_observe = pg_pool.clone();
    obj.set(
        "__observeSessionHostRaw",
        Function::new(ctx.clone(), move |session_key: String| -> String {
            host_repair::observe_session_host_raw(pg_observe.as_ref(), &session_key)
        })?,
    )?;

    let pg_repair = pg_pool.clone();
    obj.set(
        "__repairStaleSessionRaw",
        Function::new(
            ctx.clone(),
            move |session_key: String, request_json: String| -> String {
                host_repair::repair_stale_session_raw(
                    pg_repair.as_ref(),
                    &session_key,
                    &request_json,
                )
            },
        )?,
    )?;

    let pg_dispatch_type = pg_pool.clone();
    obj.set(
        "__getDispatchTypeRaw",
        Function::new(ctx.clone(), move |dispatch_id: String| -> String {
            get_dispatch_type_raw(pg_dispatch_type.as_ref(), &dispatch_id)
        })?,
    )?;

    let pg_record = pg_pool.clone();
    obj.set(
        "__recordDeadlockTerminationRaw",
        Function::new(ctx.clone(), move |payload_json: String| -> String {
            record_deadlock_termination_raw(pg_record.as_ref(), &payload_json)
        })?,
    )?;

    let pg_cleanup_counters = pg_pool.clone();
    obj.set(
        "__cleanupDeadlockCountersForInactiveSessionsRaw",
        Function::new(ctx.clone(), move || -> String {
            cleanup_deadlock_counters_for_inactive_sessions_raw(pg_cleanup_counters.as_ref())
        })?,
    )?;

    let pg_cleanup_history = pg_pool.clone();
    obj.set(
        "__cleanupDeadlockHistoryBeforeRaw",
        Function::new(ctx.clone(), move |cutoff_ms: i64| -> String {
            cleanup_deadlock_history_before_raw(pg_cleanup_history.as_ref(), cutoff_ms)
        })?,
    )?;

    ad.set("timeouts", obj)?;
    super::exec_ops::register_session_command_ops(ctx, pg_pool)?;

    ctx.eval::<(), _>(
        r#"
        (function() {
            function unwrap(result) {
                if (result.error) throw new Error(result.error);
                return result;
            }
            agentdesk.timeouts.maintainStaleSessions = function() {
                return unwrap(JSON.parse(agentdesk.timeouts.__maintainStaleSessionsRaw()));
            };
            agentdesk.timeouts.clearDeadlockCountersForFreshSessions = function(staleScanMinutes) {
                return unwrap(JSON.parse(agentdesk.timeouts.__clearDeadlockCountersForFreshSessionsRaw(staleScanMinutes)));
            };
            agentdesk.timeouts.listStaleWorkingSessions = function(graceMinutes) {
                var result = unwrap(JSON.parse(agentdesk.timeouts.__listStaleWorkingSessionsRaw(graceMinutes)));
                return result.sessions || [];
            };
            agentdesk.timeouts.listDeadlockCandidates = function(staleScanMinutes, limit) {
                var result = unwrap(JSON.parse(agentdesk.timeouts.__listDeadlockCandidatesRaw(staleScanMinutes, limit || 50)));
                return result.sessions || [];
            };
            agentdesk.timeouts.observeSessionHost = function(sessionKey) {
                return unwrap(JSON.parse(agentdesk.timeouts.__observeSessionHostRaw(sessionKey || "")));
            };
            agentdesk.timeouts.repairStaleSession = function(sessionKey, request) {
                return unwrap(JSON.parse(agentdesk.timeouts.__repairStaleSessionRaw(
                    sessionKey || "",
                    JSON.stringify(request || {})
                )));
            };
            agentdesk.timeouts.getDispatchType = function(dispatchId) {
                if (!dispatchId) return null;
                var result = unwrap(JSON.parse(agentdesk.timeouts.__getDispatchTypeRaw(dispatchId || "")));
                return result.dispatch_type || null;
            };
            agentdesk.timeouts.recordDeadlockTermination = function(payload) {
                return unwrap(JSON.parse(agentdesk.timeouts.__recordDeadlockTerminationRaw(JSON.stringify(payload || {}))));
            };
            agentdesk.timeouts.cleanupDeadlockCountersForInactiveSessions = function() {
                return unwrap(JSON.parse(agentdesk.timeouts.__cleanupDeadlockCountersForInactiveSessionsRaw()));
            };
            agentdesk.timeouts.cleanupDeadlockHistoryBefore = function(cutoffMs) {
                return unwrap(JSON.parse(agentdesk.timeouts.__cleanupDeadlockHistoryBeforeRaw(cutoffMs || 0)));
            };
        })();
        "#,
    )?;

    Ok(())
}

struct PolicyIo<'pool, 'js> {
    pool: Option<&'pool PgPool>,
    ctx: Ctx<'js>,
}

impl<'js> PolicyIo<'_, 'js> {
    // Keep the existing JS JSON projection, including its number representation.
    fn projected<A: rquickjs::function::IntoArgs<'js>>(
        &self,
        object: &str,
        method: &str,
        args: A,
    ) -> Result<serde_json::Value, String> {
        let value: rquickjs::Value = self.call(object, method, args)?;
        let raw = self
            .ctx
            .json_stringify(value)
            .map_err(|error| error.to_string())?;
        match raw {
            Some(raw) => policy_result(&raw.to_string().map_err(|error| error.to_string())?),
            None => Ok(serde_json::Value::Null),
        }
    }

    fn call<A: rquickjs::function::IntoArgs<'js>, R: rquickjs::FromJs<'js>>(
        &self,
        object: &str,
        method: &str,
        args: A,
    ) -> Result<R, String> {
        let result = (|| -> JsResult<R> {
            let ad: Object = self.ctx.globals().get("agentdesk")?;
            let object: Object = ad.get(object)?;
            let function: Function = object.get(method)?;
            function.call(args)
        })();
        result.map_err(|error| {
            if matches!(error, rquickjs::Error::Exception) {
                let thrown = self.ctx.catch();
                let text: JsResult<String> = self
                    .ctx
                    .globals()
                    .get::<_, Function>("String")
                    .and_then(|string| string.call((thrown,)));
                text.unwrap_or_else(|_| "Error: unknown".to_string())
            } else {
                error.to_string()
            }
        })
    }
}

fn policy_result(raw: &str) -> Result<serde_json::Value, String> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|error| error.to_string())?;
    if let Some(error) = value["error"].as_str().filter(|error| !error.is_empty()) {
        return Err(error.to_string());
    }
    Ok(value)
}

impl crate::services::stale_turn_reconciler::PolicyMaintenanceIo for PolicyIo<'_, '_> {
    fn clear_fresh(&mut self, minutes: i32) -> Result<(), String> {
        policy_result(&clear_deadlock_counters_for_fresh_sessions_raw(
            self.pool, minutes,
        ))
        .map(|_| ())
    }
    fn working(&mut self, minutes: i32) -> Result<Vec<serde_json::Value>, String> {
        let result = policy_result(&list_stale_working_sessions_raw(self.pool, minutes))?;
        Ok(result["sessions"].as_array().cloned().unwrap_or_default())
    }
    fn deadlocks(&mut self, minutes: i32, limit: i32) -> Result<Vec<serde_json::Value>, String> {
        let result = policy_result(&list_deadlock_candidates_raw(self.pool, minutes, limit))?;
        Ok(result["sessions"].as_array().cloned().unwrap_or_default())
    }
    fn observe(&mut self, key: &str) -> Result<serde_json::Value, String> {
        self.projected("timeouts", "observeSessionHost", (key,))
    }
    fn inflights(&mut self) -> Result<Vec<serde_json::Value>, String> {
        Ok(self
            .projected("inflight", "list", ())?
            .as_array()
            .cloned()
            .unwrap_or_default())
    }
    fn repair(
        &mut self,
        key: &str,
        request: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let raw: String = self.call(
            "timeouts",
            "__repairStaleSessionRaw",
            (key, request.to_string()),
        )?;
        policy_result(&raw)
    }
    fn delete_counter(&mut self, key: &str) -> Result<(), String> {
        self.call("kv", "delete", (key,))
    }
    fn cleanup_inactive(&mut self) -> Result<(), String> {
        policy_result(&cleanup_deadlock_counters_for_inactive_sessions_raw(
            self.pool,
        ))
        .map(|_| ())
    }
    fn cleanup_history(&mut self, cutoff: i64) -> Result<(), String> {
        policy_result(&cleanup_deadlock_history_before_raw(self.pool, cutoff)).map(|_| ())
    }
    fn log(&mut self, level: &str, message: String) -> Result<(), String> {
        self.call("log", level, (message,))
    }
}

fn unavailable() -> String {
    json!({ "error": "postgres backend is required for agentdesk.timeouts" }).to_string()
}

fn valid_minutes(value: i32, field: &str) -> Result<i32, String> {
    if !(1..=24 * 60).contains(&value) {
        return Err(format!("{field} must be between 1 and 1440 minutes"));
    }
    Ok(value)
}

fn valid_limit(value: i32, field: &str) -> Result<i32, String> {
    if !(1..=500).contains(&value) {
        return Err(format!("{field} must be between 1 and 500"));
    }
    Ok(value)
}

fn valid_session_key(session_key: &str) -> Result<String, String> {
    let trimmed = session_key.trim();
    if trimmed.is_empty() {
        return Err("session_key is required".to_string());
    }
    Ok(trimmed.to_string())
}

fn valid_cutoff_ms(cutoff_ms: i64) -> Result<i64, String> {
    if cutoff_ms <= 0 {
        return Err("cutoff_ms must be positive".to_string());
    }
    Ok(cutoff_ms)
}

fn format_ts(value: Option<DateTime<Utc>>) -> serde_json::Value {
    match value {
        Some(value) => json!(value.format("%Y-%m-%d %H:%M:%S").to_string()),
        None => serde_json::Value::Null,
    }
}

fn clear_deadlock_counters_for_fresh_sessions_raw(
    pg_pool: Option<&PgPool>,
    stale_scan_minutes: i32,
) -> String {
    let minutes = match valid_minutes(stale_scan_minutes, "stale_scan_minutes") {
        Ok(value) => value,
        Err(error) => return json!({ "error": error }).to_string(),
    };
    let Some(pool) = pg_pool else {
        return unavailable();
    };
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let rows_affected = sqlx::query(
                "DELETE FROM kv_meta
                 WHERE key IN (
                    SELECT 'deadlock_check:' || session_key
                    FROM sessions
                    WHERE status IN ('turn_active', 'working')
                      AND last_heartbeat >= NOW() - ($1::int * INTERVAL '1 minute')
                 )",
            )
            .bind(minutes)
            .execute(&bridge_pool)
            .await
            .map_err(|error| format!("clear fresh-session deadlock counters: {error}"))?
            .rows_affected();
            Ok(json!({ "ok": true, "deleted": rows_affected }).to_string())
        },
        |error| json!({ "error": error }).to_string(),
    ) {
        Ok(result) => result,
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}

fn list_stale_working_sessions_raw(pg_pool: Option<&PgPool>, grace_minutes: i32) -> String {
    let minutes = match valid_minutes(grace_minutes, "grace_minutes") {
        Ok(value) => value,
        Err(error) => return json!({ "error": error }).to_string(),
    };
    let Some(pool) = pg_pool else {
        return unavailable();
    };
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let rows = sqlx::query(
                "SELECT s.session_key,
                        s.active_dispatch_id,
                        s.active_turn_nonce,
                        td.status AS active_dispatch_status
                 FROM sessions s
                 LEFT JOIN task_dispatches td ON td.id = s.active_dispatch_id
                 WHERE s.status IN ('turn_active', 'working')
                   AND s.last_heartbeat < NOW() - ($1::int * INTERVAL '1 minute')
                 ORDER BY s.last_heartbeat ASC",
            )
            .bind(minutes)
            .fetch_all(&bridge_pool)
            .await
            .map_err(|error| format!("list stale working sessions: {error}"))?;

            let sessions = rows
                .into_iter()
                .map(|row| {
                    json!({
                        "session_key": row.try_get::<Option<String>, _>("session_key").ok().flatten(),
                        "active_dispatch_id": row.try_get::<Option<String>, _>("active_dispatch_id").ok().flatten(),
                        "active_dispatch_status": row.try_get::<Option<String>, _>("active_dispatch_status").ok().flatten(),
                        "active_turn_nonce": row.try_get::<Option<String>, _>("active_turn_nonce").ok().flatten()
                    })
                })
                .collect::<Vec<_>>();
            Ok(json!({ "sessions": sessions }).to_string())
        },
        |error| json!({ "error": error }).to_string(),
    ) {
        Ok(result) => result,
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}

fn list_deadlock_candidates_raw(
    pg_pool: Option<&PgPool>,
    stale_scan_minutes: i32,
    limit: i32,
) -> String {
    let minutes = match valid_minutes(stale_scan_minutes, "stale_scan_minutes") {
        Ok(value) => value,
        Err(error) => return json!({ "error": error }).to_string(),
    };
    let limit = match valid_limit(limit, "limit") {
        Ok(value) => value,
        Err(error) => return json!({ "error": error }).to_string(),
    };
    let Some(pool) = pg_pool else {
        return unavailable();
    };
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let rows = sqlx::query(
                "SELECT session_key,
                        agent_id,
                        active_dispatch_id,
                        active_turn_nonce,
                        last_heartbeat
                 FROM sessions
                 WHERE status IN ('turn_active', 'working')
                   AND session_key NOT LIKE '%deadlock-manager%'
                   AND last_heartbeat < NOW() - ($1::int * INTERVAL '1 minute')
                 ORDER BY last_heartbeat ASC
                 LIMIT $2",
            )
            .bind(minutes)
            .bind(limit)
            .fetch_all(&bridge_pool)
            .await
            .map_err(|error| format!("list deadlock candidates: {error}"))?;

            let sessions = rows
                .into_iter()
                .map(|row| {
                    let last_heartbeat =
                        row.try_get::<Option<DateTime<Utc>>, _>("last_heartbeat")
                            .ok()
                            .flatten();
                    json!({
                        "session_key": row.try_get::<Option<String>, _>("session_key").ok().flatten(),
                        "agent_id": row.try_get::<Option<String>, _>("agent_id").ok().flatten(),
                        "active_dispatch_id": row.try_get::<Option<String>, _>("active_dispatch_id").ok().flatten(),
                        "active_turn_nonce": row.try_get::<Option<String>, _>("active_turn_nonce").ok().flatten(),
                        "last_heartbeat": format_ts(last_heartbeat)
                    })
                })
                .collect::<Vec<_>>();
            Ok(json!({ "sessions": sessions }).to_string())
        },
        |error| json!({ "error": error }).to_string(),
    ) {
        Ok(result) => result,
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}

fn get_dispatch_type_raw(pg_pool: Option<&PgPool>, dispatch_id: &str) -> String {
    let dispatch_id = dispatch_id.trim().to_string();
    if dispatch_id.is_empty() {
        return json!({ "dispatch_type": null }).to_string();
    }
    let Some(pool) = pg_pool else {
        return unavailable();
    };
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let dispatch_type = sqlx::query_scalar::<_, Option<String>>(
                "SELECT dispatch_type FROM task_dispatches WHERE id = $1",
            )
            .bind(&dispatch_id)
            .fetch_optional(&bridge_pool)
            .await
            .map_err(|error| format!("load dispatch type {dispatch_id}: {error}"))?
            .flatten();
            Ok(json!({ "dispatch_type": dispatch_type }).to_string())
        },
        |error| json!({ "error": error }).to_string(),
    ) {
        Ok(result) => result,
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}

#[derive(Deserialize)]
struct DeadlockTerminationPayload {
    session_key: String,
    dispatch_id: Option<String>,
    reason_text: Option<String>,
    probe_snapshot: Option<String>,
    tmux_alive: bool,
}

fn record_deadlock_termination_raw(pg_pool: Option<&PgPool>, payload_json: &str) -> String {
    let payload: DeadlockTerminationPayload = match serde_json::from_str(payload_json) {
        Ok(payload) => payload,
        Err(error) => {
            return json!({ "error": format!("invalid deadlock termination payload: {error}") })
                .to_string();
        }
    };
    let session_key = match valid_session_key(&payload.session_key) {
        Ok(value) => value,
        Err(error) => return json!({ "error": error }).to_string(),
    };
    let dispatch_id = payload.dispatch_id.and_then(|value| {
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    });
    let Some(pool) = pg_pool else {
        return unavailable();
    };
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let rows_affected = sqlx::query(
                "INSERT INTO session_termination_events
                 (session_key, dispatch_id, killer_component, reason_code, reason_text, probe_snapshot, tmux_alive)
                 VALUES ($1, $2, 'deadlock_policy', 'deadlock_timeout', $3, $4, $5)",
            )
            .bind(&session_key)
            .bind(dispatch_id)
            .bind(payload.reason_text)
            .bind(payload.probe_snapshot)
            .bind(if payload.tmux_alive { 1_i32 } else { 0_i32 })
            .execute(&bridge_pool)
            .await
            .map_err(|error| format!("record deadlock termination {session_key}: {error}"))?
            .rows_affected();
            Ok(json!({ "ok": true, "rows_affected": rows_affected }).to_string())
        },
        |error| json!({ "error": error }).to_string(),
    ) {
        Ok(result) => result,
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}

fn cleanup_deadlock_counters_for_inactive_sessions_raw(pg_pool: Option<&PgPool>) -> String {
    let Some(pool) = pg_pool else {
        return unavailable();
    };
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let rows_affected = sqlx::query(
                "DELETE FROM kv_meta
                 WHERE key LIKE 'deadlock_check:%'
                   AND REPLACE(key, 'deadlock_check:', '') NOT IN (
                      SELECT session_key
                      FROM sessions
                      WHERE status IN ('turn_active', 'working')
                   )",
            )
            .execute(&bridge_pool)
            .await
            .map_err(|error| format!("cleanup inactive deadlock counters: {error}"))?
            .rows_affected();
            Ok(json!({ "ok": true, "deleted": rows_affected }).to_string())
        },
        |error| json!({ "error": error }).to_string(),
    ) {
        Ok(result) => result,
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}

fn cleanup_deadlock_history_before_raw(pg_pool: Option<&PgPool>, cutoff_ms: i64) -> String {
    let cutoff_ms = match valid_cutoff_ms(cutoff_ms) {
        Ok(value) => value,
        Err(error) => return json!({ "error": error }).to_string(),
    };
    let Some(pool) = pg_pool else {
        return unavailable();
    };
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let rows_affected = sqlx::query(
                "DELETE FROM kv_meta
                 WHERE key LIKE 'deadlock_history:%'
                   AND regexp_replace(key, '^.*:', '') ~ '^[0-9]+$'
                   AND regexp_replace(key, '^.*:', '')::bigint < $1",
            )
            .bind(cutoff_ms)
            .execute(&bridge_pool)
            .await
            .map_err(|error| format!("cleanup deadlock history before {cutoff_ms}: {error}"))?
            .rows_affected();
            Ok(json!({ "ok": true, "deleted": rows_affected }).to_string())
        },
        |error| json!({ "error": error }).to_string(),
    ) {
        Ok(result) => result,
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_observation_keeps_the_existing_js_projection_and_error() {
        use crate::services::stale_turn_reconciler::PolicyMaintenanceIo;
        let runtime = rquickjs::Runtime::new().unwrap();
        let context = rquickjs::Context::full(&runtime).unwrap();
        context.with(|ctx| {
            ctx.eval::<(), _>("var agentdesk = {session: {}};").unwrap();
            register_timeouts_ops(&ctx, None).unwrap();
            let ad: Object = ctx.globals().get("agentdesk").unwrap();
            let timeouts: Object = ad.get("timeouts").unwrap();
            for (raw, expected) in [
                (
                    r#"{"state":"live","session_id":9007199254740993}"#,
                    Ok(json!({"state":"live","session_id":9007199254740992_i64})),
                ),
                (
                    "null",
                    Err("TypeError: cannot read property 'error' of null".to_string()),
                ),
                (r#"{"error":"probe"}"#, Err("Error: probe".to_string())),
            ] {
                timeouts
                    .set(
                        "__observeSessionHostRaw",
                        Function::new(ctx.clone(), move |_: String| raw.to_string()).unwrap(),
                    )
                    .unwrap();
                let mut io = PolicyIo {
                    pool: None,
                    ctx: ctx.clone(),
                };
                assert_eq!(io.observe("key"), expected);
            }
        });
    }

    #[test]
    fn validation_rejects_out_of_range_minutes() {
        assert!(valid_minutes(0, "m").is_err());
        assert!(valid_minutes(1441, "m").is_err());
        assert_eq!(valid_minutes(30, "m").unwrap(), 30);
    }

    #[test]
    fn validation_rejects_empty_session_key() {
        assert!(valid_session_key("").is_err());
        assert!(valid_session_key("  ").is_err());
        assert_eq!(
            valid_session_key(" provider:tmux ").unwrap(),
            "provider:tmux"
        );
    }
}
