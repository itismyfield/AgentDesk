use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};
use crate::db::auto_queue::test_support::TestPostgresDb;

#[test]
fn node_choices_use_installed_lease_ttl_pg() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "cluster: {lease_ttl_secs: 2}",
            "cluster: {lease_ttl_secs: 9}",
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let db = TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            sqlx::query("INSERT INTO worker_nodes (instance_id, status, capabilities, last_heartbeat_at) VALUES ('snapshot-ttl-worker', 'online', $1, NOW() - INTERVAL '5 seconds')")
                .bind(serde_json::json!({"intake_worker": {"enabled": true, "providers": ["claude"]}}))
                .execute(&pool).await.unwrap();
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
            let before = list_online_node_choices(&shared, &ProviderKind::Claude).await.unwrap();
            assert!(!before.iter().any(|node| node.instance_id == "snapshot-ttl-worker"));
            fixture.reload();
            sqlx::query("UPDATE worker_nodes SET last_heartbeat_at = NOW() - INTERVAL '5 seconds' WHERE instance_id = 'snapshot-ttl-worker'")
                .execute(&pool).await.unwrap();
            let after = list_online_node_choices(&shared, &ProviderKind::Claude).await.unwrap();
            assert!(after.iter().any(|node| node.instance_id == "snapshot-ttl-worker"));
            pool.close().await;
            db.drop().await;
        });
    });
}
