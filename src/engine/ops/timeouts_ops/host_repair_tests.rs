use std::sync::{Arc, Mutex};

use rquickjs::{Function, Object};
use serde_json::{Value, json};
use sqlx::PgPool;

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind, upsert_hook_session_with_identity_pg,
};
use crate::db::dispatched_sessions::HookSessionUpsert;
use crate::db::dispatched_sessions::hosted_execution::tests::{
    TOKEN, future_schema, owner, pending, record, wire,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedState, install_pending_pg,
};

const AGENT: &str = "p4d-agent";
const STALE_REASON: &str = "Stale working session recovery — no active tmux session after 10min";

fn tmux_name(name: &str) -> String {
    format!("AgentDesk-claude-{name}")
}

fn key(name: &str) -> String {
    format!("claude/{TOKEN}/mac-mini:{}", tmux_name(name))
}

fn channel(index: usize) -> String {
    format!("14796713013870{index:05}")
}

async fn setup() -> (TestPostgresDb, PgPool) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ($1, $1, 'claude', 'p4d-agent-channel')",
    )
    .bind(AGENT)
    .execute(&pool)
    .await
    .unwrap();
    (db, pool)
}

async fn seed_dispatch(pool: &PgPool, id: &str, status: &str) {
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
         VALUES ($1, $1, 'in_progress', $2)",
    )
    .bind(format!("card-{id}"))
    .bind(AGENT)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches
            (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context)
         VALUES ($1, $2, $3, 'implementation', $4, $1, '{}')",
    )
    .bind(id)
    .bind(format!("card-{id}"))
    .bind(AGENT)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
}

/// A `turn_active` row silent for 40 minutes, bound to its own `dispatch-<name>`.
async fn seed(pool: &PgPool, name: &str, index: usize, raw: Option<Value>, dispatch: &str) {
    let (key, channel) = (key(name), channel(index));
    let params = HookSessionUpsert {
        session_key: &key,
        instance_id: Some("test-node"),
        agent_id: Some(AGENT),
        provider: "claude",
        status: "idle",
        session_info: None,
        model: None,
        tokens: None,
        cwd: None,
        active_dispatch_id: None,
        thread_channel_id: None,
        channel_id: Some(&channel),
        claude_session_id: None,
        raw_provider_session_id: None,
        turn_start_nonce: None,
        dispatched_origin: false,
    };
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: TOKEN,
        channel_id: &channel,
    };
    upsert_hook_session_with_identity_pg(pool, params, Some(identity))
        .await
        .unwrap();
    let dispatch_id = format!("dispatch-{name}");
    seed_dispatch(pool, &dispatch_id, dispatch).await;
    sqlx::query(
        "UPDATE sessions SET status = 'turn_active', active_dispatch_id = $2, hosted_execution = $3,
                last_heartbeat = NOW() - INTERVAL '40 minutes'
         WHERE session_key = $1",
    )
    .bind(&key)
    .bind(&dispatch_id)
    .bind(raw)
    .execute(pool)
    .await
    .unwrap();
}

/// Session status, dispatch link (none/own/other id), session_info, dispatch status/reason
/// and event count.
async fn snapshot(pool: &PgPool, name: &str) -> Value {
    let row: (
        String,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        i64,
    ) = sqlx::query_as(
        "SELECT s.status,
                    CASE WHEN s.active_dispatch_id IS NULL THEN 'none'
                         WHEN s.active_dispatch_id = td.id THEN 'own'
                         ELSE s.active_dispatch_id END,
                    s.session_info, td.status,
                    td.result::jsonb->>'reason',
                    (SELECT COUNT(*) FROM dispatch_events e WHERE e.dispatch_id = td.id)
             FROM sessions s, task_dispatches td
             WHERE s.session_key = $1 AND td.id = $2",
    )
    .bind(key(name))
    .bind(format!("dispatch-{name}"))
    .fetch_one(pool)
    .await
    .unwrap();
    json!(row)
}

async fn session_id(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM sessions WHERE session_key = $1")
        .bind(key(name))
        .fetch_optional(pool)
        .await
        .unwrap()
        .unwrap_or(-1)
}

fn observe(pool: &PgPool, key: &str, liveness: HostLiveness) -> (Value, Vec<String>) {
    let mut probed = Vec::new();
    let raw = observe_session_host_with(Some(pool), key, |session, _| {
        probed.push(session.name.to_string());
        liveness
    });
    (serde_json::from_str(&raw).unwrap(), probed)
}

async fn repair(
    pool: &PgPool,
    name: &str,
    id: i64,
    observed: &str,
    fail: bool,
    clear: bool,
) -> Value {
    let request = RepairRequest {
        session_id: id,
        active_dispatch_id: Some(format!("dispatch-{name}")),
        observed: observed.to_string(),
        fail_dispatch: fail,
        fail_reason: STALE_REASON.to_string(),
        clear_active_dispatch_id: clear,
    };
    repair_stale_session_pg(pool, &key(name), request)
        .await
        .unwrap()
}

fn command_refusal(pool: &PgPool, key: &str) -> Value {
    let refused = session_command_target(Some(pool), key, "kill").unwrap_err();
    serde_json::from_str(&refused).unwrap()
}

fn write_herdr_marker(name: &str) {
    let path = crate::services::tmux_common::session_temp_path(&tmux_name(name), "host_kind");
    std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
    std::fs::write(path, "herdr").unwrap();
}

// Each repair the policy can request on a legacy row lands the same rows as the base
// dispatch.markFailed + timeouts.markSessionIdle pair on an identical twin row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_row_repairs_match_the_base_policy_writes_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = setup().await;
    let cases = [
        ("stale-pending", "pending", true, true),
        ("stale-dispatched", "dispatched", true, true),
        ("stale-completed", "completed", false, true),
        ("deadlock", "pending", false, false),
    ];
    for (index, (name, dispatch, fail, clear)) in cases.into_iter().enumerate() {
        let base = format!("{name}-base");
        seed(&pool, name, 2 * index, None, dispatch).await;
        seed(&pool, &base, 2 * index + 1, None, dispatch).await;
        if fail {
            crate::dispatch::set_dispatch_status_with_backends(
                Some(&pool),
                &format!("dispatch-{base}"),
                "failed",
                Some(&json!({ "reason": STALE_REASON })),
                "js_dispatch_mark_failed_raw",
                Some(&["pending", "dispatched"]),
                false,
            )
            .unwrap();
        }
        sqlx::query(
            "UPDATE sessions SET status = 'idle',
                 active_dispatch_id = CASE WHEN $2 THEN NULL ELSE active_dispatch_id END,
                 last_heartbeat = NOW()
             WHERE session_key = $1 AND status IN ('turn_active', 'working')",
        )
        .bind(key(&base))
        .bind(clear)
        .execute(&pool)
        .await
        .unwrap();

        let (observed, probed) = observe(&pool, &key(name), HostLiveness::DeadOrAbsent);
        assert_eq!(observed["state"], "dead", "{name}: {observed}");
        assert_eq!(observed["tmux_name"], tmux_name(name));
        assert_eq!(
            probed,
            [tmux_name(name)],
            "{name}: one probe on the row's tmux name"
        );
        let id = observed["session_id"].as_i64().unwrap();
        let result = repair(&pool, name, id, "dead", fail, clear).await;
        assert_eq!(result["repaired"], true, "{name}: {result}");
        assert_eq!(
            snapshot(&pool, name).await,
            snapshot(&pool, &base).await,
            "{name}"
        );
    }
    let failed = snapshot(&pool, "stale-pending").await;
    assert_eq!(
        failed,
        json!(["idle", "none", "Dispatch failed", "failed", STALE_REASON, 1])
    );
    let kept = snapshot(&pool, "deadlock").await;
    assert_eq!(kept, json!(["idle", "own", null, "pending", null, 0]));
    assert_eq!(
        session_command_target(Some(&pool), &key("deadlock"), "kill"),
        Ok(tmux_name("deadlock"))
    );
    pool.close().await;
    db.drop().await;
}

// Herdr, unreadable, foreign, missing or marker-traced rows and a failed probe on a legacy
// row: no probe, no repair write and no tmux command target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_legacy_rows_defer_without_probe_write_or_tmux_command_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = setup().await;
    let herdr = "herdr_unsupported";
    let rows = [
        ("bound", Some(HostedState::Bound), "herdr", herdr),
        ("pending", Some(HostedState::Pending), "herdr", herdr),
        ("retired", Some(HostedState::Retired), "herdr", herdr),
        ("future", None, "host_unknown", "host_unknown"),
        ("foreign", None, "row_conflict", "row_conflict"),
        ("marked", None, "herdr", herdr),
    ];
    for (index, (name, state, reason, command)) in rows.into_iter().enumerate() {
        let row_owner = owner(&channel(100 + index));
        let raw = match (name, state) {
            (_, Some(state)) => Some(wire(&record(&row_owner, "n1", state))),
            ("future", _) => Some(future_schema(&row_owner)),
            ("foreign", _) => Some(wire(&pending(&owner(&channel(199)), "n9"))),
            _ => None,
        };
        seed(&pool, name, 100 + index, raw, "pending").await;
        if name == "marked" {
            write_herdr_marker(name);
        }
        let before = snapshot(&pool, name).await;
        let (observed, probed) = observe(&pool, &key(name), HostLiveness::DeadOrAbsent);
        assert_eq!(observed["state"], "unknown", "{name}: {observed}");
        assert_eq!(observed["reason"], reason, "{name}: {observed}");
        assert!(probed.is_empty(), "{name}: no tmux probe may run");
        let id = session_id(&pool, name).await;
        let result = repair(&pool, name, id, "dead", true, true).await;
        assert_eq!(result["repaired"], false, "{name}: {result}");
        assert_eq!(result["deferred"], reason, "{name}: {result}");
        assert_eq!(
            snapshot(&pool, name).await,
            before,
            "{name}: row and dispatch unchanged"
        );
        assert_eq!(
            command_refusal(&pool, &key(name))["reason"],
            command,
            "{name}"
        );
    }

    let (missing, probed) = observe(&pool, &key("absent"), HostLiveness::DeadOrAbsent);
    assert_eq!(
        (missing["reason"].as_str(), probed.len()),
        (Some("session_missing"), 0)
    );
    assert_eq!(
        command_refusal(&pool, &key("absent"))["reason"],
        "session_missing"
    );
    assert_eq!(
        command_refusal(&pool, &tmux_name("absent"))["reason"],
        "session_missing"
    );

    seed(&pool, "probe-error", 120, None, "pending").await;
    let before = snapshot(&pool, "probe-error").await;
    let (observed, probed) = observe(&pool, &key("probe-error"), HostLiveness::ProbeError);
    assert_eq!(observed["reason"], "probe_failed");
    assert_eq!(probed, [tmux_name("probe-error")]);
    let id = session_id(&pool, "probe-error").await;
    let result = repair(&pool, "probe-error", id, "unknown", true, true).await;
    assert_eq!(result["deferred"], "probe_failed", "{result}");
    assert_eq!(snapshot(&pool, "probe-error").await, before);
    pool.close().await;
    db.drop().await;
}

// A Herdr record installed, the dispatch link moved or the row recreated after the
// observation: the repair writes nothing, and a dispatch it failed is rolled back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repair_after_the_row_changed_since_observation_writes_nothing_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = setup().await;
    for (index, name) in ["installed", "moved", "recreated"].into_iter().enumerate() {
        seed(&pool, name, 200 + index, None, "pending").await;
    }
    let observed_id = |name: &str| {
        let (observed, _) = observe(&pool, &key(name), HostLiveness::DeadOrAbsent);
        assert_eq!(observed["state"], "dead", "{name}: {observed}");
        observed["session_id"].as_i64().unwrap()
    };

    let id = observed_id("installed");
    let HostedLookup::Found(legacy) =
        load_hosted_execution_pg(&pool, HostedLookupKey::SessionKey(&key("installed"))).await
    else {
        panic!("installed row");
    };
    install_pending_pg(&pool, &legacy, pending(&owner(&channel(200)), "n1"))
        .await
        .unwrap();
    let before = snapshot(&pool, "installed").await;
    let result = repair(&pool, "installed", id, "dead", true, true).await;
    assert_eq!(result["deferred"], "herdr", "{result}");
    assert_eq!(snapshot(&pool, "installed").await, before);

    let id = observed_id("moved");
    seed_dispatch(&pool, "dispatch-moved-next", "pending").await;
    sqlx::query("UPDATE sessions SET active_dispatch_id = 'dispatch-moved-next' WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let before = snapshot(&pool, "moved").await;
    let result = repair(&pool, "moved", id, "dead", true, true).await;
    assert_eq!(result["deferred"], "row_changed", "{result}");
    assert_eq!(
        snapshot(&pool, "moved").await,
        before,
        "dispatch fail rolled back"
    );

    let id = observed_id("recreated");
    sqlx::query("DELETE FROM sessions WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM task_dispatches WHERE id = 'dispatch-recreated'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM kanban_cards WHERE id = 'card-dispatch-recreated'")
        .execute(&pool)
        .await
        .unwrap();
    seed(&pool, "recreated", 203, None, "pending").await;
    let before = snapshot(&pool, "recreated").await;
    let result = repair(&pool, "recreated", id, "dead", true, true).await;
    assert_eq!(result["deferred"], "row_changed", "{result}");
    assert_eq!(snapshot(&pool, "recreated").await, before);
    pool.close().await;
    db.drop().await;
}

/// Runs the real `policies/timeouts/active-monitor.js` `_section_I` on the real ops; only
/// the tmux probe is faked (every probe answers dead). Returns the probed tmux names.
fn run_section_i(pool: &PgPool) -> Vec<String> {
    let read = |path: &str| {
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
            .unwrap()
    };
    let (helpers, monitor) = (
        read("policies/lib/timeouts-helpers.js"),
        read("policies/timeouts/active-monitor.js"),
    );
    let probed = Arc::new(Mutex::new(Vec::new()));
    let runtime = rquickjs::Runtime::new().unwrap();
    let context = rquickjs::Context::full(&runtime).unwrap();
    context.with(|ctx| {
        ctx.globals()
            .set("agentdesk", Object::new(ctx.clone()).unwrap())
            .unwrap();
        crate::engine::ops::log_ops::register_log_ops(&ctx).unwrap();
        crate::engine::ops::kv_ops::register_kv_ops(&ctx, Some(pool.clone())).unwrap();
        crate::engine::ops::exec_ops::register_exec_ops(&ctx).unwrap();
        super::super::register_timeouts_ops(&ctx, Some(pool.clone())).unwrap();
        let ad: Object = ctx.globals().get("agentdesk").unwrap();
        let timeouts: Object = ad.get("timeouts").unwrap();
        let (pool, probed) = (pool.clone(), probed.clone());
        let observe = move |session_key: String| -> String {
            observe_session_host_with(Some(&pool), &session_key, |session, _| {
                probed.lock().unwrap().push(session.name.to_string());
                HostLiveness::DeadOrAbsent
            })
        };
        timeouts
            .set(
                "__observeSessionHostRaw",
                Function::new(ctx.clone(), observe).unwrap(),
            )
            .unwrap();
        let script = format!(
            "(function() {{
                var helpers = {{ exports: {{}} }};
                (function(module, exports) {{ {helpers} }})(helpers, helpers.exports);
                var monitor = {{ exports: {{}} }};
                (function(module) {{ {monitor} }})(monitor);
                var timeouts = {{}};
                monitor.exports(timeouts, helpers.exports);
                timeouts._section_I();
                return true;
            }})()"
        );
        assert!(ctx.eval::<bool, _>(script).unwrap());
    });
    let probed = probed.lock().unwrap().clone();
    probed
}

// The real `_section_I` hands each row's full key to the facade: a Herdr row with or
// without a live inflight keeps its row, dispatch and deadlock counter; legacy is repaired.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn section_i_defers_herdr_rows_and_repairs_legacy_rows_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = setup().await;
    let herdr = |index| {
        Some(wire(&record(
            &owner(&channel(index)),
            "n1",
            HostedState::Bound,
        )))
    };
    seed(&pool, "legacy", 300, None, "pending").await;
    seed(&pool, "herdr-live", 301, herdr(301), "pending").await;
    seed(&pool, "herdr-idle", 302, herdr(302), "dispatched").await;
    let root = crate::cli::agentdesk_runtime_root().unwrap();
    let inflight_dir = root.join("runtime/discord_inflight/claude");
    std::fs::create_dir_all(&inflight_dir).unwrap();
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let inflight = json!({
        "session_key": key("herdr-live"), "tmux_session_name": tmux_name("herdr-live"),
        "channel_name": "herdr-live", "started_at": now, "updated_at": now,
        "request_owner_user_id": 1, "dispatch_id": "dispatch-herdr-live",
    });
    std::fs::write(
        inflight_dir.join(format!("{}.json", channel(301))),
        inflight.to_string(),
    )
    .unwrap();
    for name in ["herdr-live", "herdr-idle"] {
        sqlx::query("INSERT INTO kv_meta (key, value) VALUES ($1, '{}')")
            .bind(format!("deadlock_check:{}", key(name)))
            .execute(&pool)
            .await
            .unwrap();
    }
    let before = [
        snapshot(&pool, "herdr-live").await,
        snapshot(&pool, "herdr-idle").await,
    ];

    let probed = run_section_i(&pool);

    assert_eq!(
        probed,
        [tmux_name("legacy")],
        "only the legacy row is probed, once"
    );
    assert_eq!(
        snapshot(&pool, "legacy").await,
        json!(["idle", "none", "Dispatch failed", "failed", STALE_REASON, 1])
    );
    let after = [
        snapshot(&pool, "herdr-live").await,
        snapshot(&pool, "herdr-idle").await,
    ];
    assert_eq!(
        after, before,
        "Herdr rows keep status, dispatch link and dispatch"
    );
    let counters: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kv_meta WHERE key LIKE $1")
        .bind(format!(
            "deadlock_check:claude/{TOKEN}/mac-mini:AgentDesk-claude-herdr-%"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(counters, 2, "Herdr deadlock counters are not consumed");
    pool.close().await;
    db.drop().await;
}
