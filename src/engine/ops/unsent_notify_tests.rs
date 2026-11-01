use rquickjs::{Context, Runtime};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::db::auto_queue::test_support::TestPostgresDb;

async fn setup() -> (TestPostgresDb, PgPool) {
    let db = TestPostgresDb::create().await;
    let original = db.connect_and_migrate().await;
    // Put the frozen clock before pg_catalog, including on separately connected bridges.
    sqlx::query("CREATE OR REPLACE FUNCTION public.now() RETURNS timestamptz LANGUAGE SQL IMMUTABLE AS $$ SELECT '2026-11-01 00:00:00+00'::timestamptz $$")
        .execute(&original).await.unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(6)
        .connect_with(
            original
                .connect_options()
                .as_ref()
                .clone()
                .options([("search_path", "public,pg_catalog")]),
        )
        .await
        .unwrap();
    original.close().await;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT now()::text")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "2026-11-01 00:00:00+00"
    );
    (db, pool)
}

async fn seed(pool: &PgPool, id: &str, age: Option<i32>, status: &str, card: bool) {
    if card {
        sqlx::query("INSERT INTO kanban_cards (id, title) VALUES ($1, 'card title')")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO task_dispatches (id, kanban_card_id, title, status, created_at, required_capabilities) VALUES ($1, $2, 'dispatch title', $3, now() - ($4::int * INTERVAL '1 second'), '{\"gpu\":true}'::jsonb)")
        .bind(id).bind(card.then_some(id)).bind(status).bind(age)
        .execute(pool).await.unwrap();
}

async fn marker(pool: &PgPool, id: &str, kind: &str) {
    sqlx::query("INSERT INTO kv_meta (key, value, expires_at) VALUES ($1, 'proof', now() - INTERVAL '1 minute')")
        .bind(format!("dispatch_{kind}:{id}")).execute(pool).await.unwrap();
}

async fn outbox(pool: &PgPool, id: &str, action: &str, status: &str) {
    sqlx::query("INSERT INTO dispatch_outbox (dispatch_id, action, title, status) VALUES ($1, $2, 'existing title', $3)")
        .bind(id).bind(action).bind(status).execute(pool).await.unwrap();
}

fn policy_context(pool: &PgPool) -> (Runtime, Context) {
    let runtime = Runtime::new().unwrap();
    let context = Context::full(&runtime).unwrap();
    context.with(|ctx| {
        super::register_globals_with_supervisor_and_pg(
            &ctx,
            Some(pool.clone()),
            crate::supervisor::BridgeHandle::new(),
        )
        .unwrap();
    });
    crate::engine::loader::load_single_policy(
        &context,
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("policies/timeouts.js"),
    )
    .unwrap();
    context.with(|ctx| {
        ctx.eval::<(), _>(r#"
            var policy = __policyCapture.captured;
            policy._section_J = policy._section_I = policy._section_K = function() {};
            var legacyCalls = 0, warnings = [];
            var oldQuery = agentdesk.db.query, oldExecute = agentdesk.db.execute;
            agentdesk.db.query = function() { legacyCalls++; return oldQuery.apply(this, arguments); };
            agentdesk.db.execute = function() { legacyCalls++; return oldExecute.apply(this, arguments); };
            agentdesk.log.warn = function(message) { warnings.push(message); };
        "#).unwrap();
    });
    (runtime, context)
}

fn tick(pool: &PgPool) -> Value {
    let (_runtime, context) = policy_context(pool);
    context.with(|ctx| {
        ctx.eval::<(), _>("policy.onTick30s({});").unwrap();
        serde_json::from_str(
            &ctx.eval::<String, _>("JSON.stringify({legacyCalls:legacyCalls, warnings:warnings})")
                .unwrap(),
        )
        .unwrap()
    })
}

async fn notify_rows(pool: &PgPool) -> Vec<(String, String, Option<Value>)> {
    sqlx::query_as("SELECT dispatch_id, title, required_capabilities FROM dispatch_outbox WHERE action = 'notify' ORDER BY dispatch_id")
        .fetch_all(pool).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_preserves_two_minute_markers_and_all_outbox_statuses_pg() {
    let (db, pool) = setup().await;
    for (id, age, status, card) in [
        ("before", Some(119), "pending", true),
        ("equal", Some(120), "pending", true),
        ("after", Some(121), "pending", true),
        ("null-age", None, "pending", true),
        ("dispatched", Some(121), "dispatched", true),
        ("completed", Some(121), "completed", true),
        ("no-card", Some(121), "pending", false),
        ("notified", Some(121), "pending", true),
        ("reserving", Some(121), "pending", true),
    ] {
        seed(&pool, id, age, status, card).await;
    }
    marker(&pool, "notified", "notified").await;
    marker(&pool, "reserving", "reserving").await;
    for action in ["notify", "followup"] {
        for status in ["pending", "processing", "failed", "done"] {
            let id = format!("{action}-{status}");
            seed(&pool, &id, Some(121), "pending", true).await;
            outbox(&pool, &id, action, status).await;
        }
    }
    assert_eq!(tick(&pool)["warnings"], json!([]));
    let rows = notify_rows(&pool).await;
    assert_eq!(
        rows,
        vec![
            ("after".into(), "card title".into(), None),
            ("followup-done".into(), "card title".into(), None),
            ("notify-done".into(), "existing title".into(), None),
            ("notify-failed".into(), "existing title".into(), None),
            ("notify-pending".into(), "existing title".into(), None),
            ("notify-processing".into(), "existing title".into(), None),
        ]
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM dispatch_delivery_events")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM kv_meta WHERE key LIKE 'dispatch_%'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    assert_eq!(tick(&pool)["warnings"], json!([]));
    assert_eq!(notify_rows(&pool).await, rows);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_preserves_broad_backfill_and_typed_marker_recovery_pg() {
    let (db, pool) = setup().await;
    for (id, age, status, card) in [
        ("young", Some(1), "pending", true),
        ("dispatched", Some(1), "dispatched", true),
        ("no-card", None, "pending", false),
        ("notified", Some(121), "pending", true),
        ("reserving", Some(121), "pending", true),
        ("existing", Some(121), "pending", true),
        ("completed", Some(121), "completed", true),
    ] {
        seed(&pool, id, age, status, card).await;
    }
    marker(&pool, "notified", "notified").await;
    marker(&pool, "reserving", "reserving").await;
    outbox(&pool, "existing", "notify", "done").await;
    let stats = crate::reconcile::reconcile_boot_db_pg(&pool, "test-boot", None)
        .await
        .unwrap();
    assert_eq!(stats.missing_notify_outbox_backfilled, 5);
    let rows = notify_rows(&pool).await;
    assert_eq!(rows.len(), 6);
    for (id, title, capabilities) in &rows {
        if id == "existing" {
            assert_eq!((title.as_str(), capabilities), ("existing title", &None));
        } else {
            assert_eq!(
                (title.as_str(), capabilities),
                ("dispatch title", &Some(json!({"gpu":true})))
            );
        }
    }
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM dispatch_delivery_events WHERE dispatch_id = 'notified'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        "sent"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM kv_meta WHERE key = 'dispatch_notified:notified'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM kv_meta WHERE key = 'dispatch_reserving:reserving'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    assert_eq!(tick(&pool)["warnings"], json!([]));
    assert_eq!(notify_rows(&pool).await, rows);
    assert_eq!(
        crate::reconcile::reconcile_boot_db_pg(&pool, "test-restart", None)
            .await
            .unwrap()
            .missing_notify_outbox_backfilled,
        0
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_and_policy_compete_for_the_same_missing_notify_pg() {
    let (db, pool) = setup().await;
    seed(&pool, "race", Some(121), "pending", true).await;
    // Block both real entrypoints after candidate scans, inside BEFORE INSERT.
    sqlx::raw_sql("CREATE FUNCTION hold_notify_insert() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock_shared(734910); RETURN NEW; END $$; CREATE TRIGGER hold_notify BEFORE INSERT ON dispatch_outbox FOR EACH ROW EXECUTE FUNCTION hold_notify_insert();")
        .execute(&pool).await.unwrap();
    let mut barrier = pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock(734910)")
        .execute(&mut *barrier)
        .await
        .unwrap();
    let tick_pool = pool.clone();
    let policy_task = tokio::task::spawn_blocking(move || tick(&tick_pool));
    let boot_pool = pool.clone();
    let boot_task = tokio::spawn(async move {
        crate::reconcile::reconcile_boot_db_pg(&boot_pool, "race-boot", None)
            .await
            .unwrap()
    });
    let both_waiting = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND objid = 734910 AND NOT granted AND database = (SELECT oid FROM pg_database WHERE datname = current_database())")
                .fetch_one(&pool).await.unwrap();
            if count == 2 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await;
    sqlx::query("SELECT pg_advisory_unlock(734910)")
        .execute(&mut *barrier)
        .await
        .unwrap();
    drop(barrier);
    both_waiting.expect("both production entrypoints must reach INSERT");
    assert_eq!(policy_task.await.unwrap()["warnings"], json!([]));
    assert!(boot_task.await.unwrap().missing_notify_outbox_backfilled <= 1);
    let rows = notify_rows(&pool).await;
    assert_eq!(rows.len(), 1);
    assert!(matches!(
        (rows[0].1.as_str(), &rows[0].2),
        ("card title", None) | ("dispatch title", Some(_))
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM dispatch_delivery_events")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(tick(&pool)["warnings"], json!([]));
    assert_eq!(notify_rows(&pool).await, rows);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_insert_error_keeps_prior_autocommit_and_retries_next_tick_pg() {
    let (db, pool) = setup().await;
    for id in ["first", "second", "third"] {
        seed(&pool, id, Some(121), "pending", true).await;
    }
    sqlx::raw_sql("CREATE SEQUENCE notify_attempt; CREATE FUNCTION fail_second_notify() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF nextval('notify_attempt') = 2 THEN RAISE EXCEPTION 'injected second insert failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_second BEFORE INSERT ON dispatch_outbox FOR EACH ROW EXECUTE FUNCTION fail_second_notify();")
        .execute(&pool).await.unwrap();
    let result = tick(&pool);
    assert_eq!(result["warnings"].as_array().unwrap().len(), 1);
    assert!(
        result["warnings"][0]
            .as_str()
            .unwrap()
            .contains("injected second insert failure")
    );
    assert_eq!(notify_rows(&pool).await.len(), 1);
    assert_eq!(tick(&pool)["warnings"], json!([]));
    assert_eq!(notify_rows(&pool).await.len(), 3);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_delegates_unsent_decisions_and_effects_to_native_owner_pg() {
    let (db, pool) = setup().await;
    seed(&pool, "owner", Some(121), "pending", true).await;
    let (_runtime, context) = policy_context(&pool);
    let trace = context.with(|ctx| {
        ctx.eval::<(), _>(r#"
            var nativeCalls = [];
            agentdesk.timeouts.maintainUnsentNotifications = function() { nativeCalls.push(arguments.length); };
            policy.onTick30s({});
        "#).unwrap();
        serde_json::from_str::<Value>(&ctx.eval::<String, _>(
            "JSON.stringify({nativeCalls:nativeCalls, legacyCalls:legacyCalls, warnings:warnings})",
        ).unwrap()).unwrap()
    });
    assert_eq!(
        trace,
        json!({"nativeCalls":[0],"legacyCalls":0,"warnings":[]})
    );
    assert!(
        notify_rows(&pool).await.is_empty(),
        "policy must not INSERT behind the owner's back"
    );
    pool.close().await;
    db.drop().await;
}

#[test]
fn auto_queue_thresholds_belong_to_notify_owner() {
    let root = include_str!("../../reconcile.rs");
    for threshold in [
        "AUTO_QUEUE_PENDING_DELIVERY_ORPHAN_STALE_CLAIM",
        "AUTO_QUEUE_PENDING_DELIVERY_ORPHAN_BATCH_LIMIT",
    ] {
        assert!(
            !root.contains(threshold),
            "owner-only threshold in reconcile root: {threshold}"
        );
    }
}
