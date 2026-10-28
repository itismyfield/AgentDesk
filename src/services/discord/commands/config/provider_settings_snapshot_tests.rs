use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};
use crate::db::auto_queue::test_support::TestPostgresDb;

#[test]
fn settings_owner_uses_installed_lease_ttl_pg() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "cluster: {lease_ttl_secs: 2}",
            "cluster: {lease_ttl_secs: 9}",
        );
        // The single-test child sets identity before starting its async runtime.
        unsafe {
            std::env::set_var("AGENTDESK_INSTANCE_ID", "snapshot-settings-local");
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let db = TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            sqlx::query("INSERT INTO worker_nodes (instance_id, status, capabilities, last_heartbeat_at) VALUES ('snapshot-settings-remote', 'online', $1, NOW() - INTERVAL '5 seconds')")
                .bind(serde_json::json!({"intake_worker": {"enabled": true, "providers": ["codex"], "features": ["preserve_on_cancel_v1"]}}))
                .execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO sessions (session_key, provider, channel_id, instance_id, status) VALUES ('snapshot-settings-session', 'codex', '734921', 'snapshot-settings-remote', 'idle')")
                .execute(&pool).await.unwrap();
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
            let channel = serenity::ChannelId::new(734921);
            let before = shared.check_settings_owner(&ProviderKind::Codex, channel).await.unwrap_err().to_string();
            assert!(before.contains("정리되지 않은 소유 기록"), "{before}");
            assert!(before.contains("snapshot-settings-remote"), "{before}");
            fixture.reload();
            sqlx::query("UPDATE worker_nodes SET last_heartbeat_at = NOW() - INTERVAL '5 seconds' WHERE instance_id = 'snapshot-settings-remote'")
                .execute(&pool).await.unwrap();
            let after = shared.check_settings_owner(&ProviderKind::Codex, channel).await.unwrap_err().to_string();
            assert!(after.contains("노드 `snapshot-settings-remote` 소유 채널"), "{after}");
            assert!(!after.contains("정리되지 않은 소유 기록"), "{after}");
            pool.close().await;
            db.drop().await;
        });
    });
}
