use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

#[test]
fn snapshot_order_diagnostic_uses_published_manager_channel_pg() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "server: {}\nkanban: {manager_channel_id: '41'}",
            "server: {}\nkanban: {manager_channel_id: '42'}",
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (_, values) = crate::config::snapshot_reader_tests::capture_identity(|| {
            rt.block_on(async {
                let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
                let pool = db.connect_and_migrate().await;
                let config = crate::config::runtime_config();
                std::fs::create_dir_all(&config.policies.dir).unwrap();
                let engine = crate::engine::PolicyEngine::new(&config).unwrap();
                let broadcast_tx = crate::eventbus::new_broadcast();
                let batch_buffer = crate::eventbus::spawn_batch_flusher(broadcast_tx.clone());
                let state = AppState {
                    pg_pool: Some(pool.clone()),
                    engine,
                    config: std::sync::Arc::new(config),
                    broadcast_tx,
                    batch_buffer,
                    health_registry: None,
                    cluster_instance_id: None,
                };
                let mut headers = HeaderMap::new();
                headers.insert("x-channel-id", "41".parse().unwrap());
                let body = OrderBody {
                    order: Vec::new(),
                    rationale: None,
                    reasoning: None,
                };
                assert!(
                    submit_order_with_pg(
                        &state,
                        "snapshot-absent-run",
                        &headers,
                        None,
                        &body,
                        &pool
                    )
                    .await
                    .is_err()
                );
                drop(state);
                pool.close().await;
                db.drop().await;
            })
        });
        assert_eq!(values, vec![true]);
    });
}
