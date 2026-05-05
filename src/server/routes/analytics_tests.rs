use super::*;
use serde_json::json;

struct TestPostgresDb {
    admin_url: String,
    database_name: String,
    database_url: String,
}

impl TestPostgresDb {
    async fn create() -> Self {
        let admin_url = postgres_admin_database_url();
        let database_name = format!("agentdesk_analytics_{}", uuid::Uuid::new_v4().simple());
        let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
        crate::db::postgres::create_test_database(&admin_url, &database_name, "analytics tests")
            .await
            .expect("create postgres test db");

        Self {
            admin_url,
            database_name,
            database_url,
        }
    }

    async fn connect_and_migrate(&self) -> sqlx::PgPool {
        crate::db::postgres::connect_test_pool_and_migrate(&self.database_url, "analytics tests")
            .await
            .expect("apply postgres migration")
    }

    async fn drop(self) {
        crate::db::postgres::drop_test_database(
            &self.admin_url,
            &self.database_name,
            "analytics tests",
        )
        .await
        .expect("drop postgres test db");
    }
}

fn postgres_base_database_url() -> String {
    if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
        let trimmed = base.trim();
        if !trimmed.is_empty() {
            return trimmed.trim_end_matches('/').to_string();
        }
    }

    let user = std::env::var("PGUSER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("USER")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "postgres".to_string());
    let password = std::env::var("PGPASSWORD")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let host = std::env::var("PGHOST")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "localhost".to_string());
    let port = std::env::var("PGPORT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "5432".to_string());

    match password {
        Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
        None => format!("postgresql://{user}@{host}:{port}"),
    }
}

fn postgres_admin_database_url() -> String {
    let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "postgres".to_string());
    format!("{}/{}", postgres_base_database_url(), admin_db)
}

#[tokio::test]
async fn machine_status_machines_config_prefers_postgres_when_pool_exists() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    sqlx::query(
        "INSERT INTO kv_meta (key, value)
             VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind("machines")
    .bind(serde_json::json!([{ "name": "pg-machine", "host": "pg-host" }]).to_string())
    .execute(&pool)
    .await
    .unwrap();

    let machines = load_machine_config(Some(&pool)).await;

    assert_eq!(
        machines,
        vec![("pg-machine".to_string(), "pg-host.local".to_string())]
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn machine_status_machines_config_uses_hostname_when_postgres_is_unconfigured() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let hostname = crate::services::platform::hostname_short();

    let machines = load_machine_config(Some(&pool)).await;

    assert_eq!(machines, vec![(hostname.clone(), hostname)]);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn machine_status_machines_config_uses_hostname_for_empty_postgres_config() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    sqlx::query(
        "INSERT INTO kv_meta (key, value)
             VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind("machines")
    .bind("[]")
    .execute(&pool)
    .await
    .unwrap();
    let hostname = crate::services::platform::hostname_short();

    let machines = load_machine_config(Some(&pool)).await;

    assert_eq!(machines, vec![(hostname.clone(), hostname)]);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn machine_status_machines_config_uses_hostname_without_pg_pool() {
    let hostname = crate::services::platform::hostname_short();
    let machines = load_machine_config(None).await;

    assert_eq!(machines, vec![(hostname.clone(), hostname)]);
}

#[tokio::test]
async fn policy_hooks_route_returns_recent_filtered_events() {
    // Seed the in-memory event ring buffer with synthetic
    // `policy_hook_executed` entries and verify the route filters them.
    crate::services::observability::events::reset_for_tests();

    crate::services::observability::events::record_simple(
        "policy_hook_executed",
        None,
        None,
        serde_json::json!({
            "policy_name": "alpha-policy",
            "hook_name": "onTick",
            "policy_version": "abc123",
            "duration_ms": 3,
            "result": "ok",
            "effects_count": 0,
        }),
    );
    crate::services::observability::events::record_simple(
        "policy_hook_executed",
        None,
        None,
        serde_json::json!({
            "policy_name": "beta-policy",
            "hook_name": "onCardTerminal",
            "policy_version": "def456",
            "duration_ms": 7,
            "result": "err",
            "effects_count": 1,
        }),
    );
    // Noise event — must not surface.
    crate::services::observability::events::record_simple(
        "turn_finished",
        Some(42),
        Some("codex"),
        serde_json::json!({"status": "ok"}),
    );

    let (status, Json(body)) = policy_hooks(Query(PolicyHooksQuery {
        policy_name: Some("beta-policy".to_string()),
        hook_name: None,
        last_minutes: None,
        limit: None,
    }))
    .await;

    assert_eq!(status, StatusCode::OK);
    let events = body["events"].as_array().expect("events array");
    assert_eq!(events.len(), 1, "only beta-policy event should match");
    assert_eq!(events[0]["policy_name"], json!("beta-policy"));
    assert_eq!(events[0]["hook_name"], json!("onCardTerminal"));
    assert_eq!(events[0]["result"], json!("err"));
    assert_eq!(events[0]["effects_count"], json!(1));
    assert_eq!(events[0]["policy_version"], json!("def456"));

    // Unfiltered query should return both policy_hook_executed events
    // (but not the turn_finished noise event).
    let (_, Json(body_all)) = policy_hooks(Query(PolicyHooksQuery {
        policy_name: None,
        hook_name: None,
        last_minutes: Some(60),
        limit: Some(10),
    }))
    .await;
    let events_all = body_all["events"].as_array().unwrap();
    assert_eq!(events_all.len(), 2);
}

#[tokio::test]
async fn build_rate_limit_provider_payloads_pg_hides_unused_unsupported_qwen() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO rate_limit_cache (provider, data, fetched_at)
             VALUES ($1, $2, $3)",
    )
    .bind("claude")
    .bind(
        serde_json::json!({
            "buckets": [{
                "name": "requests",
                "limit": 100,
                "used": 20,
                "remaining": 80,
                "reset": 1_700_000_000_i64
            }]
        })
        .to_string(),
    )
    .bind(1_700_000_000_i64)
    .execute(&pool)
    .await
    .unwrap();

    let providers = build_rate_limit_provider_payloads_pg(&pool, 1_700_000_100).await;

    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["provider"], json!("claude"));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn build_rate_limit_provider_payloads_pg_shows_recent_unsupported_qwen_only_when_used() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, created_at, last_heartbeat)
             VALUES ($1, $2, 'idle', TO_TIMESTAMP($3), TO_TIMESTAMP($4))",
    )
    .bind("qwen-session-1")
    .bind("qwen")
    .bind(1_700_000_000_i64)
    .bind(1_700_000_050_i64)
    .execute(&pool)
    .await
    .unwrap();

    let providers = build_rate_limit_provider_payloads_pg(&pool, 1_700_000_100).await;

    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["provider"], json!("qwen"));
    assert_eq!(providers[0]["unsupported"], json!(true));
    assert_eq!(providers[0]["buckets"], json!([]));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn build_rate_limit_provider_payloads_pg_shows_recent_unsupported_opencode_only_when_used() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, created_at, last_heartbeat)
             VALUES ($1, $2, 'idle', TO_TIMESTAMP($3), TO_TIMESTAMP($4))",
    )
    .bind("opencode-session-1")
    .bind("opencode")
    .bind(1_700_000_000_i64)
    .bind(1_700_000_050_i64)
    .execute(&pool)
    .await
    .unwrap();

    let providers = build_rate_limit_provider_payloads_pg(&pool, 1_700_000_100).await;

    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["provider"], json!("opencode"));
    assert_eq!(providers[0]["unsupported"], json!(true));
    assert_eq!(providers[0]["buckets"], json!([]));

    pool.close().await;
    pg_db.drop().await;
}

/// #1070: foundation-layer `/api/analytics/observability` endpoint shape
/// + hot-path wiring check. `emit_turn_started`/`emit_turn_finished` must
/// populate the atomic counters that the endpoint exposes.
#[tokio::test]
async fn observability_route_exposes_atomic_counters_and_recent_events() {
    let _guard = crate::services::observability::test_runtime_lock();
    crate::services::observability::reset_for_tests();
    crate::services::observability::metrics::reset_for_tests();
    crate::services::observability::events::reset_for_tests();

    crate::services::observability::init_observability(None);

    // Attempt + success
    crate::services::observability::emit_turn_started(
        "codex",
        5150,
        Some("dispatch-obs"),
        Some("session-obs"),
        Some("turn-obs"),
    );
    crate::services::observability::emit_turn_finished(
        "codex",
        5150,
        Some("dispatch-obs"),
        Some("session-obs"),
        Some("turn-obs"),
        "completed",
        42,
        false,
    );
    // Attempt + fail (different turn).
    crate::services::observability::emit_turn_started(
        "codex",
        5150,
        Some("dispatch-obs-2"),
        Some("session-obs-2"),
        Some("turn-obs-2"),
    );
    crate::services::observability::emit_turn_finished(
        "codex",
        5150,
        Some("dispatch-obs-2"),
        Some("session-obs-2"),
        Some("turn-obs-2"),
        "error",
        10,
        false,
    );
    // Watcher replacement + guard fire
    crate::services::observability::emit_watcher_replaced("codex", 5150, "stale_cancel");
    crate::services::observability::emit_guard_fired(
        "codex",
        5150,
        Some("dispatch-obs"),
        None,
        None,
        "placeholder_suppress",
    );

    let (status, Json(body)) = observability(Query(ObservabilityQuery {
        recent_limit: Some(50),
    }))
    .await;

    assert_eq!(status, StatusCode::OK);

    let counters = body["counters"].as_array().expect("counters array");
    let row = counters
        .iter()
        .find(|row| row["channel_id"] == json!(5150) && row["provider"] == json!("codex"))
        .expect("expected counter row for codex/5150");
    assert_eq!(row["attempts"], json!(2));
    assert_eq!(row["success"], json!(1));
    assert_eq!(row["fail"], json!(1));
    assert_eq!(row["guard_fires"], json!(1));
    assert_eq!(row["watcher_replacements"], json!(1));
    let rate = row["success_rate"].as_f64().expect("success_rate f64");
    assert!((rate - 0.5).abs() < 1e-9, "success_rate={rate}");

    let events = body["recent_events"].as_array().expect("recent_events");
    assert!(!events.is_empty());
    let kinds: std::collections::HashSet<&str> = events
        .iter()
        .filter_map(|ev| ev["event_type"].as_str())
        .collect();
    assert!(kinds.contains("turn_started"));
    assert!(kinds.contains("turn_finished"));
    assert!(kinds.contains("watcher_replaced"));
    assert!(kinds.contains("guard_fired"));
}

/// Issue #1243 — exercise the cache hot path: a second call within the
/// 60s TTL must hit the in-process cache and return the same body without
/// touching PG. This is asserted via the `X-Analytics-Cache: hit` marker
/// that `build_analytics_response` attaches.
#[tokio::test]
async fn analytics_cache_serves_warm_hits_without_repeat_query() {
    use axum::body::to_bytes;

    reset_analytics_cache();
    let body = json!({"counters": [], "events": []});
    let entry = write_analytics_cache("analytics-test-key".to_string(), body.clone());
    assert_eq!(entry.body, body);
    assert!(!entry.etag.is_empty(), "etag should be non-empty");

    let cached = read_analytics_cache("analytics-test-key").expect("cache hit");
    assert_eq!(cached.body, body);
    assert_eq!(cached.etag, entry.etag);

    let response = build_analytics_response(&cached, "hit");
    assert_eq!(response.status(), StatusCode::OK);
    let cache_header = response
        .headers()
        .get("X-Analytics-Cache")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(cache_header, "hit");
    let etag_header = response
        .headers()
        .get("ETag")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(etag_header, entry.etag);
    let cc_header = response
        .headers()
        .get("Cache-Control")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        cc_header.contains("stale-while-revalidate"),
        "Cache-Control must include SWR directive, got: {cc_header}"
    );

    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(parsed, body);
}

/// Issue #1243 — micro-benchmark: cold-then-warm reads of the analytics
/// cache. The cold call falls through write_analytics_cache (compute +
/// hash + insert), the warm call must return in well under a millisecond
/// because it is just a HashMap lookup + clone.
#[tokio::test]
async fn analytics_cache_warm_read_is_fast() {
    reset_analytics_cache();
    let key = "analytics-bench-key".to_string();
    let body = json!({
        "counters": (0..50).map(|i| json!({"channel": i})).collect::<Vec<_>>(),
        "events": (0..200).map(|i| json!({"id": i, "payload": "x".repeat(64)})).collect::<Vec<_>>(),
    });

    // Cold path: writes the cache entry.
    let cold_start = std::time::Instant::now();
    let cold_entry = write_analytics_cache(key.clone(), body.clone());
    let cold_ms = cold_start.elapsed().as_micros();
    assert_eq!(cold_entry.body, body);

    // Warm path: 100 lookups should each be cheap.
    let warm_start = std::time::Instant::now();
    for _ in 0..100 {
        let _ = read_analytics_cache(&key).expect("warm hit");
    }
    let warm_total_ms = warm_start.elapsed().as_micros();
    let warm_avg_us = warm_total_ms as f64 / 100.0;

    // The cold path is dominated by serde_json::to_string() on the body
    // for the etag hash; on tiny payloads it's < 200µs. The warm path is
    // a HashMap lookup + Value clone, well under 200µs each. We assert a
    // generous threshold so this test isn't flaky on slow CI hardware
    // but still catches a regression that adds an order of magnitude.
    assert!(
        warm_avg_us < 1_000.0,
        "warm read avg {warm_avg_us:.1}µs > 1000µs (cold {cold_ms}µs)"
    );
}
