use super::{apply_kv_seed_actions, startup_reseed, startup_reseed_with_warmup_pool};
use crate::config::{ClusterRole, Config};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::settings::KvSeedAction;
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::time::Duration;

async fn configuration_snapshot(pool: &PgPool) -> BTreeMap<&'static str, serde_json::Value> {
    let mut snapshot = BTreeMap::new();
    for (table, query) in [
        (
            "kv_meta",
            "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM kv_meta t",
        ),
        (
            "agents",
            "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM agents t",
        ),
        (
            "github_repos",
            "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM github_repos t",
        ),
        (
            "pipeline_stages",
            "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM pipeline_stages t",
        ),
    ] {
        let rows = sqlx::query_scalar(query)
            .fetch_one(pool)
            .await
            .expect("snapshot shared configuration");
        snapshot.insert(table, rows);
    }
    snapshot
}

async fn kv_value(pool: &PgPool, key: &str) -> Option<String> {
    sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await
        .expect("read fixture KV value")
}

#[tokio::test]
async fn runner_missing_shared_keys_matrix_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let mut config = Config::default();
    config.cluster.enabled = true;
    config.server.port = 41001;
    config.runtime.reset_overrides_on_restart = true;
    config.runtime.max_retries = Some(9);
    config.github.repos = vec!["fixture/runner-must-not-register".to_string()];
    config.agents.clear();
    let expected_error = "shared configuration is not initialized; start a node configured as \
        cluster.role=hub before starting runner/auto nodes (missing server_port \
        or runtime-config); this node will not seed the shared database";

    for mask in 0..4 {
        sqlx::query("DELETE FROM kv_meta WHERE key IN ('server_port', 'runtime-config')")
            .execute(&pool)
            .await
            .expect("reset shared-key fixture");
        for (bit, key, value) in [(1, "server_port", "31001"), (2, "runtime-config", "{}")] {
            if mask & bit != 0 {
                sqlx::query("INSERT INTO kv_meta (key, value) VALUES ($1, $2)")
                    .bind(key)
                    .bind(value)
                    .execute(&pool)
                    .await
                    .expect("install shared-key fixture");
            }
        }
        let before = configuration_snapshot(&pool).await;
        for role in [ClusterRole::Runner, ClusterRole::Auto] {
            config.cluster.role = role;
            let result = startup_reseed(&pool, &config).await;
            if mask == 3 {
                result.expect("both hub-owned shared keys permit runner startup");
            } else {
                assert_eq!(
                    result.expect_err("every missing shared-key combination refuses startup"),
                    expected_error
                );
            }
            assert_eq!(
                configuration_snapshot(&pool).await,
                before,
                "runner startup must not seed or delete shared configuration; mask={mask}"
            );
        }
    }
    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn warmup_failure_uses_runtime_pool_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let mut config = Config::default();
    config.database.enabled = true;
    config.database.host = "127.0.0.1".to_string();
    config.database.port = 0;
    config.database.dbname = "startup_warmup_connection_must_fail".to_string();
    config.cluster.enabled = true;
    config.cluster.role = ClusterRole::Hub;
    config.server.port = 41002;
    config.github.repos = vec!["fixture/runtime-fallback".to_string()];
    config.agents.clear();

    tokio::time::timeout(
        Duration::from_secs(30),
        startup_reseed_with_warmup_pool(&pool, &config),
    )
    .await
    .expect("warmup failure and fallback remain bounded")
    .expect("failed warmup connection must use the supplied runtime pool");

    assert_eq!(
        kv_value(&pool, "server_port").await.as_deref(),
        Some("41002")
    );
    assert!(kv_value(&pool, "runtime-config").await.is_some());
    let registered: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM github_repos WHERE id = $1)")
            .bind("fixture/runtime-fallback")
            .fetch_one(&pool)
            .await
            .expect("runtime fallback registers the configured repository");
    assert!(registered);
    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn seed_failure_propagates_without_warmup_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    pool.close().await;
    let mut config = Config::default();
    config.database.enabled = false;

    let error = startup_reseed_with_warmup_pool(&pool, &config)
        .await
        .expect_err("a seed failure must not be converted into successful startup");
    assert!(error.contains("kv_meta"), "{error}");
    assert!(
        error.ends_with(&sqlx::Error::PoolClosed.to_string()),
        "{error}"
    );
    database.drop().await;
}

#[tokio::test]
async fn seed_actions_keep_order_and_partial_writes_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    sqlx::query("INSERT INTO kv_meta (key, value) VALUES ('s401-delete', 'original')")
        .execute(&pool)
        .await
        .expect("seed delete fixture");
    sqlx::raw_sql(
        "CREATE TABLE startup_action_events (sequence BIGSERIAL, key TEXT, operation TEXT);
         CREATE FUNCTION record_startup_action() RETURNS trigger LANGUAGE plpgsql AS $$
         DECLARE affected_key TEXT;
         BEGIN
             IF TG_OP = 'DELETE' THEN affected_key := OLD.key;
             ELSE affected_key := NEW.key;
             END IF;
             IF affected_key = 's401-fail' THEN
                 RAISE EXCEPTION 'synthetic seed action failure';
             END IF;
             INSERT INTO startup_action_events (key, operation) VALUES (affected_key, TG_OP);
             RETURN NULL;
         END $$;
         CREATE TRIGGER startup_action_event AFTER INSERT OR UPDATE OR DELETE ON kv_meta
         FOR EACH ROW EXECUTE FUNCTION record_startup_action()",
    )
    .execute(&pool)
    .await
    .expect("install synthetic seed failure and ordering observer");

    let actions = [
        KvSeedAction::Put {
            key: "s401-first".to_string(),
            value: "first".to_string(),
        },
        KvSeedAction::PutIfAbsent {
            key: "s401-first".to_string(),
            value: "must-not-replace".to_string(),
        },
        KvSeedAction::Delete {
            key: "s401-delete".to_string(),
        },
        KvSeedAction::Put {
            key: "s401-fail".to_string(),
            value: "rejected".to_string(),
        },
        KvSeedAction::Put {
            key: "s401-never".to_string(),
            value: "must-not-run".to_string(),
        },
    ];
    let error = apply_kv_seed_actions(&pool, &actions)
        .await
        .expect_err("stop at the first failed action");
    assert!(error.starts_with("upsert kv_meta s401-fail:"), "{error}");
    assert!(error.contains("synthetic seed action failure"), "{error}");
    assert_eq!(
        kv_value(&pool, "s401-first").await.as_deref(),
        Some("first")
    );
    for key in ["s401-delete", "s401-fail", "s401-never"] {
        assert_eq!(kv_value(&pool, key).await, None, "key={key}");
    }
    let events: Vec<(String, String)> =
        sqlx::query_as("SELECT key, operation FROM startup_action_events ORDER BY sequence")
            .fetch_all(&pool)
            .await
            .expect("read committed action ordering");
    assert_eq!(
        events,
        vec![
            ("s401-first".to_string(), "INSERT".to_string()),
            ("s401-delete".to_string(), "DELETE".to_string()),
        ]
    );
    pool.close().await;
    database.drop().await;
}
