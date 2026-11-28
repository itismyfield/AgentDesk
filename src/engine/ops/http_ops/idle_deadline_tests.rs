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
