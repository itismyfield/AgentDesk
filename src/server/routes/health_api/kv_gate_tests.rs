//! The dispatch-gate block when the kv read that carries its persisted overrides is cut.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tower::ServiceExt;

use crate::services::discord::health::HealthRegistry;
use crate::services::health_diagnostics::pg_wire_proxy::PgWireProxy;

async fn detail_body(app: &axum::Router) -> Value {
    let mut request = axum::http::Request::builder()
        .uri("/health/detail")
        .body(axum::body::Body::empty())
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:1".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// A persisted danger override of 77 behind a stalled kv read: the body shows no fallback
/// `enabled`/`danger_pct` as the effective setting, keeps the counters, and shows 77 once read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_gate_kv_timeout_does_not_report_fallback_as_effective() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let seed = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO kv_meta (key, value) VALUES ($1, $2)")
        .bind("runtime-config")
        .bind(r#"{"dispatchRateLimitGateDangerPct":77}"#)
        .execute(&seed)
        .await
        .unwrap();
    let proxy = PgWireProxy::start(&db.database_url, Duration::ZERO).await;
    let pool = proxy.pool(8);
    let mut engine_config = crate::config::Config::default();
    engine_config.policies.hot_reload = false;
    let tx = crate::server::ws::new_broadcast();
    let app = crate::server::routes::api_router_with_pg(
        crate::engine::PolicyEngine::new(&engine_config).unwrap(),
        crate::config::Config::default(),
        tx.clone(),
        crate::server::ws::spawn_batch_flusher(tx),
        Some(Arc::new(HealthRegistry::new())),
        Some(pool.clone()),
    );

    proxy.stall("FROM kv_meta");
    let cut = detail_body(&app).await;
    assert!(
        cut["degraded_reasons"].to_string().contains("db_slow"),
        "{cut}"
    );
    let gate = &cut["rate_limit_dispatch_gate"];
    for setting in ["enabled", "danger_pct"] {
        assert!(
            gate.get(setting).is_none(),
            "fallback {setting} shown as effective: {gate}"
        );
    }
    assert!(gate["evaluations"].is_u64(), "{gate}");

    proxy.release();
    let read = detail_body(&app).await;
    assert_eq!(read["rate_limit_dispatch_gate"]["danger_pct"], 77, "{read}");

    pool.close().await;
    seed.close().await;
    drop(proxy);
    db.drop().await;
}
