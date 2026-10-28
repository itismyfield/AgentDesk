use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};
use crate::db::auto_queue::test_support::TestPostgresDb;

fn database_yaml(database_url: &str) -> String {
    let url = url::Url::parse(database_url).unwrap();
    serde_yaml::to_string(&serde_json::json!({"database": {
        "enabled": true, "host": url.host_str().unwrap(), "port": url.port().unwrap_or(5432),
        "dbname": url.path().trim_start_matches('/'), "user": url.username(),
        "password": url.password(), "pool_max": 1, "foreground_reserve": 0
    }}))
    .unwrap()
}

#[test]
fn runtime_completion_uses_installed_database_pg() {
    isolated(|| {
        let fixture = Fixture::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let db = rt.block_on(TestPostgresDb::create());
        let expected = url::Url::parse(&db.database_url)
            .unwrap()
            .path()
            .trim_start_matches('/')
            .to_owned();
        fixture.install(
            &database_yaml(&db.database_url),
            "database: {enabled: false}",
        );
        let actual = with_runtime_postgres_result(|pool| {
            Box::pin(async move {
                let result = sqlx::query_scalar::<_, String>("SELECT current_database()")
                    .fetch_one(&pool)
                    .await
                    .map_err(|error| error.to_string());
                pool.close().await;
                result
            })
        })
        .unwrap();
        assert_eq!(actual, expected);
        fixture.reload();
        let error = with_runtime_postgres_result(|_| Box::pin(async { Ok(()) })).unwrap_err();
        assert_eq!(error, "postgres is not configured");
        rt.block_on(db.drop());
    });
}

#[test]
fn runtime_completion_without_snapshot_preserves_load_error_prefix() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.write("server: {}");
        std::fs::write(&fixture.path, "agents: [\n").unwrap();
        let error = with_runtime_postgres_result(|_| Box::pin(async { Ok(()) })).unwrap_err();
        assert!(
            error.starts_with("load runtime config: Failed to parse config:"),
            "{error}"
        );
    });
}

#[test]
fn runtime_retry_limit_uses_snapshot_then_persisted_policy_pg() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "runtime: {max_entry_retries: 2}",
            "runtime: {max_entry_retries: 9}",
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let db = TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            let mut tx = pool.begin().await.unwrap();
            assert_eq!(runtime_max_entry_retries_pg(&mut tx).await.unwrap(), 2);
            for (persisted, expected) in [(5, 5), (0, 1), (u64::MAX, i64::MAX)] {
                sqlx::query("INSERT INTO kv_meta (key, value) VALUES ('runtime-config', $1) ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value")
                    .bind(serde_json::json!({"maxEntryRetries": persisted}).to_string())
                    .execute(&mut *tx).await.unwrap();
                assert_eq!(runtime_max_entry_retries_pg(&mut tx).await.unwrap(), expected);
            }
            sqlx::query("DELETE FROM kv_meta WHERE key = 'runtime-config'").execute(&mut *tx).await.unwrap();
            fixture.reload();
            assert_eq!(runtime_max_entry_retries_pg(&mut tx).await.unwrap(), 9);
            fixture.install("runtime: {}", "runtime: {max_entry_retries: 9}");
            assert_eq!(runtime_max_entry_retries_pg(&mut tx).await.unwrap(), 3);
            fixture.install("runtime: {max_entry_retries: 0}", "runtime: {max_entry_retries: 9}");
            assert_eq!(runtime_max_entry_retries_pg(&mut tx).await.unwrap(), 1);
            fixture.install(&format!("runtime: {{max_entry_retries: {}}}", u64::MAX), "runtime: {max_entry_retries: 9}");
            assert_eq!(runtime_max_entry_retries_pg(&mut tx).await.unwrap(), i64::MAX);
            tx.rollback().await.unwrap();
            pool.close().await;
            db.drop().await;
        });
    });
}
