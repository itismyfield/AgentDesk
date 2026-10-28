use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};
use crate::db::auto_queue::test_support::TestPostgresDb;

#[test]
fn runtime_recovery_take_uses_installed_database_pg() {
    isolated(|| {
        let fixture = Fixture::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let db = rt.block_on(TestPostgresDb::create());
        let pool = rt.block_on(db.connect_and_migrate());
        rt.block_on(sqlx::query("INSERT INTO kv_meta (key, value) VALUES ('snapshot-recovery', '  snapshot recovery value  ')").execute(&pool)).unwrap();
        let url = url::Url::parse(&db.database_url).unwrap();
        let body = serde_yaml::to_string(&serde_json::json!({"database": {
            "enabled": true, "host": url.host_str().unwrap(), "port": url.port().unwrap_or(5432),
            "dbname": url.path().trim_start_matches('/'), "user": url.username(),
            "password": url.password(), "pool_max": 1, "foreground_reserve": 0
        }}))
        .unwrap();
        fixture.install(&body, "database: {enabled: false}");
        assert_eq!(
            take_kv_meta_value_runtime_pg("snapshot-recovery").as_deref(),
            Some("snapshot recovery value")
        );
        assert_eq!(take_kv_meta_value_runtime_pg("snapshot-recovery"), None);
        rt.block_on(async {
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM kv_meta WHERE key = 'snapshot-recovery'")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(count, 0);
            pool.close().await;
            db.drop().await;
        });
    });
}
