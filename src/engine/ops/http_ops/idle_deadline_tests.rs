use std::net::TcpListener;
use std::time::Duration;

use rquickjs::{Context, Runtime};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::{invoke_localhost_post, register_http_ops};
use crate::engine::loader::{
    ArmedDeadline, install_policy_interrupt_handler, new_eval_deadline_slot,
};
use crate::test_support::wait::await_within;
use crate::utils::bridge_deadline::ScopedBridgeDeadline;

const CLUSTER: &str = "/api/cluster/nodes";
const CANDIDATES: &str = "/api/sessions/idle-cleanup-candidates";
const SLOW: &str = "/api/sessions/slow/kill-tmux";
const READY: &str = "/api/sessions/ready/kill-tmux";
const SERVER_WAIT_CAP: Duration = Duration::from_secs(8);

#[test]
fn loopback_read_timeout_has_a_canonical_deadline_error() {
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/silent", silent.local_addr().unwrap());
    let _deadline = ScopedBridgeDeadline::new(Duration::from_millis(50));
    let reply: Value = serde_json::from_str(&invoke_localhost_post(&url, "{}")).unwrap();
    assert_eq!(reply, json!({"error": "response deadline exceeded"}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quickjs_idle_kill_settles_deadline_and_resumes_after_the_slow_post() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        tokio::time::timeout(SERVER_WAIT_CAP, serve_ticks(listener))
            .await
            .expect("both ticks must reach the loopback server")
    });
    let runtime = Runtime::new().unwrap();
    let ctx = Context::full(&runtime).unwrap();
    let slot = new_eval_deadline_slot();
    install_policy_interrupt_handler(&runtime, slot.clone(), None);

    ctx.with(|ctx| {
        ctx.globals().set("port", port).unwrap();
        ctx.eval::<(), _>(r#"
            var warnings = [], errors = [], infos = [];
            var timeouts = {};
            var rows = ["slow", "ready"].map(function(key, index) {
                return {
                    session_key: key, provider: "claude", instance_id: null,
                    last_seen_at: "2000-01-01T00:00:00Z",
                    idle_cursor_at: "2000-01-01T00:00:00Z", idle_cursor_id: String(index + 1)
                };
            });
            var agentdesk = {
                config: { get: function() { return port; } },
                db: { query: function(sql, params) {
                    if (sql.startsWith("SELECT DISTINCT")) return [{ instance_id: null }];
                    if (sql.indexOf("::bigint") >= 0) return rows.slice(Number(params[params.length - 1]));
                    return rows;
                } },
                log: {
                    warn: function(line) { warnings.push(line); },
                    error: function(line) { errors.push(line); },
                    info: function(line) { infos.push(line); }
                }
            };
            var module = { exports: {} };
        "#).unwrap();
        register_http_ops(&ctx).unwrap();
        ctx.eval::<(), _>(include_str!("../../../../policies/timeouts/idle-kill-owners.js")).unwrap();
        ctx.eval::<(), _>(r#"
            var createOwnerGuard = module.exports;
            var require = function(name) {
                if (name !== "./idle-kill-owners") throw Error(name);
                return createOwnerGuard;
            };
            module = { exports: {} };
        "#).unwrap();
        ctx.eval::<(), _>(include_str!("../../../../policies/timeouts/idle-kill.js")).unwrap();
        ctx.eval::<(), _>(r#"
            module.exports(timeouts, {
                loadAgentDirectory: function() { return []; },
                backfillMissingSessionAgentIds: function() {},
                parseSessionThreadId: function() { return null; },
                resolveSessionAgentContext: function() { return {}; }
            });
        "#).unwrap();
    });
    let tick = || {
        ctx.with(|ctx| {
            let _deadline = ArmedDeadline::new(&slot, Duration::from_millis(750));
            ctx.eval::<(), _>("timeouts._section_O();")
                .expect("tick must return before QuickJS interrupts");
        })
    };
    let logs = |name| ctx.with(|ctx| ctx.eval::<Vec<String>, _>(name).unwrap());

    tick();
    assert_eq!(
        logs("warnings"),
        ["[idle-kill] kill-tmux deadline reached; remaining=1"]
    );
    assert!(logs("errors").is_empty());
    assert!(logs("infos").is_empty());

    tick();
    assert_eq!(logs("warnings").len(), 1);
    assert!(logs("errors").is_empty());
    assert!(
        logs("infos")
            .iter()
            .any(|line| line.contains("Killed idle tmux") && line.ends_with("ready"))
    );
    drop(ctx);
    drop(runtime);
    await_within(server, SERVER_WAIT_CAP).await.unwrap();
}

async fn serve_ticks(listener: tokio::net::TcpListener) {
    let mut stalled = None;
    let keys = json!({ "keys": ["slow", "ready"] });
    let remaining = json!({ "keys": ["ready"] });
    let cluster = json!({
        "cluster": { "configured_forward_owner_ids": [], "local_instance_id": null },
        "nodes": [],
    });
    for (method, path, request, reply) in [
        ("GET", CLUSTER, Value::Null, Some(cluster.clone())),
        (
            "POST",
            CANDIDATES,
            keys,
            Some(json!({
                "decisions": {"slow": {"supported": true}, "ready": {"supported": true}}
            })),
        ),
        ("POST", SLOW, Value::Null, None),
        ("GET", CLUSTER, Value::Null, Some(cluster)),
        (
            "POST",
            CANDIDATES,
            remaining,
            Some(json!({
                "decisions": {"ready": {"supported": true}}
            })),
        ),
        (
            "POST",
            READY,
            Value::Null,
            Some(json!({
                "ok": true, "tmux_was_alive": true, "tmux_killed": true,
            })),
        ),
    ] {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        stream.read_line(&mut line).await.unwrap();
        assert_eq!(line, format!("{method} {path} HTTP/1.0\r\n"));
        let mut body_len = 0;
        loop {
            line.clear();
            assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            if let Some(len) = line.strip_prefix("Content-Length: ") {
                body_len = len.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; body_len];
        stream.read_exact(&mut body).await.unwrap();
        if !request.is_null() {
            assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), request);
        }
        if let Some(reply) = reply {
            let body = reply.to_string();
            let reply = format!(
                "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.get_mut().write_all(reply.as_bytes()).await.unwrap();
        } else {
            stalled = Some(stream);
        }
    }
    drop(stalled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_kill_db_work_deadline_settles_and_resumes_pg() {
    use axum::{
        Json, Router,
        extract::Path,
        routing::{get, post},
    };
    use std::sync::{Arc, Mutex};

    use crate::db::auto_queue::test_support::TestPostgresDb;

    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate_with_max_connections(4).await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let capture_requests = requests.clone();
    let router = Router::new()
        .route(
            CLUSTER,
            get(|| async {
                Json(json!({
                    "cluster": { "configured_forward_owner_ids": [], "local_instance_id": null },
                    "nodes": [],
                }))
            }),
        )
        .route(
            CANDIDATES,
            post(|Json(body): Json<Value>| async move {
                let decisions: serde_json::Map<_, _> = body["keys"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|key| (key.as_str().unwrap().to_owned(), json!({"supported": true})))
                    .collect();
                Json(json!({ "decisions": decisions }))
            }),
        )
        .route(
            "/api/sessions/{key}/kill-tmux",
            post(move |Path(key): Path<String>| {
                let requests = capture_requests.clone();
                async move {
                    requests.lock().unwrap().push(key);
                    Json(json!({"ok": true, "tmux_was_alive": true, "tmux_killed": true}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, shutdown) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = shutdown.await;
            })
            .await
            .unwrap();
    });
    let policy_pool = pool.clone();
    let checks = tokio::spawn(async move {
        for phase in ["agent", "owner", "page", "aux"] {
            requests.lock().unwrap().clear();
            let runtime = Runtime::new().unwrap();
            let ctx = Context::full(&runtime).unwrap();
            let slot = new_eval_deadline_slot();
            install_policy_interrupt_handler(&runtime, slot.clone(), None);
            ctx.with(|ctx| {
                ctx.globals().set("port", port).unwrap();
                ctx.globals().set("phase", phase).unwrap();
                load_pg_idle_policy(&ctx, policy_pool.clone());
                let tick = || {
                    let _deadline = ArmedDeadline::new(&slot, Duration::from_millis(900));
                    ctx.eval::<(), _>("timeouts._section_O();")
                        .unwrap_or_else(|error| panic!("{phase}: QuickJS tick failed: {error}"));
                };

                tick();
                assert!(ctx.eval::<bool, _>("delayed").unwrap(), "{phase}");
                assert_eq!(
                    ctx.eval::<Vec<String>, _>("warnings").unwrap(),
                    ["[idle-kill] kill-tmux deadline reached; remaining=unknown"],
                    "{phase}",
                );
                assert_eq!(ctx.eval::<usize, _>("errors.length").unwrap(), 0, "{phase}");
                let before: &[&str] = if phase == "aux" { &["slow"] } else { &[] };
                assert_eq!(requests.lock().unwrap().as_slice(), before, "{phase}");

                tick();
                assert_eq!(
                    ctx.eval::<usize, _>("warnings.length").unwrap(),
                    1,
                    "{phase}"
                );
                assert_eq!(ctx.eval::<usize, _>("errors.length").unwrap(), 0, "{phase}");
                assert_eq!(
                    requests.lock().unwrap().as_slice(),
                    ["slow", "ready"],
                    "{phase}"
                );
            });
        }
    });
    let checked = await_within(checks, Duration::from_secs(20)).await;
    let _ = stop.send(());
    await_within(server, SERVER_WAIT_CAP).await.unwrap();
    await_within(pool.close(), SERVER_WAIT_CAP).await;
    await_within(database.drop(), SERVER_WAIT_CAP).await;
    checked.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tick5min_reserves_idle_settlement_during_slow_g_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::engine::{hooks::Hook, loader::load_single_policy};
    use std::path::Path;

    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate_with_max_connections(2).await;
    let policy_pool = pool.clone();
    let checks = tokio::spawn(async move {
        let runtime = Runtime::new().unwrap();
        let ctx = Context::full(&runtime).unwrap();
        let slot = new_eval_deadline_slot();
        install_policy_interrupt_handler(&runtime, slot.clone(), None);
        ctx.with(|ctx| {
            ctx.eval::<(), _>(r#"
                var warnings = [], gQueries = 0, slowG = true, ownerGets = 0, killPosts = 0;
                var agentdesk = {
                    log: {warn: function(line) { warnings.push(line); }, debug: function() {}},
                    config: {get: function(key) { return key === "server_port" ? 8791 : 45; }},
                    pipeline: {
                        getConfig: function() { return {}; },
                        kickoffState: function() { return "requested"; },
                        nextGatedTarget: function() { return "in_progress"; }
                    },
                    http: {
                        get: function() {
                            ownerGets++;
                            return {cluster: {local_instance_id: null, configured_forward_owner_ids: []}, nodes: []};
                        },
                        post: function(url) {
                            if (url.indexOf("idle-cleanup-candidates") >= 0) {
                                return {decisions: {slow: {supported: true}}};
                            }
                            killPosts++;
                            return {ok: true, skipped_provider_activity_guard: true};
                        }
                    }
                };
            "#).unwrap();
            super::super::db_ops::register_db_ops(&ctx, Some(policy_pool.clone())).unwrap();
            ctx.eval::<(), _>(r#"
                var query = agentdesk.db.query;
                agentdesk.db.query = function(sql, params, reserve) {
                    if (sql.indexOf("FROM task_dispatches WHERE status IN") >= 0) {
                        gQueries++;
                        if (slowG) return query("SELECT 1 FROM pg_sleep(1.5)", [], reserve);
                    }
                    if (sql.indexOf("idle_cursor_at") >= 0) {
                        return [{
                            session_key: "slow", provider: "claude", instance_id: null,
                            idle_cursor_at: "2000-01-01T00:00:00Z", idle_cursor_id: "1"
                        }];
                    }
                    return [];
                };
            "#).unwrap();
        });
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("policies/timeouts.js");
        let policy = load_single_policy(&ctx, &file).unwrap();
        ctx.with(|ctx| {
            let tick = || {
                let _armed = ArmedDeadline::new(&slot, Duration::from_millis(900));
                let function = policy.hooks[&Hook::OnTick5min]
                    .clone()
                    .restore(&ctx)
                    .unwrap();
                function
                    .call::<_, ()>(())
                    .expect("onTick5min must return without a QuickJS interrupt");
                assert!(crate::utils::bridge_deadline::current_work_deadline().is_none());
            };

            tick();
            assert_eq!(ctx.eval::<i32, _>("gQueries").unwrap(), 1);
            assert_eq!(ctx.eval::<i32, _>("ownerGets + killPosts").unwrap(), 0);
            assert_eq!(
                ctx.eval::<Vec<String>, _>("warnings").unwrap(),
                ["[idle-kill] kill-tmux deadline reached; remaining=unknown"],
            );

            ctx.eval::<(), _>("slowG = false;").unwrap();
            tick();
            assert_eq!(ctx.eval::<i32, _>("gQueries").unwrap(), 2);
            assert_eq!(ctx.eval::<i32, _>("killPosts").unwrap(), 1);
            let warnings = ctx.eval::<Vec<String>, _>("warnings").unwrap();
            assert_eq!(warnings.len(), 1, "warnings={warnings:?}");
        });
    });
    let checked = await_within(checks, Duration::from_secs(20)).await;
    await_within(pool.close(), SERVER_WAIT_CAP).await;
    await_within(database.drop(), SERVER_WAIT_CAP).await;
    checked.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tick5min_excludes_hosted_records_in_candidate_sql_pg() {
    use axum::{
        Json, Router,
        extract::Path,
        routing::{get, post},
    };
    use std::sync::{Arc, Mutex};

    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedState,
        test_support::{TOKEN, owner, record, wire},
    };
    use crate::engine::{hooks::Hook, loader::load_single_policy};

    const LEGACY: &str = "fixture:AgentDesk-claude-legacy";
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate_with_max_connections(4).await;
    let hosted = (1..=192)
        .map(|index| {
            let identity = owner(&(800000 + index).to_string());
            wire(&record(
                &identity,
                &format!("hosted-{index}"),
                HostedState::Bound,
            ))
        })
        .collect();
    let inserted = sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, last_heartbeat, identity_kind,
                              discord_token_hash, channel_id, instance_id, hosted_execution)
         SELECT 'fixture:AgentDesk-claude-hosted-' || n, 'claude', 'idle',
                NOW() - INTERVAL '8 hours', 'discord_channel', $1,
                (800000 + n)::text, 'test-node', execution
         FROM jsonb_array_elements($2::jsonb) WITH ORDINALITY AS hosted(execution, n)",
    )
    .bind(TOKEN)
    .bind(Value::Array(hosted))
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(inserted.rows_affected(), 192);
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, last_heartbeat, identity_kind,
                              discord_token_hash, channel_id, instance_id)
         VALUES ($1, 'claude', 'idle', NOW() - INTERVAL '7 hours',
                 'discord_channel', $2, '801000', 'fixture-local')",
    )
    .bind(LEGACY)
    .bind(TOKEN)
    .execute(&pool)
    .await
    .unwrap();

    let candidates = Arc::new(Mutex::new(Vec::new()));
    let kills = Arc::new(Mutex::new(Vec::new()));
    let captured_candidates = candidates.clone();
    let captured_kills = kills.clone();
    let router = Router::new()
        .route(
            CLUSTER,
            get(|| async {
                Json(json!({
                    "cluster": {
                        "local_instance_id": "fixture-local",
                        "configured_forward_owner_ids": ["test-node"],
                        "lease_ttl_secs": 60,
                    },
                    "nodes": [{"instance_id": "test-node", "status": "online",
                               "last_heartbeat_at": chrono::Utc::now().to_rfc3339()}],
                }))
            }),
        )
        .route(
            CANDIDATES,
            post(move |Json(body): Json<Value>| {
                let candidates = captured_candidates.clone();
                async move {
                    let keys = body["keys"].clone();
                    candidates.lock().unwrap().push(keys.clone());
                    if keys != json!([LEGACY]) {
                        return Json(json!({"deadline_exceeded": true, "request_started": true}));
                    }
                    Json(json!({"decisions": {(LEGACY): {"supported": true}}}))
                }
            }),
        )
        .route(
            "/api/sessions/{key}/kill-tmux",
            post(move |Path(key): Path<String>| {
                let kills = captured_kills.clone();
                async move {
                    kills.lock().unwrap().push(key);
                    Json(json!({"ok": true, "skipped_provider_activity_guard": true}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, shutdown) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = shutdown.await;
            })
            .await
            .unwrap();
    });
    let policy_pool = pool.clone();
    let checks = tokio::spawn(async move {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let slot = new_eval_deadline_slot();
        install_policy_interrupt_handler(&runtime, slot.clone(), None);
        context.with(|ctx| {
            ctx.globals().set("port", port).unwrap();
            ctx.eval::<(), _>(
                r#"
                var warnings = [];
                var agentdesk = {
                    log: {warn: function(line) { warnings.push(line); },
                          debug: function() {}, info: function() {}},
                    config: {get: function(key) { return key === "server_port" ? port : 45; }},
                    pipeline: {
                        getConfig: function() { return {}; },
                        kickoffState: function() { return "requested"; },
                        nextGatedTarget: function() { return "in_progress"; }
                    }
                };
            "#,
            )
            .unwrap();
            super::super::db_ops::register_db_ops(&ctx, Some(policy_pool)).unwrap();
            register_http_ops(&ctx).unwrap();
        });
        let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("policies/timeouts.js");
        let policy = load_single_policy(&context, &file).unwrap();
        context.with(|ctx| {
            let _deadline = ArmedDeadline::new(&slot, Duration::from_secs(5));
            policy.hooks[&Hook::OnTick5min]
                .clone()
                .restore(&ctx)
                .unwrap()
                .call::<_, ()>(())
                .unwrap();
            assert!(ctx.eval::<Vec<String>, _>("warnings").unwrap().is_empty());
        });
    });
    let result = await_within(checks, SERVER_WAIT_CAP).await;
    let _ = stop.send(());
    await_within(server, SERVER_WAIT_CAP).await.unwrap();
    await_within(pool.close(), SERVER_WAIT_CAP).await;
    await_within(database.drop(), SERVER_WAIT_CAP).await;
    assert_eq!(candidates.lock().unwrap().as_slice(), [json!([LEGACY])]);
    assert_eq!(kills.lock().unwrap().as_slice(), [LEGACY]);
    result.unwrap();
}

fn load_pg_idle_policy(ctx: &rquickjs::Ctx<'_>, pool: sqlx::PgPool) {
    ctx.eval::<(), _>(
        r#"
        var warnings = [], errors = [];
        var timeouts = {}, delayed = false;
        var agentdesk = {
            config: { get: function() { return port; } },
            log: {
                warn: function(line) { warnings.push(line); },
                error: function(line) { errors.push(line); },
                debug: function() {}, info: function() {}
            }
        };
        var module = { exports: {} };
    "#,
    )
    .unwrap();
    register_http_ops(ctx).unwrap();
    super::super::db_ops::register_db_ops(ctx, Some(pool)).unwrap();
    ctx.eval::<(), _>(include_str!(
        "../../../../policies/timeouts/idle-kill-owners.js"
    ))
    .unwrap();
    ctx.eval::<(), _>(r#"
        var createOwnerGuard = module.exports;
        var require = function(name) {
            if (name !== "./idle-kill-owners") throw Error(name);
            return createOwnerGuard;
        };
        var query = agentdesk.db.query;
        agentdesk.db.query = function(sql, params, reserveMs) {
            var stage = sql.indexOf("FROM agents") >= 0 ? "agent" :
                sql.indexOf("SELECT DISTINCT") === 0 ? "owner" :
                sql.indexOf("idle_cursor_at") >= 0 ? "page" :
                sql.indexOf("FROM task_dispatches") >= 0 ? "aux" : "backfill";
            if (!delayed && phase === stage) {
                delayed = true;
                return query("SELECT 1 AS delayed FROM pg_sleep(1.5)", [], reserveMs);
            }
            if (stage === "page") {
                var after = sql.indexOf("::bigint") >= 0 ? Number(params[params.length - 1]) : 0;
                return query("SELECT * FROM (VALUES " +
                    "('slow', 'claude', '2000-01-01T00:00:00Z', '2000-01-01T00:00:00Z', '1', 'fake-dispatch')," +
                    "('ready', 'claude', '2000-01-01T00:00:00Z', '2000-01-01T00:00:00Z', '2', NULL)" +
                    ") AS fixture(session_key, provider, last_seen_at, idle_cursor_at, idle_cursor_id, active_dispatch_id) " +
                    "WHERE idle_cursor_id::bigint > ? ORDER BY idle_cursor_id", [after], reserveMs);
            }
            if (stage === "owner") return query("SELECT NULL::text AS instance_id", [], reserveMs);
            return query("SELECT 1 WHERE FALSE", [], reserveMs);
        };
        module = { exports: {} };
    "#).unwrap();
    ctx.eval::<(), _>(include_str!("../../../../policies/lib/timeouts-helpers.js"))
        .unwrap();
    ctx.eval::<(), _>("var helpers = module.exports; module = { exports: {} };")
        .unwrap();
    ctx.eval::<(), _>(include_str!("../../../../policies/timeouts/idle-kill.js"))
        .unwrap();
    ctx.eval::<(), _>("module.exports(timeouts, helpers);")
        .unwrap();
}