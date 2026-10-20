//! Doctor against a real `/api/health/detail` body whose outbox read was cut at the deadline
//! while a mailbox actor stalled.

use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::services::discord::health::HealthRegistry;
use crate::services::health_diagnostics::pg_wire_proxy::PgWireProxy;
use tower::ServiceExt;

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

/// A cut outbox read leaves no `dispatch_outbox` block and `outbox_age` 0: doctor reports it
/// unmeasured, not an empty outbox, and reads the stalled actor's reason as a read-only warning.
#[test]
fn doctor_dispatch_outbox_missing_block_is_unmeasured() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let main = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let parked = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let registry = Arc::new(HealthRegistry::new());
    let shared = crate::services::discord::make_shared_data_for_tests();
    // A worker needs no gateway connection, so no disconnected-provider error joins the reasons.
    main.block_on(registry.register_worker("claude".to_string(), shared.clone()));
    // Nothing drives `parked`, so this actor never answers.
    parked.block_on(
        crate::services::discord::host_teardown_gate::test_support::nameless_turn(
            &shared,
            poise::serenity_prelude::ChannelId::new(6_871_380_001),
        ),
    );
    main.block_on(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let seed = db.connect_and_migrate().await;
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
            Some(registry),
            Some(pool.clone()),
        );
        let snapshot = |body| HealthSnapshot {
            base: "http://127.0.0.1:1".to_string(),
            body: Some(body),
            error: None,
        };

        proxy.stall("FROM dispatch_outbox");
        let body = detail_body(&app).await;
        assert!(body.get("dispatch_outbox").is_none(), "{body}");
        assert_eq!(body["outbox_age"], 0, "{body}");
        let cut = snapshot(body);
        let outbox = check_dispatch_outbox(&cut);
        assert_eq!(outbox.status, CheckStatus::Fail, "{}", outbox.detail);
        assert!(outbox.detail.contains("unmeasured"), "{}", outbox.detail);
        assert_eq!(outbox.fix_safety, FixSafety::NotFixable);
        let reasons = check_degraded_reasons(&cut);
        assert!(reasons.detail.contains("db_slow"), "{}", reasons.detail);
        assert!(
            reasons.detail.contains("mailbox_snapshot_unavailable"),
            "{}",
            reasons.detail
        );
        assert_eq!(reasons.status, CheckStatus::Warn, "{}", reasons.detail);
        assert_eq!(reasons.fix_safety, FixSafety::ReadOnly);

        proxy.release();
        let measured = snapshot(detail_body(&app).await);
        assert_eq!(check_dispatch_outbox(&measured).status, CheckStatus::Pass);

        pool.close().await;
        seed.close().await;
        drop(proxy);
        db.drop().await;
    });
}
