use rquickjs::{Ctx, Function, Object, Result as JsResult};
use sqlx::PgPool;

// ── Config ops ───────────────────────────────────────────────────

const RUNTIME_CONFIG_BLOB_KEY: &str = "runtime-config";

pub(super) fn register_config_ops<'js>(ctx: &Ctx<'js>, pg_pool: Option<PgPool>) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let config_obj = Object::new(ctx.clone())?;

    // __config_get_raw(key) → serialized JSON consumed by the JS wrapper below.
    let pg_c = pg_pool;
    config_obj.set(
        "__get_raw",
        Function::new(
            ctx.clone(),
            rquickjs::function::MutFn::from(move |key: String| -> String {
                if let Some(pool) = pg_c.as_ref() {
                    return config_get_raw_pg(pool, &key);
                }
                "null".to_string()
            }),
        )?,
    )?;

    ad.set("config", config_obj)?;

    // JS wrapper
    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            agentdesk.config.get = function(key) {
                return JSON.parse(agentdesk.config.__get_raw(key));
            };
        })();
        undefined;
    "#,
    )?;

    Ok(())
}

fn config_get_raw_pg(pool: &PgPool, key: &str) -> String {
    if key == RUNTIME_CONFIG_BLOB_KEY {
        return "null".to_string();
    }

    let key = key.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let scalar =
                sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1")
                    .bind(&key)
                    .fetch_optional(&bridge_pool)
                    .await
                    .map_err(|error| format!("load postgres kv_meta {key}: {error}"))?;

            let runtime_config = if should_load_runtime_config(&key, scalar.as_deref()) {
                sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1")
                    .bind(RUNTIME_CONFIG_BLOB_KEY)
                    .fetch_optional(&bridge_pool)
                    .await
                    .map_err(|error| format!("load postgres runtime-config for {key}: {error}"))?
            } else {
                None
            };

            Ok(resolve_config_get_raw(
                &key,
                scalar.as_deref(),
                runtime_config.as_deref(),
            ))
        },
        |_error| "null".to_string(),
    )
    .unwrap_or_else(|_error| "null".to_string())
}

fn should_load_runtime_config(key: &str, scalar: Option<&str>) -> bool {
    key != RUNTIME_CONFIG_BLOB_KEY
        && scalar.is_none()
        && crate::services::settings::is_runtime_config_key(key)
}

fn resolve_config_get_raw(key: &str, scalar: Option<&str>, runtime_config: Option<&str>) -> String {
    if key == RUNTIME_CONFIG_BLOB_KEY {
        return "null".to_string();
    }
    if let Some(value) = scalar {
        return serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
    }
    if !crate::services::settings::is_runtime_config_key(key) {
        return "null".to_string();
    }

    runtime_config
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| {
            value
                .as_object()
                .and_then(|values| values.get(key))
                .and_then(|value| serde_json::to_string(value).ok())
        })
        .unwrap_or_else(|| "null".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDatabase {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestDatabase {
        async fn create() -> Self {
            let admin_url = crate::dispatch::test_support::postgres_admin_database_url();
            let database_name = format!("agentdesk_config_ops_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!(
                "{}/{}",
                crate::dispatch::test_support::postgres_base_database_url(),
                database_name
            );
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "config ops pg tests",
            )
            .await
            .expect("create config ops postgres test db");
            Self {
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "config ops pg tests",
            )
            .await
            .expect("drop config ops postgres test db");
        }
    }

    #[test]
    fn config_get_raw_queries_runtime_blob_only_for_allowlisted_scalar_misses() {
        assert!(!should_load_runtime_config("maxEntryRetries", Some("9")));
        assert!(should_load_runtime_config("maxEntryRetries", None));
        assert!(!should_load_runtime_config(RUNTIME_CONFIG_BLOB_KEY, None));
        assert!(!should_load_runtime_config("privateBlobField", None));
    }

    #[test]
    fn config_get_raw_never_exposes_the_reserved_runtime_blob_key() {
        let runtime_blob = r#"{"maxEntryRetries":7}"#;

        assert_eq!(
            resolve_config_get_raw(
                RUNTIME_CONFIG_BLOB_KEY,
                Some(runtime_blob),
                Some(runtime_blob)
            ),
            "null"
        );
    }

    #[test]
    fn config_get_raw_keeps_scalar_string_semantics_and_precedence() {
        let runtime_blob = r#"{"review_enabled":true,"maxEntryRetries":7}"#;

        assert_eq!(
            resolve_config_get_raw("review_enabled", Some("false"), Some(runtime_blob)),
            r#""false""#,
            "existing CONFIG_KEYS scalars must still reach JS as strings"
        );
        assert_eq!(
            resolve_config_get_raw("maxEntryRetries", Some("9"), Some(runtime_blob)),
            r#""9""#,
            "a legacy runtime scalar must win over the blob"
        );
    }

    #[test]
    fn config_get_raw_preserves_runtime_blob_json_types() {
        let runtime_blob = r#"{
            "staleDispatchedRecoverNullDispatch": false,
            "maxEntryRetries": 7,
            "staleDispatchedTerminalStatuses": "failed,expired"
        }"#;
        let array_blob = r#"{
            "staleDispatchedTerminalStatuses": ["failed", "expired"]
        }"#;

        assert_eq!(
            resolve_config_get_raw(
                "staleDispatchedRecoverNullDispatch",
                None,
                Some(runtime_blob)
            ),
            "false"
        );
        assert_eq!(
            resolve_config_get_raw("maxEntryRetries", None, Some(runtime_blob)),
            "7"
        );
        assert_eq!(
            resolve_config_get_raw("staleDispatchedTerminalStatuses", None, Some(runtime_blob)),
            r#""failed,expired""#
        );
        assert_eq!(
            resolve_config_get_raw("staleDispatchedTerminalStatuses", None, Some(array_blob)),
            r#"["failed","expired"]"#
        );
    }

    #[test]
    fn config_get_raw_runtime_blob_fallback_is_allowlisted_and_fail_closed() {
        assert_eq!(
            resolve_config_get_raw(
                "privateBlobField",
                None,
                Some(r#"{"privateBlobField":"secret"}"#)
            ),
            "null"
        );
        assert_eq!(
            resolve_config_get_raw("maxEntryRetries", None, Some("not-json")),
            "null"
        );
        assert_eq!(
            resolve_config_get_raw("maxEntryRetries", None, Some("[]")),
            "null"
        );
        assert_eq!(
            resolve_config_get_raw("maxEntryRetries", None, Some("{}")),
            "null"
        );
        assert_eq!(
            resolve_config_get_raw("maxEntryRetries", None, None),
            "null"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn policy_engine_config_get_query_error_fails_closed_with_live_postgres() {
        let database = TestDatabase::create().await;
        let pool =
            crate::db::postgres::connect_test_pool(&database.database_url, "config ops pg tests")
                .await
                .expect("connect config ops postgres test db");

        let test_result: Result<(String, i32, bool), String> = async {
            sqlx::query("CREATE TABLE kv_meta (key TEXT PRIMARY KEY, value TEXT)")
                .execute(&pool)
                .await
                .map_err(|error| format!("create config ops kv_meta table: {error}"))?;
            sqlx::query("INSERT INTO kv_meta (key, value) VALUES ($1, $2)")
                .bind("review_enabled")
                .bind("false")
                .execute(&pool)
                .await
                .map_err(|error| format!("seed config ops scalar: {error}"))?;

            let mut config = crate::config::Config::default();
            config.policies.dir = std::path::PathBuf::from("/nonexistent");
            config.policies.hot_reload = false;
            let engine = crate::engine::PolicyEngine::new_with_pg(&config, Some(pool.clone()))
                .map_err(|error| format!("build config ops PolicyEngine: {error}"))?;
            let before: String = engine
                .eval_js(r#"agentdesk.config.get("review_enabled")"#)
                .map_err(|error| {
                    format!("evaluate pre-error scalar through PolicyEngine: {error}")
                })?;

            sqlx::query("DROP TABLE kv_meta")
                .execute(&pool)
                .await
                .map_err(|error| format!("drop config ops kv_meta table: {error}"))?;
            let connection_probe = sqlx::query_scalar::<_, i32>("SELECT 1")
                .fetch_one(&pool)
                .await
                .map_err(|error| format!("probe live postgres after kv_meta drop: {error}"))?;
            let query_error_is_null = engine
                .eval_js::<bool>(r#"agentdesk.config.get("review_enabled") === null"#)
                .map_err(|error| {
                    format!("evaluate query-error config get through PolicyEngine: {error}")
                })?;

            Ok((before, connection_probe, query_error_is_null))
        }
        .await;

        pool.close().await;
        database.drop().await;

        let (before, connection_probe, query_error_is_null) =
            test_result.expect("complete query-error config get contract");
        assert_eq!(
            before, "false",
            "valid scalar semantics must remain unchanged"
        );
        assert_eq!(
            connection_probe, 1,
            "the PostgreSQL connection must remain live"
        );
        assert!(
            query_error_is_null,
            "a kv_meta query error must become JS null instead of invalid JSON"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn policy_engine_config_get_preserves_types_precedence_and_fail_closed_contract() {
        let database = TestDatabase::create().await;
        let pool =
            crate::db::postgres::connect_test_pool(&database.database_url, "config ops pg tests")
                .await
                .expect("connect config ops postgres test db");
        sqlx::query("CREATE TABLE kv_meta (key TEXT PRIMARY KEY, value TEXT)")
            .execute(&pool)
            .await
            .expect("create config ops kv_meta table");
        sqlx::query("INSERT INTO kv_meta (key, value) VALUES ($1, $2), ($3, $4), ($5, $6)")
            .bind(RUNTIME_CONFIG_BLOB_KEY)
            .bind(
                r#"{
                    "maxEntryRetries": 7,
                    "maxRetries": 6,
                    "staleDispatchedTerminalStatuses": "failed,expired",
                    "staleDispatchedRecoverNullDispatch": false,
                    "reviewReminderMin": null,
                    "privateBlobField": "secret"
                }"#,
            )
            .bind("maxEntryRetries")
            .bind("9")
            .bind("review_enabled")
            .bind("false")
            .execute(&pool)
            .await
            .expect("seed config ops kv_meta values");

        let mut config = crate::config::Config::default();
        config.policies.dir = std::path::PathBuf::from("/nonexistent");
        config.policies.hot_reload = false;
        let engine = crate::engine::PolicyEngine::new_with_pg(&config, Some(pool.clone()))
            .expect("build config ops PolicyEngine");

        let initial_json: String = engine
            .eval_js(
                r#"JSON.stringify({
                    scalar: agentdesk.config.get("review_enabled"),
                    precedence: agentdesk.config.get("maxEntryRetries"),
                    boolValue: agentdesk.config.get("staleDispatchedRecoverNullDispatch"),
                    intValue: agentdesk.config.get("maxRetries"),
                    stringValue: agentdesk.config.get("staleDispatchedTerminalStatuses"),
                    nullValue: agentdesk.config.get("reviewReminderMin"),
                    miss: agentdesk.config.get("githubRepoCacheSec"),
                    unknown: agentdesk.config.get("privateBlobField"),
                    reserved: agentdesk.config.get("runtime-config")
                })"#,
            )
            .expect("evaluate config get initial values through PolicyEngine");
        let initial: serde_json::Value =
            serde_json::from_str(&initial_json).expect("parse PolicyEngine config result");
        assert_eq!(
            initial,
            serde_json::json!({
                "scalar": "false",
                "precedence": "9",
                "boolValue": false,
                "intValue": 6,
                "stringValue": "failed,expired",
                "nullValue": null,
                "miss": null,
                "unknown": null,
                "reserved": null
            })
        );

        sqlx::query("DELETE FROM kv_meta WHERE key = $1")
            .bind("maxEntryRetries")
            .execute(&pool)
            .await
            .expect("delete legacy runtime scalar");
        sqlx::query("UPDATE kv_meta SET value = $1 WHERE key = $2")
            .bind(
                r#"{
                    "maxEntryRetries": 7,
                    "staleDispatchedTerminalStatuses": ["failed", "expired"]
                }"#,
            )
            .bind(RUNTIME_CONFIG_BLOB_KEY)
            .execute(&pool)
            .await
            .expect("write array runtime-config");
        let array_json: String = engine
            .eval_js(
                r#"JSON.stringify({
                    blobAfterScalarRemoval: agentdesk.config.get("maxEntryRetries"),
                    arrayValue: agentdesk.config.get("staleDispatchedTerminalStatuses")
                })"#,
            )
            .expect("evaluate config get array values through PolicyEngine");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&array_json).expect("parse array result"),
            serde_json::json!({
                "blobAfterScalarRemoval": 7,
                "arrayValue": ["failed", "expired"]
            })
        );

        sqlx::query("UPDATE kv_meta SET value = $1 WHERE key = $2")
            .bind("not-json")
            .bind(RUNTIME_CONFIG_BLOB_KEY)
            .execute(&pool)
            .await
            .expect("write malformed runtime-config");
        assert_eq!(
            engine
                .eval_js::<String>(
                    r#"JSON.stringify({
                        known: agentdesk.config.get("maxEntryRetries"),
                        scalar: agentdesk.config.get("review_enabled"),
                        reserved: agentdesk.config.get("runtime-config")
                    })"#
                )
                .expect("evaluate malformed blob through PolicyEngine"),
            r#"{"known":null,"scalar":"false","reserved":null}"#
        );
        sqlx::query("UPDATE kv_meta SET value = $1 WHERE key = $2")
            .bind("[]")
            .bind(RUNTIME_CONFIG_BLOB_KEY)
            .execute(&pool)
            .await
            .expect("write non-object runtime-config");
        assert!(
            engine
                .eval_js::<bool>(r#"agentdesk.config.get("maxEntryRetries") === null"#)
                .expect("evaluate non-object blob through PolicyEngine")
        );

        sqlx::query("UPDATE kv_meta SET value = $1 WHERE key = $2")
            .bind(r#"{"maxEntryRetries":55}"#)
            .bind(RUNTIME_CONFIG_BLOB_KEY)
            .execute(&pool)
            .await
            .expect("restore valid runtime-config before query error check");
        assert!(
            engine
                .eval_js::<bool>(r#"agentdesk.config.get("maxEntryRetries") === 55"#)
                .expect("evaluate pre-error runtime-config through PolicyEngine"),
            "the pre-error query must prove the configured value is readable"
        );
        pool.close().await;
        database.drop().await;
        assert!(
            engine
                .eval_js::<bool>(r#"agentdesk.config.get("maxEntryRetries") === null"#)
                .expect("evaluate closed-pool query through PolicyEngine"),
            "database errors must preserve the fail-closed JS contract"
        );
        drop(engine);
    }
}
