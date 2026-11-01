use chrono::{DateTime, Utc};
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde_json::json;
use sqlx::{PgPool, Row as SqlxRow};

// ── Timeout policy typed facade (#3733) ─────────────────────────────
//
// Typed I/O for the stale-turn owner. Candidate timing and maintenance decisions
// belong to services::stale_turn_reconciler.

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

    let pg_observe = pg_pool.clone();
    obj.set(
        "__observeSessionHostRaw",
        Function::new(ctx.clone(), move |session_key: String| -> String {
            host_repair::observe_session_host_raw(pg_observe.as_ref(), &session_key)
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
            agentdesk.timeouts.observeSessionHost = function(sessionKey) {
                return unwrap(JSON.parse(agentdesk.timeouts.__observeSessionHostRaw(sessionKey || "")));
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
        policy_result(&host_repair::repair_stale_session_raw(
            self.pool,
            key,
            &request.to_string(),
        ))
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
    fn policy_timeout_surface_exposes_only_owner_and_host_observation() {
        let runtime = rquickjs::Runtime::new().unwrap();
        let context = rquickjs::Context::full(&runtime).unwrap();
        context.with(|ctx| {
            super::super::register_globals_with_supervisor_and_pg(
                &ctx,
                None,
                crate::supervisor::BridgeHandle::new(),
            )
            .unwrap();
            let keys: String = ctx
                .eval("JSON.stringify(Reflect.ownKeys(agentdesk.timeouts).map(String).sort())")
                .unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&keys).unwrap(),
                json!([
                    "__maintainStaleSessionsRaw",
                    "__maintainUnsentNotificationsRaw",
                    "__observeSessionHostRaw",
                    "maintainStaleSessions",
                    "maintainUnsentNotifications",
                    "observeSessionHost"
                ]),
                "policies must only see the maintenance owners and host observation"
            );
        });
    }

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
