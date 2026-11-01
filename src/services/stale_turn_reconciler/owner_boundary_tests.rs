use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;

// Put an immutable clock before pg_catalog so the real queries see exact boundaries,
// including on the async bridge's separately connected pool.
pub(crate) async fn frozen_clock_pool(pool: &PgPool) -> PgPool {
    sqlx::raw_sql("CREATE SCHEMA owner_clock; CREATE FUNCTION owner_clock.now() RETURNS timestamptz LANGUAGE sql IMMUTABLE AS $$ SELECT timestamptz '2026-11-01 00:00:00+00' $$;")
        .execute(pool).await.unwrap();
    sqlx::postgres::PgPoolOptions::new()
        .connect_with(
            pool.connect_options()
                .as_ref()
                .clone()
                .options([("search_path", "owner_clock,public,pg_catalog")]),
        )
        .await
        .unwrap()
}

async fn seed(pool: &PgPool, key: &str, age: Option<i64>, dispatch: Option<&str>) {
    sqlx::query("INSERT INTO sessions (session_key, provider, status, active_dispatch_id, last_heartbeat) VALUES ($1, 'claude', 'turn_active', $2, NOW() - ($3::bigint * INTERVAL '1 second'))")
        .bind(key).bind(dispatch).bind(age).execute(pool).await.unwrap();
}

async fn status(pool: &PgPool, key: &str) -> String {
    sqlx::query_scalar("SELECT status FROM sessions WHERE session_key = $1")
        .bind(key)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn episode(id: u64) -> InflightObservation {
    InflightObservation::Present(
        crate::services::discord::zombie_foreground_release::InflightEpisodeIdentity {
            started_at: "2026-11-01 00:00:00".into(),
            user_msg_id: id,
            turn_nonce: Some(format!("turn-{id}")),
            turn_start_offset: None,
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_owner_preserves_the_four_policy_counterexamples_pg() {
    let db = TestPostgresDb::create().await;
    let original = db.connect_and_migrate().await;
    let pool = frozen_clock_pool(&original).await;
    for (key, age, dispatch) in [
        ("six-minute-missing", 360, None),
        ("eleven-minute-dispatch", 660, Some("pending-dispatch")),
        ("thirty-one-minute-ambiguous", 1860, None),
        ("eleven-minute-ready-inflight", 660, None),
    ] {
        seed(&pool, key, Some(age), dispatch).await;
    }
    let result = reconcile_stale_turns_matching_with_warrant_pg(
        &pool,
        None,
        |key, _| match key {
            "thirty-one-minute-ambiguous" => IndependentLiveness::LiveOrAmbiguous,
            "eleven-minute-ready-inflight" => IndependentLiveness::ReadyForInput,
            _ => IndependentLiveness::NoPane,
        },
        |key, _| {
            if key == "eleven-minute-ready-inflight" {
                episode(1)
            } else {
                InflightObservation::Absent
            }
        },
        Some((None, AxisBSite::PolicyTickStaleSweep)),
    )
    .await
    .unwrap();
    assert_eq!(result.reconciled, 2);
    for (key, expected) in [
        ("six-minute-missing", "idle"),
        ("eleven-minute-dispatch", "turn_active"),
        ("thirty-one-minute-ambiguous", "turn_active"),
        ("eleven-minute-ready-inflight", "idle"),
    ] {
        assert_eq!(status(&pool, key).await, expected, "{key}");
    }
    pool.close().await;
    original.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_owner_strict_five_minute_and_null_boundary_pg() {
    let db = TestPostgresDb::create().await;
    let original = db.connect_and_migrate().await;
    let pool = frozen_clock_pool(&original).await;
    for (key, age) in [
        ("before", Some(299)),
        ("equal", Some(300)),
        ("after", Some(301)),
        ("null", None),
    ] {
        seed(&pool, key, age, None).await;
    }
    assert_eq!(
        reconcile_stale_turns_matching_with_warrant_pg(
            &pool,
            None,
            |_, _| IndependentLiveness::NoPane,
            |_, _| InflightObservation::Absent,
            Some((None, AxisBSite::BootReconcileSweep)),
        )
        .await
        .unwrap()
        .reconciled,
        1
    );
    for (key, expected) in [
        ("before", "turn_active"),
        ("equal", "turn_active"),
        ("after", "idle"),
        ("null", "turn_active"),
    ] {
        assert_eq!(status(&pool, key).await, expected, "{key}");
    }
    pool.close().await;
    original.close().await;
    db.drop().await;
}

#[derive(Clone)]
struct TraceIo {
    row: serde_json::Value,
    observed: serde_json::Value,
    inflights: Vec<serde_json::Value>,
    repaired: serde_json::Value,
    fault: Option<&'static str>,
    trace: Vec<serde_json::Value>,
}

impl TraceIo {
    fn step(&mut self, name: &str, args: serde_json::Value) -> Result<(), String> {
        self.trace.push(serde_json::json!([name, args]));
        if self.fault == Some(name) {
            Err(format!("Error: injected {name}"))
        } else {
            Ok(())
        }
    }
}

impl PolicyMaintenanceIo for TraceIo {
    fn clear_fresh(&mut self, minutes: i32) -> Result<(), String> {
        self.step("clear_fresh", serde_json::json!([minutes]))
    }
    fn working(&mut self, minutes: i32) -> Result<Vec<serde_json::Value>, String> {
        self.step("working", serde_json::json!([minutes]))?;
        Ok(vec![self.row.clone()])
    }
    fn deadlocks(&mut self, minutes: i32, limit: i32) -> Result<Vec<serde_json::Value>, String> {
        self.step("deadlocks", serde_json::json!([minutes, limit]))?;
        Ok(vec![self.row.clone()])
    }
    fn observe(&mut self, key: &str) -> Result<serde_json::Value, String> {
        self.step("observe", serde_json::json!([key]))?;
        Ok(self.observed.clone())
    }
    fn inflights(&mut self) -> Result<Vec<serde_json::Value>, String> {
        self.step("inflights", serde_json::json!([]))?;
        Ok(self.inflights.clone())
    }
    fn repair(
        &mut self,
        key: &str,
        request: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.step("repair", serde_json::json!([key, request]))?;
        Ok(self.repaired.clone())
    }
    fn delete_counter(&mut self, key: &str) -> Result<(), String> {
        self.step("delete_counter", serde_json::json!([key]))
    }
    fn cleanup_inactive(&mut self) -> Result<(), String> {
        self.step("cleanup_inactive", serde_json::json!([]))
    }
    fn cleanup_history(&mut self, cutoff: i64) -> Result<(), String> {
        let retention = chrono::Utc::now().timestamp_millis() - cutoff;
        assert!((604_799_000..=604_801_000).contains(&retention));
        self.step("cleanup_history", serde_json::json!([]))
    }
    fn log(&mut self, level: &str, message: String) -> Result<(), String> {
        self.step("log", serde_json::json!([level, message]))
    }
}

// The pre-move policy is an independent executable oracle, including cache hits,
// ignored placeholders, errors and partial failure ordering, not a second Rust reducer.
fn legacy_trace(io: TraceIo) -> (Vec<serde_json::Value>, Option<String>) {
    use rquickjs::{Context, Function, Runtime};
    use serde_json::{Value, json};
    use std::{cell::RefCell, rc::Rc};
    let io = Rc::new(RefCell::new(io));
    let runtime = Runtime::new().unwrap();
    let context = Context::full(&runtime).unwrap();
    let error: Option<String> = context.with(|ctx| {
        let shared = io.clone();
        ctx.globals().set("__io", Function::new(ctx.clone(), move |name: String, args: String| {
            let args: Value = serde_json::from_str(&args).unwrap();
            let mut io = shared.borrow_mut();
            let result: Result<Value, String> = match name.as_str() {
                "clear_fresh" => io.clear_fresh(args[0].as_i64().unwrap() as i32).map(|_| Value::Null),
                "working" => io.working(args[0].as_i64().unwrap() as i32).map(|v| json!(v)),
                "deadlocks" => io.deadlocks(args[0].as_i64().unwrap() as i32, args[1].as_i64().unwrap() as i32).map(|v| json!(v)),
                "observe" => io.observe(args[0].as_str().unwrap()),
                "inflights" => io.inflights().map(|v| json!(v)),
                "repair" => io.repair(args[0].as_str().unwrap(), args[1].clone()),
                "delete_counter" => io.delete_counter(args[0].as_str().unwrap()).map(|_| Value::Null),
                "cleanup_inactive" => io.cleanup_inactive().map(|_| Value::Null),
                "cleanup_history" => io.cleanup_history(args[0].as_i64().unwrap()).map(|_| Value::Null),
                "log" => io.log(args[0].as_str().unwrap(), args[1].as_str().unwrap().to_string()).map(|_| Value::Null),
                _ => panic!("unmapped legacy IO {name}"),
            };
            match result { Ok(value) => json!({"value": value}), Err(error) => json!({"error": error}) }.to_string()
        }).unwrap()).unwrap();
        let helper = include_str!("../../../policies/lib/timeouts-helpers.js");
        let legacy = include_str!("../../../tests/fixtures/stale_busy_owner/active-monitor.js");
        let result: String = ctx.eval(format!(r#"(function() {{
            function call(name, args) {{ var r = JSON.parse(__io(name, JSON.stringify(args))); if (r.error) throw r.error; return r.value; }}
            var agentdesk = {{ timeouts: {{
                clearDeadlockCountersForFreshSessions: function(m) {{ call('clear_fresh', [m]); }},
                listStaleWorkingSessions: function(m) {{ return call('working', [m]); }},
                listDeadlockCandidates: function(m, n) {{ return call('deadlocks', [m, n]); }},
                observeSessionHost: function(k) {{ return call('observe', [k]); }},
                repairStaleSession: function(k, r) {{ return call('repair', [k, r]); }},
                cleanupDeadlockCountersForInactiveSessions: function() {{ call('cleanup_inactive', []); }},
                cleanupDeadlockHistoryBefore: function(t) {{ call('cleanup_history', [t]); }}
            }}, inflight: {{ list: function() {{ return call('inflights', []); }} }},
            kv: {{ delete: function(k) {{ call('delete_counter', [k]); }} }},
            log: {{ warn: function(m) {{ call('log', ['warn', m]); }}, info: function(m) {{ call('log', ['info', m]); }} }} }};
            var helpers = {{ exports: {{}} }};
            (function(module, exports) {{ {helper} }})(helpers, helpers.exports);
            var monitor = {{ exports: {{}} }};
            (function(module) {{ {legacy} }})(monitor);
            var policy = {{}}; monitor.exports(policy, helpers.exports);
            try {{ policy._section_I(); return 'null'; }} catch(e) {{ return JSON.stringify(String(e)); }}
        }})()"#)).unwrap();
        serde_json::from_str(&result).unwrap()
    });
    let trace = io.borrow().trace.clone();
    (trace, error)
}

#[test]
fn policy_owner_trace_matches_pre_move_policy_for_cache_liveness_and_failures() {
    use serde_json::{Value, json};
    let key = "provider:__proto__";
    let template = TraceIo {
        row: json!({"session_key": key, "active_dispatch_id": "d1", "active_dispatch_status": "pending", "active_turn_nonce": "turn-1"}),
        observed: json!({"state": "live", "session_id": 7, "tmux_name": "__proto__"}),
        inflights: vec![],
        repaired: json!({"repaired": true, "dispatch_rows_affected": 1, "dispatch_error": null}),
        fault: None,
        trace: vec![],
    };
    let mut cases = Vec::new();
    for state in ["live", "dead", "unknown", "other"] {
        for mode in [
            "absent",
            "managed",
            "external_input",
            "placeholder",
            "tmux-only",
            "other-key",
        ] {
            for status in ["pending", "dispatched", "completed"] {
                let mut io = template.clone();
                io.observed["state"] = json!(state);
                io.observed["reason"] = json!("probe_failed");
                io.row["active_dispatch_status"] = json!(status);
                if mode != "absent" {
                    let mut row = json!({"session_key": key, "tmux_session_name": "__proto__", "turn_source": mode,
                        "request_owner_user_id": 1, "user_msg_id": 2, "updated_at": "2026-10-30 00:00:00"});
                    if mode == "placeholder" {
                        row = json!({"session_key": key, "tmux_session_name": "__proto__", "session_id": null,
                            "request_owner_user_id": "0", "user_msg_id": 0, "rebind_origin": true,
                            "any_tool_used": false, "has_post_tool_text": false});
                    } else if mode == "tmux-only" {
                        row["session_key"] = json!("different");
                    } else if mode == "other-key" {
                        row["session_key"] = json!("different");
                        row["tmux_session_name"] = json!("different");
                    }
                    io.inflights.push(row);
                }
                cases.push(io);
            }
        }
    }
    for fault in [
        "observe",
        "inflights",
        "clear_fresh",
        "working",
        "deadlocks",
        "repair",
        "delete_counter",
        "cleanup_inactive",
        "cleanup_history",
    ] {
        let mut io = template.clone();
        io.fault = Some(fault);
        cases.push(io);
    }
    for observed in [
        Value::Null,
        json!({"state": "unknown", "reason": "herdr"}),
        json!({"state": "unknown", "reason": "host_conflict"}),
    ] {
        let mut io = template.clone();
        io.observed = observed;
        cases.push(io);
    }
    for key in [Value::Null, json!(""), json!("  ")] {
        let mut io = template.clone();
        io.row["session_key"] = key;
        cases.push(io);
    }
    for repaired in [
        json!({"repaired": false, "deferred": "row_changed"}),
        json!({"repaired": true, "dispatch_rows_affected": 0}),
        json!({"repaired": true, "dispatch_error": "dispatch transition failed"}),
    ] {
        let mut io = template.clone();
        io.repaired = repaired;
        cases.push(io);
    }
    assert_eq!(cases.len(), 90);
    for (index, mut io) in cases.into_iter().enumerate() {
        let expected = legacy_trace(io.clone());
        let error = maintain_policy_sessions(&mut io).err();
        assert_eq!((io.trace, error), expected, "legacy trace case {index}");
    }
}
