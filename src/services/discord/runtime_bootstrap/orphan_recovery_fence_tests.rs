//! Boot orphan-dispatch recovery clears stale session links and requeues orphan dispatches.

use super::recover_orphan_pending_dispatches_once;
use sqlx::PgPool;

async fn card_and_dispatch(pool: &PgPool, agent: &str, dispatch: &str, status: &str) {
    let card = format!("card-{dispatch}");
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
         VALUES ($1, $1, 'in_progress', $2)",
    )
    .bind(&card)
    .bind(agent)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches
            (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at)
         VALUES ($1, $2, $3, 'implementation', $4, $1, NOW() - INTERVAL '2 days')",
    )
    .bind(dispatch)
    .bind(&card)
    .bind(agent)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO kv_meta (key, value) VALUES ($1, '1')")
        .bind(format!("dispatch_notified:{dispatch}"))
        .execute(pool)
        .await
        .unwrap();
}

async fn active_session(pool: &PgPool, agent: &str, channel: u64, dispatch: &str) {
    sqlx::query(
        "INSERT INTO sessions (session_key, agent_id, status, active_dispatch_id, channel_id)
         VALUES ($1, $2, 'turn_active', $3, $4)",
    )
    .bind(format!("c2b2a-session-{agent}"))
    .bind(agent)
    .bind(dispatch)
    .bind(channel.to_string())
    .execute(pool)
    .await
    .unwrap();
}

async fn session_link(pool: &PgPool, agent: &str) -> (String, Option<String>) {
    sqlx::query_as("SELECT status, active_dispatch_id FROM sessions WHERE agent_id = $1")
        .bind(agent)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn notified(pool: &PgPool, dispatch: &str) -> (bool, bool) {
    let marker: Option<String> = sqlx::query_scalar("SELECT key FROM kv_meta WHERE key = $1")
        .bind(format!("dispatch_notified:{dispatch}"))
        .fetch_optional(pool)
        .await
        .unwrap();
    let outbox: Option<String> = sqlx::query_scalar(
        "SELECT status FROM dispatch_outbox WHERE dispatch_id = $1 AND action = 'notify'",
    )
    .bind(dispatch)
    .fetch_optional(pool)
    .await
    .unwrap();
    (marker.is_some(), outbox.as_deref() == Some("pending"))
}

/// A terminal dispatch's session link is cleared and a pending dispatch's stale marker is
/// replaced by a pending notify outbox row.
#[tokio::test]
async fn orphan_recovery_clears_stale_links_and_requeues_pending_dispatches_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let sibling = 6_325_580_003u64;
    crate::db::agents::insert_agent_channels_for_tests(
        &pool,
        "c2b2a-b",
        Some(&sibling.to_string()),
        None,
    )
    .await;
    card_and_dispatch(&pool, "c2b2a-b", "c2b2a-done-b", "completed").await;
    active_session(&pool, "c2b2a-b", sibling, "c2b2a-done-b").await;
    card_and_dispatch(&pool, "c2b2a-b", "c2b2a-pending-b", "pending").await;
    let pid = crate::cli::agentdesk_runtime_root().unwrap();
    let pid = pid.join("runtime").join("dcserver.pid");
    std::fs::create_dir_all(pid.parent().unwrap()).unwrap();
    std::fs::write(&pid, "1").unwrap();
    recover_orphan_pending_dispatches_once(Some(&pool)).await;

    assert_eq!(
        session_link(&pool, "c2b2a-b").await,
        ("idle".to_string(), None)
    );
    assert_eq!(notified(&pool, "c2b2a-pending-b").await, (false, true));
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn recovery_orphan_boot_keeps_protected_dispatch_notify_fenced_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    crate::db::replay_disposition::tests::apply_test_mutant(&pool).await;
    let mut dispatches = Vec::new();
    for (n, state) in [
        "withheld",
        "started_unclassified",
        "startup_failed_no_effect",
        "legacy",
    ]
    .into_iter()
    .enumerate()
    {
        let dispatch = format!("p3-orphan-dispatch-{n}");
        let agent = format!("p3-orphan-agent-{n}");
        crate::db::agents::insert_agent_channels_for_tests(&pool, &agent, None, None).await;
        card_and_dispatch(&pool, &agent, &dispatch, "pending").await;
        if state != "legacy" {
            let receipt:i64=sqlx::query_scalar("INSERT INTO intake_outbox(target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,user_text,provider,status,replay_only,replay_disposition,replay_source_message_ids,request_owner_id,turn_kind,agent_id,replay_preserved)
                VALUES('worker','leader',$1,$1,'original','claude','unknown',TRUE,'registered_not_started',ARRAY[$1],'7','standard','agent','{}'::jsonb) RETURNING id")
                .bind(format!("p3-orphan-{n}")).fetch_one(&pool).await.unwrap();
            sqlx::query("UPDATE task_dispatches SET replay_receipt_id=$2 WHERE id=$1")
                .bind(&dispatch)
                .bind(receipt)
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query(
                "UPDATE intake_outbox SET replay_disposition='started_unclassified' WHERE id=$1",
            )
            .bind(receipt)
            .execute(&pool)
            .await
            .unwrap();
            if state != "started_unclassified" {
                sqlx::query("UPDATE intake_outbox SET replay_disposition=$2 WHERE id=$1")
                    .bind(receipt)
                    .bind(state)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        }
        dispatches.push((dispatch, state));
    }
    let path = crate::cli::agentdesk_runtime_root()
        .unwrap()
        .join("runtime/dcserver.pid");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "1").unwrap();
    recover_orphan_pending_dispatches_once(Some(&pool)).await;
    for (dispatch, state) in dispatches {
        assert_eq!(
            notified(&pool, &dispatch).await.1,
            state == "legacy",
            "{state}: protected boot rearm must stay zero, healthy control one"
        );
    }
    pool.close().await;
    db.drop().await;
}

async fn owner_pool(pool: &PgPool) -> PgPool {
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

async fn owner_dispatch(pool: &PgPool, id: &str, age: i64) {
    let agent = format!("agent-{id}");
    crate::db::agents::insert_agent_channels_for_tests(pool, &agent, None, None).await;
    card_and_dispatch(pool, &agent, id, "pending").await;
    sqlx::query("UPDATE task_dispatches SET created_at = NOW() - ($2::bigint * INTERVAL '1 second') WHERE id = $1")
        .bind(id).bind(age).execute(pool).await.unwrap();
    sqlx::query("UPDATE kanban_cards SET latest_dispatch_id = $1 WHERE id = $2")
        .bind(id)
        .bind(format!("card-{id}"))
        .execute(pool)
        .await
        .unwrap();
}

async fn owner_state(pool: &PgPool, id: &str) -> (String, String, i64) {
    sqlx::query_as("SELECT d.status, c.status, (SELECT COUNT(*) FROM kv_meta k WHERE k.key = 'runtime_supervisor:orphan_confirm:' || d.id) FROM task_dispatches d JOIN kanban_cards c ON c.id = d.kanban_card_id WHERE d.id = $1")
        .bind(id).fetch_one(pool).await.unwrap()
}

fn owner_pid(at: i64) {
    let pid = crate::cli::agentdesk_runtime_root()
        .unwrap()
        .join("runtime/dcserver.pid");
    std::fs::create_dir_all(pid.parent().unwrap()).unwrap();
    std::fs::write(&pid, "1").unwrap();
    filetime::set_file_mtime(pid, filetime::FileTime::from_unix_time(at, 0)).unwrap();
}

// Execute the real scheduler, K module, DB/pipeline/runtime bridges and supervisor.
// Only unrelated sections are stubbed; the clock and root are isolated fixtures.
fn owner_tick(pool: &PgPool, now_ms: i64, watch_sql: bool) -> i32 {
    let empty = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.policies.dir = empty.path().to_path_buf();
    config.policies.hot_reload = false;
    let engine = crate::engine::PolicyEngine::new_with_pg(&config, Some(pool.clone())).unwrap();
    let bridge = crate::supervisor::BridgeHandle::new();
    bridge.attach_engine(&engine);
    let runtime = rquickjs::Runtime::new().unwrap();
    let context = rquickjs::Context::full(&runtime).unwrap();
    let read = |name| {
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(name))
            .unwrap()
    };
    let helpers = read("policies/lib/timeouts-helpers.js");
    let orphan = read("policies/timeouts/orphan-dispatch.js");
    let entry = read("policies/timeouts.js");
    context.with(|ctx| {
        crate::engine::ops::register_globals_with_supervisor_and_pg(&ctx, Some(pool.clone()), bridge).unwrap();
        let script = format!(r#"(function() {{
            Date.now = function() {{ return {now_ms}; }};
            var legacySql = 0;
            if ({watch_sql}) {{
                var query = agentdesk.db.query;
                agentdesk.db.query = function(sql, params) {{ legacySql++; return query(sql, params); }};
            }}
            var helpers = {{exports: {{}}}};
            (function(module, exports) {{ {helpers} }})(helpers, helpers.exports);
            var orphan = {{exports: {{}}}};
            (function(module) {{ {orphan} }})(orphan);
            var root = {{exports: {{}}}};
            var require = function(name) {{
                if (name === './lib/timeouts-helpers') return helpers.exports;
                if (name === './timeouts/orphan-dispatch') return orphan.exports;
                return function(t) {{}};
            }};
            (function(module, require) {{ {entry} }})(root, require);
            ['I0', 'J', 'I'].forEach(function(s) {{ root.exports.policy['_section_' + s] = function() {{}}; }});
            root.exports.policy.onTick30s({{}});
            return legacySql;
        }})()"#);
        ctx.eval::<i32, _>(script).unwrap()
    })
}

const OWNER_NOW: i64 = 1_793_491_200_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphan_owner_policy_grace_and_dispatch_identity_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let original = db.connect_and_migrate().await;
    let pool = owner_pool(&original).await;
    for (id, age) in [
        ("owner-orphan", 301),
        ("owner-young", 299),
        ("owner-exact", 300),
        ("owner-linked", 601),
        ("owner-agent-busy", 601),
    ] {
        owner_dispatch(&pool, id, age).await;
    }
    // A different agent owns this exact dispatch link: tick excludes it.
    active_session(&pool, "agent-owner-young", 7001, "owner-linked").await;
    // A same-agent busy session on another dispatch only excludes boot recovery.
    active_session(&pool, "agent-owner-agent-busy", 7002, "owner-young").await;
    sqlx::query("INSERT INTO kv_meta(key, value) VALUES ('server_boot_at', '2026-10-31 23:50:00')")
        .execute(&pool)
        .await
        .unwrap();
    owner_tick(&pool, OWNER_NOW - 1_000, false);
    assert_eq!(
        owner_state(&pool, "owner-orphan").await.2,
        0,
        "599 seconds keeps boot grace"
    );
    owner_tick(&pool, OWNER_NOW, false);
    assert_eq!(
        owner_state(&pool, "owner-orphan").await,
        ("pending".into(), "in_progress".into(), 1),
        "600 seconds emits only the first confirmation"
    );
    owner_tick(&pool, OWNER_NOW + 1_000, false);
    for id in ["owner-orphan", "owner-agent-busy"] {
        let (status, card, confirmation) = owner_state(&pool, id).await;
        assert_eq!(status, "failed", "{id}");
        assert_ne!(card, "in_progress", "card returned to dispatchable state");
        assert_eq!(
            confirmation, 0,
            "the second observation consumes confirmation, {id}"
        );
    }
    for id in ["owner-young", "owner-exact", "owner-linked"] {
        assert_eq!(
            owner_state(&pool, id).await,
            ("pending".into(), "in_progress".into(), 0),
            "{id}"
        );
    }
    owner_dispatch(&pool, "owner-pg-clock", 601).await;
    sqlx::query("UPDATE kv_meta SET value = NOW()::TEXT WHERE key = 'server_boot_at'")
        .execute(&pool)
        .await
        .unwrap();
    owner_tick(&pool, OWNER_NOW, false);
    assert_eq!(
        owner_state(&pool, "owner-pg-clock").await.2,
        0,
        "QuickJS accepts the production PostgreSQL offset plus Z and keeps grace"
    );
    owner_tick(&pool, OWNER_NOW + 600_000, false);
    assert_eq!(
        owner_state(&pool, "owner-pg-clock").await.2,
        1,
        "the production boot value expires at exactly ten minutes"
    );
    sqlx::query("UPDATE kv_meta SET value = 'not-a-timestamp' WHERE key = 'server_boot_at'")
        .execute(&pool)
        .await
        .unwrap();
    owner_tick(&pool, OWNER_NOW, false);
    assert_eq!(
        owner_state(&pool, "owner-pg-clock").await.0,
        "failed",
        "an invalid boot value retains the old no-grace behavior"
    );
    pool.close().await;
    original.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphan_owner_boot_pid_cutoff_rearm_and_agent_identity_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let boot = chrono::Utc::now().timestamp() - 600;
    for id in [
        "owner-before",
        "owner-at",
        "owner-after",
        "owner-assigned-other",
        "owner-busy",
        "owner-redelivered",
        "owner-already-dispatched",
    ] {
        owner_dispatch(&pool, id, 3_600).await;
    }
    for (id, seconds) in [("owner-before", -1), ("owner-at", 0), ("owner-after", 1)] {
        sqlx::query(
            "UPDATE task_dispatches SET created_at = to_timestamp($2::bigint) WHERE id = $1",
        )
        .bind(id)
        .bind(boot + seconds)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query("UPDATE kanban_cards SET assigned_agent_id = 'agent-owner-before' WHERE id = 'card-owner-assigned-other'")
        .execute(&pool).await.unwrap();
    active_session(&pool, "agent-owner-busy", 7003, "owner-after").await;
    sqlx::query(
        "UPDATE task_dispatches SET status = 'dispatched' WHERE id = 'owner-already-dispatched'",
    )
    .execute(&pool)
    .await
    .unwrap();
    // The worker commits done before updating the dispatch, leaving this crash window.
    for id in ["owner-redelivered", "owner-already-dispatched"] {
        sqlx::query("INSERT INTO dispatch_outbox(dispatch_id, action, agent_id, card_id, title, status, retry_count, processed_at) VALUES ($1, 'notify', $2, $3, 'old', 'done', 3, NOW())")
            .bind(id).bind(format!("agent-{id}")).bind(format!("card-{id}"))
            .execute(&pool).await.unwrap();
    }
    sqlx::query("INSERT INTO kv_meta(key,value) VALUES ('server_boot_at', '2000-01-01 00:00:00')")
        .execute(&pool)
        .await
        .unwrap();
    owner_pid(boot);
    recover_orphan_pending_dispatches_once(Some(&pool)).await;
    for id in ["owner-before", "owner-redelivered"] {
        assert_eq!(notified(&pool, id).await, (false, true), "{id}");
        assert_eq!(
            owner_state(&pool, id).await,
            ("pending".into(), "in_progress".into(), 0)
        );
    }
    for id in [
        "owner-at",
        "owner-after",
        "owner-assigned-other",
        "owner-busy",
    ] {
        assert_eq!(notified(&pool, id).await, (true, false), "{id}");
    }
    assert_eq!(
        notified(&pool, "owner-already-dispatched").await,
        (true, false)
    );
    owner_tick(&pool, OWNER_NOW, false);
    assert_eq!(
        owner_state(&pool, "owner-already-dispatched").await.0,
        "dispatched",
        "both paths exclude an already dispatched delivery"
    );
    let reset: (i64, bool) = sqlx::query_as("SELECT retry_count, processed_at IS NULL FROM dispatch_outbox WHERE dispatch_id = 'owner-redelivered'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(reset, (0, true));
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphan_owner_boot_and_tick_overlap_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    owner_dispatch(&pool, "owner-overlap", 3_600).await;
    owner_pid(chrono::Utc::now().timestamp());
    owner_tick(&pool, OWNER_NOW, false);
    assert_eq!(owner_state(&pool, "owner-overlap").await.2, 1);
    // Boot re-arms delivery without completing or clearing the policy confirmation.
    recover_orphan_pending_dispatches_once(Some(&pool)).await;
    assert_eq!(notified(&pool, "owner-overlap").await, (false, true));
    owner_tick(&pool, OWNER_NOW, false);
    assert_eq!(owner_state(&pool, "owner-overlap").await.0, "failed");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dispatch_outbox WHERE dispatch_id = 'owner-overlap' AND action = 'notify'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(
        count, 1,
        "boot and tick retain existing effects without creating a second notify row"
    );
    assert!(
        crate::db::dispatches::outbox::dispatch_notify_delivery_suppressed_pg(
            &pool,
            "owner-overlap",
            "notify"
        )
        .await
        .unwrap()
        .is_some(),
        "the real worker gate suppresses the terminal dispatch's queued notify"
    );
    recover_orphan_pending_dispatches_once(Some(&pool)).await;
    assert_eq!(owner_state(&pool, "owner-overlap").await.2, 0);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphan_owner_policy_has_no_js_decisions_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    owner_dispatch(&pool, "owner-boundary", 3_600).await;
    assert_eq!(
        owner_tick(&pool, OWNER_NOW, true),
        0,
        "K delegates candidate selection and timing to its Rust owner"
    );
    assert_eq!(
        owner_state(&pool, "owner-boundary").await.2,
        1,
        "actual policy invocation reached the supervisor"
    );
    pool.close().await;
    db.drop().await;
}

fn owner_shared(pool: &PgPool) -> std::sync::Arc<crate::services::discord::SharedData> {
    crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphan_owner_actual_boot_once_and_concurrent_tick_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    owner_dispatch(&pool, "owner-race", 3_600).await;
    owner_pid(chrono::Utc::now().timestamp());
    owner_tick(&pool, OWNER_NOW, false);
    assert_eq!(owner_state(&pool, "owner-race").await.2, 1);
    let shared = owner_shared(&pool);
    // Pause after boot's real candidate snapshot and before its marker deletion.
    sqlx::raw_sql("CREATE FUNCTION owner_pause_boot() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF OLD.key = 'dispatch_notified:owner-race' THEN PERFORM pg_advisory_xact_lock(7349, 9); END IF; RETURN OLD; END $$; CREATE TRIGGER owner_pause_boot BEFORE DELETE ON kv_meta FOR EACH ROW EXECUTE FUNCTION owner_pause_boot();")
        .execute(&pool).await.unwrap();
    let mut lock = pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock(7349, 9)")
        .execute(&mut *lock)
        .await
        .unwrap();
    let boot_shared = shared.clone();
    let boot = tokio::spawn(async move {
        super::recover_orphan_pending_dispatches(&boot_shared).await;
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND wait_event = 'advisory')")
            .fetch_one(&pool).await.unwrap();
        if blocked {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the actual boot entrypoint must reach its orphan effect"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    owner_tick(&pool, OWNER_NOW, false);
    assert_eq!(owner_state(&pool, "owner-race").await.0, "failed");
    sqlx::query("SELECT pg_advisory_unlock(7349, 9)")
        .execute(&mut *lock)
        .await
        .unwrap();
    drop(lock);
    boot.await.unwrap();
    assert_eq!(
        notified(&pool, "owner-race").await,
        (false, false),
        "boot rechecks terminal after the overlapping tick failed it"
    );
    owner_dispatch(&pool, "owner-once-second", 3_600).await;
    super::recover_orphan_pending_dispatches(&shared).await;
    assert_eq!(
        notified(&pool, "owner-once-second").await,
        (true, false),
        "the process boot guard still runs only once"
    );
    drop(shared);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphan_owner_actor_hook_flushes_deferred_confirmation_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    owner_dispatch(&pool, "owner-actor", 3_600).await;
    let dir = tempfile::tempdir().unwrap();
    for folder in ["lib", "timeouts"] {
        std::fs::create_dir(dir.path().join(folder)).unwrap();
    }
    for relative in [
        "timeouts.js",
        "lib/timeouts-helpers.js",
        "timeouts/orphan-dispatch.js",
    ] {
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("policies")
                .join(relative),
            dir.path().join(relative),
        )
        .unwrap();
    }
    for section in [
        "card-timeouts",
        "review-timeouts",
        "review-auto-accept",
        "dispatch-maintenance",
        "active-monitor",
        "idle-kill",
    ] {
        std::fs::write(dir.path().join(format!("timeouts/{section}.js")), "module.exports = function(t) { ['I0', 'J', 'I'].forEach(function(s) { t['_section_' + s] = function() {}; }); };").unwrap();
    }
    let mut config = crate::config::Config::default();
    config.policies.dir = dir.path().to_path_buf();
    config.policies.hot_reload = false;
    let engine = crate::engine::PolicyEngine::new_for_tick(&config, Some(pool.clone())).unwrap();
    let hook = crate::engine::hooks::Hook::OnTick30s;
    engine.fire_hook(hook, serde_json::json!({})).unwrap();
    assert_eq!(
        owner_state(&pool, "owner-actor").await,
        ("pending".into(), "in_progress".into(), 1),
        "the real actor flushes the first deferred signal"
    );
    engine.fire_hook(hook, serde_json::json!({})).unwrap();
    let (status, card, confirmation) = owner_state(&pool, "owner-actor").await;
    assert_eq!(status, "failed");
    assert_ne!(card, "in_progress");
    assert_eq!(
        confirmation, 0,
        "the second actor hook consumes confirmation and rolls back"
    );
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM runtime_decisions WHERE dispatch_id = 'owner-actor'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        audits, 0,
        "retain the existing JSONB audit binding failure after the effects"
    );
    drop(engine);
    pool.close().await;
    db.drop().await;
}
