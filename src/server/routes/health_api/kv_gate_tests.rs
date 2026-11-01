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

/// A persisted danger override of 77: a stalled or failed kv read shows no fallback setting or
/// source as effective and keeps the counters; a successful read shows its value and layer.
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
    for setting in [
        "enabled",
        "enabled_source",
        "danger_pct",
        "danger_pct_source",
    ] {
        assert!(
            gate.get(setting).is_none(),
            "fallback {setting} shown as effective: {gate}"
        );
    }
    assert!(gate["evaluations"].is_u64(), "{gate}");

    proxy.release();
    let read = detail_body(&app).await;
    assert_eq!(read["rate_limit_dispatch_gate"]["danger_pct"], 77, "{read}");
    assert_eq!(
        read["rate_limit_dispatch_gate"]["danger_pct_source"], "kv_override",
        "{read}"
    );

    // A successful read without the row is no override: the fallback is shown with its layer.
    sqlx::query("DELETE FROM kv_meta WHERE key = 'runtime-config'")
        .execute(&seed)
        .await
        .unwrap();
    let absent = detail_body(&app).await;
    let gate = &absent["rate_limit_dispatch_gate"];
    for source in ["enabled_source", "danger_pct_source"] {
        assert!(
            gate[source] == "yaml" || gate[source] == "default",
            "{source} after a read without the row: {gate}"
        );
    }

    // A read that fails at once leaves the override unknown, exactly like a cut read.
    pool.close().await;
    let failed = detail_body(&app).await;
    let gate = &failed["rate_limit_dispatch_gate"];
    for setting in [
        "enabled",
        "enabled_source",
        "danger_pct",
        "danger_pct_source",
    ] {
        assert!(
            gate.get(setting).is_none(),
            "fallback {setting} shown after a failed read: {gate}"
        );
    }
    assert!(gate["evaluations"].is_u64(), "{gate}");

    pool.close().await;
    seed.close().await;
    drop(proxy);
    db.drop().await;
}
