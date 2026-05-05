use super::*;
use crate::db;
use crate::engine::PolicyEngine;
use crate::server::routes::AppState;
use crate::services::settings::{KvSeedAction, config_default_seed_actions, seed_config_defaults};
use serde_json::{Value, json};
use std::path::PathBuf;

fn test_db() -> db::Db {
    let conn = sqlite_test::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    db::schema::migrate(&conn).unwrap();
    db::wrap_conn(conn)
}

fn test_engine(db: &db::Db) -> PolicyEngine {
    let mut config = crate::config::Config::default();
    config.policies.dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    config.policies.hot_reload = false;
    PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap()
}

fn test_engine_with_pg(pg_pool: sqlx::PgPool) -> PolicyEngine {
    let mut config = crate::config::Config::default();
    config.policies.dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    config.policies.hot_reload = false;
    PolicyEngine::new_with_pg(&config, Some(pg_pool)).unwrap()
}

/// Per-test Postgres database lifecycle for the #1238 migration of the
/// settings handler tests, which now require a PG pool because the
/// runtime-config and kv_meta surfaces are PG-only after PR #1306.
struct SettingsPgDatabase {
    _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
    admin_url: String,
    database_name: String,
    database_url: String,
}

impl SettingsPgDatabase {
    async fn create() -> Self {
        let lifecycle = crate::db::postgres::lock_test_lifecycle();
        let admin_url = pg_test_admin_database_url();
        let database_name = format!("agentdesk_settings_{}", uuid::Uuid::new_v4().simple());
        let database_url = format!("{}/{}", pg_test_base_database_url(), database_name);
        crate::db::postgres::create_test_database(
            &admin_url,
            &database_name,
            "settings handler pg",
        )
        .await
        .expect("create settings postgres test db");

        Self {
            _lifecycle: lifecycle,
            admin_url,
            database_name,
            database_url,
        }
    }

    async fn migrate(&self) -> sqlx::PgPool {
        crate::db::postgres::connect_test_pool_and_migrate(
            &self.database_url,
            "settings handler pg",
        )
        .await
        .expect("connect + migrate settings postgres test db")
    }

    async fn drop(self) {
        crate::db::postgres::drop_test_database(
            &self.admin_url,
            &self.database_name,
            "settings handler pg",
        )
        .await
        .expect("drop settings postgres test db");
    }
}

fn pg_test_base_database_url() -> String {
    if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
        let trimmed = base.trim();
        if !trimmed.is_empty() {
            return trimmed.trim_end_matches('/').to_string();
        }
    }
    let user = std::env::var("PGUSER")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| std::env::var("USER").ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "postgres".to_string());
    let password = std::env::var("PGPASSWORD")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let host = std::env::var("PGHOST")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "localhost".to_string());
    let port = std::env::var("PGPORT")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "5432".to_string());
    match password {
        Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
        None => format!("postgresql://{user}@{host}:{port}"),
    }
}

fn pg_test_admin_database_url() -> String {
    let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "postgres".to_string());
    format!("{}/{}", pg_test_base_database_url(), admin_db)
}

/// Build a [`AppState`] backed by both an in-memory libsql DB *and* a real
/// PG pool, mirroring `meetings.rs` but additionally honoring the YAML
/// baseline used by some settings tests.
fn pg_app_state(db: db::Db, pool: sqlx::PgPool, config: Option<crate::config::Config>) -> AppState {
    let mut state =
        AppState::test_state_with_pg(db.clone(), test_engine_with_pg(pool.clone()), pool);
    if let Some(cfg) = config {
        state.config = std::sync::Arc::new(cfg);
    }
    state
}

/// Apply the same kv_meta seed actions the runtime invokes for
/// `seed_config_defaults`, but routed at PG. Used by tests that need the
/// CONFIG_KEYS baseline staged in PG kv_meta before exercising handlers.
async fn pg_apply_config_default_seed_actions(pool: &sqlx::PgPool, config: &crate::config::Config) {
    for action in config_default_seed_actions(config) {
        match action {
            KvSeedAction::Put { key, value } => {
                sqlx::query(
                    "INSERT INTO kv_meta (key, value)
                     VALUES ($1, $2)
                     ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
                )
                .bind(&key)
                .bind(&value)
                .execute(pool)
                .await
                .expect("pg seed kv_meta put");
            }
            KvSeedAction::PutIfAbsent { key, value } => {
                sqlx::query(
                    "INSERT INTO kv_meta (key, value)
                     VALUES ($1, $2)
                     ON CONFLICT (key) DO NOTHING",
                )
                .bind(&key)
                .bind(&value)
                .execute(pool)
                .await
                .expect("pg seed kv_meta put_if_absent");
            }
            KvSeedAction::Delete { key } => {
                sqlx::query("DELETE FROM kv_meta WHERE key = $1")
                    .bind(&key)
                    .execute(pool)
                    .await
                    .expect("pg seed kv_meta delete");
            }
        }
    }
}

#[tokio::test]
async fn get_config_entries_pg_includes_merge_automation_and_omits_retired_keys() {
    let pg_db = SettingsPgDatabase::create().await;
    let pool = pg_db.migrate().await;
    let db = test_db();
    let state = pg_app_state(db.clone(), pool.clone(), None);

    let (status, Json(body)) = get_config_entries(State(state)).await;
    assert_eq!(status, StatusCode::OK);

    let entries = body["entries"].as_array().expect("entries array");
    let keys: std::collections::HashSet<&str> = entries
        .iter()
        .filter_map(|entry| entry["key"].as_str())
        .collect();

    assert!(keys.contains("context_compact_percent_codex"));
    assert!(keys.contains("context_compact_percent_claude"));
    assert!(keys.contains("merge_automation_enabled"));
    assert!(keys.contains("merge_strategy"));
    assert!(keys.contains("merge_strategy_mode"));
    assert!(keys.contains("merge_allowed_authors"));
    assert!(!keys.contains("max_chain_depth"));
    assert!(!keys.contains("context_clear_percent"));
    assert!(!keys.contains("context_clear_idle_minutes"));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn get_config_entries_pg_reports_baseline_override_and_restart_metadata() {
    let pg_db = SettingsPgDatabase::create().await;
    let pool = pg_db.migrate().await;
    let db = test_db();
    let mut config = crate::config::Config::default();
    config.automation.strategy = Some("rebase".to_string());
    let expected_port = config.server.port.to_string();

    pg_apply_config_default_seed_actions(&pool, &config).await;
    for (key, value) in [
        ("merge_strategy", "merge"),
        ("max_review_rounds", "7"),
        ("server_port", "9999"),
    ] {
        sqlx::query(
            "INSERT INTO kv_meta (key, value)
             VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(key)
        .bind(value)
        .execute(&pool)
        .await
        .unwrap();
    }

    let state = pg_app_state(db.clone(), pool.clone(), Some(config));
    let (status, Json(body)) = get_config_entries(State(state)).await;
    assert_eq!(status, StatusCode::OK);

    let entries = body["entries"].as_array().expect("entries array");
    let values: std::collections::HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|entry| Some((entry["key"].as_str()?, entry)))
        .collect();

    let merge_strategy = values.get("merge_strategy").expect("merge_strategy");
    assert_eq!(merge_strategy["value"], json!("merge"));
    assert_eq!(merge_strategy["baseline"], json!("rebase"));
    assert_eq!(merge_strategy["baseline_source"], json!("yaml"));
    assert_eq!(merge_strategy["override_active"], json!(true));
    assert_eq!(merge_strategy["editable"], json!(true));
    assert_eq!(
        merge_strategy["restart_behavior"],
        json!("reseed-from-yaml")
    );

    let merge_strategy_mode = values
        .get("merge_strategy_mode")
        .expect("merge_strategy_mode");
    assert_eq!(merge_strategy_mode["value"], json!("direct-first"));
    assert_eq!(merge_strategy_mode["baseline"], json!("direct-first"));
    assert_eq!(merge_strategy_mode["baseline_source"], json!("hardcoded"));
    assert_eq!(merge_strategy_mode["override_active"], json!(false));
    assert_eq!(merge_strategy_mode["editable"], json!(true));
    assert_eq!(
        merge_strategy_mode["restart_behavior"],
        json!("persist-live-override")
    );

    let max_review_rounds = values.get("max_review_rounds").expect("max_review_rounds");
    assert_eq!(max_review_rounds["value"], json!("7"));
    assert_eq!(max_review_rounds["baseline"], json!("3"));
    assert_eq!(max_review_rounds["baseline_source"], json!("hardcoded"));
    assert_eq!(max_review_rounds["override_active"], json!(true));
    assert_eq!(
        max_review_rounds["restart_behavior"],
        json!("persist-live-override")
    );

    let server_port = values.get("server_port").expect("server_port");
    assert_eq!(server_port["value"], json!(expected_port));
    assert_eq!(server_port["baseline"], json!(expected_port));
    assert_eq!(server_port["baseline_source"], json!("config"));
    assert_eq!(server_port["override_active"], json!(false));
    assert_eq!(server_port["editable"], json!(false));
    assert_eq!(server_port["restart_behavior"], json!("config-only"));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn patch_config_entries_pg_accepts_merge_automation_and_provider_specific_keys() {
    let pg_db = SettingsPgDatabase::create().await;
    let pool = pg_db.migrate().await;
    let db = test_db();
    let state = pg_app_state(db.clone(), pool.clone(), None);

    let (patch_status, Json(patch_body)) = patch_config_entries(
        State(state.clone()),
        Json(json!({
            "merge_automation_enabled": true,
            "merge_strategy": "rebase",
            "merge_strategy_mode": "pr-always",
            "merge_allowed_authors": "itismyfield,octocat",
            "context_compact_percent_codex": "85",
            "context_compact_percent_claude": "75",
        })),
    )
    .await;
    assert_eq!(patch_status, StatusCode::OK);
    assert_eq!(patch_body["updated"], json!(6));
    assert_eq!(patch_body["rejected"], json!([]));

    let (get_status, Json(get_body)) = get_config_entries(State(state)).await;
    assert_eq!(get_status, StatusCode::OK);

    let entries = get_body["entries"].as_array().expect("entries array");
    let values: std::collections::HashMap<&str, Option<&str>> = entries
        .iter()
        .filter_map(|entry| Some((entry["key"].as_str()?, entry["value"].as_str())))
        .collect();

    assert_eq!(
        values.get("context_compact_percent_codex"),
        Some(&Some("85"))
    );
    assert_eq!(
        values.get("context_compact_percent_claude"),
        Some(&Some("75"))
    );
    assert_eq!(values.get("merge_automation_enabled"), Some(&Some("true")));
    assert_eq!(values.get("merge_strategy"), Some(&Some("rebase")));
    assert_eq!(values.get("merge_strategy_mode"), Some(&Some("pr-always")));
    assert_eq!(
        values.get("merge_allowed_authors"),
        Some(&Some("itismyfield,octocat"))
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn patch_config_entries_pg_rejects_read_only_server_port() {
    let pg_db = SettingsPgDatabase::create().await;
    let pool = pg_db.migrate().await;
    let db = test_db();
    let state = pg_app_state(db.clone(), pool.clone(), None);

    let (patch_status, Json(patch_body)) = patch_config_entries(
        State(state),
        Json(json!({
            "server_port": "9999",
            "merge_strategy": "merge",
        })),
    )
    .await;
    assert_eq!(patch_status, StatusCode::OK);
    assert_eq!(patch_body["updated"], json!(1));
    assert_eq!(patch_body["rejected"], json!(["server_port"]));

    let server_port_override_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM kv_meta WHERE key = 'server_port' AND value = '9999'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(server_port_override_count, 0);

    pool.close().await;
    pg_db.drop().await;
}

#[test]
fn seed_config_defaults_removes_retired_config_keys() {
    let conn = sqlite_test::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    db::schema::migrate(&conn).unwrap();

    conn.execute(
        "INSERT INTO kv_meta (key, value) VALUES ('max_chain_depth', '5')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO kv_meta (key, value) VALUES ('context_clear_percent', '85')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO kv_meta (key, value) VALUES ('counter_model_review_enabled', 'false')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO kv_meta (key, value) VALUES ('narrate_progress', 'true')",
        [],
    )
    .unwrap();

    seed_config_defaults(&conn, &crate::config::Config::default());

    let retired_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kv_meta WHERE key IN ('max_chain_depth', 'context_clear_percent', 'counter_model_review_enabled', 'narrate_progress')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retired_count, 0);
}

#[test]
fn seed_config_defaults_prefers_yaml_values_and_preserves_other_runtime_overrides() {
    let conn = sqlite_test::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    db::schema::migrate(&conn).unwrap();

    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('merge_strategy', 'merge')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('requested_timeout_min', '15')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('max_review_rounds', '7')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('runtime-config', '{\"dispatchPollSec\":10,\"maxRetries\":7}')",
        [],
    )
    .unwrap();

    let mut config = crate::config::Config::default();
    config.automation.strategy = Some("rebase".to_string());
    config.runtime.requested_timeout_min = Some(55);
    config.runtime.long_turn_alert_interval_min = Some(35);
    config.runtime.dispatch_poll_sec = Some(45);
    config.runtime.max_entry_retries = Some(6);
    config.runtime.stale_dispatched_grace_min = Some(4);
    config.runtime.stale_dispatched_terminal_statuses =
        Some("cancelled,failed,expired".to_string());
    config.runtime.stale_dispatched_recover_null_dispatch = Some(false);
    config.runtime.stale_dispatched_recover_missing_dispatch = Some(true);

    seed_config_defaults(&conn, &config);

    let merge_strategy: String = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'merge_strategy'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(merge_strategy, "rebase");

    let timeout_min: String = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'requested_timeout_min'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(timeout_min, "55");

    let long_turn_interval_min: String = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'long_turn_alert_interval_min'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(long_turn_interval_min, "35");

    let max_review_rounds: String = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'max_review_rounds'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(max_review_rounds, "7");

    let runtime_config: Value = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'runtime-config'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| json!({}));
    assert_eq!(runtime_config["dispatchPollSec"], json!(45));
    assert_eq!(runtime_config["maxRetries"], json!(7));
    assert_eq!(runtime_config["maxEntryRetries"], json!(6));
    assert_eq!(runtime_config["staleDispatchedGraceMin"], json!(4));
    assert_eq!(
        runtime_config["staleDispatchedTerminalStatuses"],
        json!("cancelled,failed,expired")
    );
    assert_eq!(
        runtime_config["staleDispatchedRecoverNullDispatch"],
        json!(false)
    );
    assert_eq!(
        runtime_config["staleDispatchedRecoverMissingDispatch"],
        json!(true)
    );
}

#[test]
fn seed_config_defaults_can_reset_runtime_overrides_on_restart() {
    let conn = sqlite_test::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    db::schema::migrate(&conn).unwrap();

    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('merge_allowed_authors', 'legacy-user')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('runtime-config', '{\"dispatchPollSec\":10,\"maxRetries\":7}')",
        [],
    )
    .unwrap();

    let mut config = crate::config::Config::default();
    config.runtime.reset_overrides_on_restart = true;
    config.automation.enabled = Some(true);

    seed_config_defaults(&conn, &config);

    let merge_allowed_authors_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kv_meta WHERE key = 'merge_allowed_authors'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(merge_allowed_authors_count, 0);

    let merge_automation_enabled: String = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'merge_automation_enabled'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(merge_automation_enabled, "true");

    let runtime_config: Value = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'runtime-config'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| json!({}));
    assert_eq!(runtime_config["dispatchPollSec"], json!(30));
    assert_eq!(runtime_config["maxRetries"], json!(3));
    assert_eq!(runtime_config["maxEntryRetries"], json!(3));
    assert_eq!(runtime_config["staleDispatchedGraceMin"], json!(2));
    assert_eq!(
        runtime_config["staleDispatchedTerminalStatuses"],
        json!("cancelled,failed")
    );
    assert_eq!(
        runtime_config["staleDispatchedRecoverNullDispatch"],
        json!(true)
    );
    assert_eq!(
        runtime_config["staleDispatchedRecoverMissingDispatch"],
        json!(true)
    );
}

#[tokio::test]
async fn get_runtime_config_pg_uses_yaml_baseline_from_app_state() {
    let pg_db = SettingsPgDatabase::create().await;
    let pool = pg_db.migrate().await;
    let db = test_db();
    let mut config = crate::config::Config::default();
    config.runtime.dispatch_poll_sec = Some(45);
    config.runtime.max_retries = Some(5);
    config.runtime.max_entry_retries = Some(4);
    config.runtime.stale_dispatched_grace_min = Some(6);
    config.runtime.stale_dispatched_terminal_statuses =
        Some("cancelled,failed,expired".to_string());
    config.runtime.stale_dispatched_recover_null_dispatch = Some(false);
    config.runtime.stale_dispatched_recover_missing_dispatch = Some(false);
    let state = pg_app_state(db.clone(), pool.clone(), Some(config));

    let (status, Json(body)) = get_runtime_config(State(state)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["current"]["dispatchPollSec"], json!(45));
    assert_eq!(body["defaults"]["dispatchPollSec"], json!(45));
    assert_eq!(body["current"]["maxRetries"], json!(5));
    assert_eq!(body["defaults"]["maxRetries"], json!(5));
    assert_eq!(body["current"]["maxEntryRetries"], json!(4));
    assert_eq!(body["defaults"]["maxEntryRetries"], json!(4));
    assert_eq!(body["current"]["staleDispatchedGraceMin"], json!(6));
    assert_eq!(body["defaults"]["staleDispatchedGraceMin"], json!(6));
    assert_eq!(
        body["current"]["staleDispatchedTerminalStatuses"],
        json!("cancelled,failed,expired")
    );
    assert_eq!(
        body["defaults"]["staleDispatchedTerminalStatuses"],
        json!("cancelled,failed,expired")
    );
    assert_eq!(
        body["current"]["staleDispatchedRecoverNullDispatch"],
        json!(false)
    );
    assert_eq!(
        body["current"]["staleDispatchedRecoverMissingDispatch"],
        json!(false)
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn put_runtime_config_pg_mirrors_scalar_keys_for_runtime_consumers() {
    let pg_db = SettingsPgDatabase::create().await;
    let pool = pg_db.migrate().await;
    let db = test_db();
    let state = pg_app_state(db.clone(), pool.clone(), None);

    let (status, _) = put_runtime_config(
        State(state),
        Json(json!({
            "dispatchPollSec": 15,
            "maxRetries": 7,
            "maxEntryRetries": 4,
            "staleDispatchedGraceMin": 5,
            "staleDispatchedTerminalStatuses": "cancelled,failed,expired",
            "staleDispatchedRecoverNullDispatch": false,
            "rateLimitStaleSec": 900
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let stale_sec: String =
        sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = 'rateLimitStaleSec'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stale_sec, "900");
    let max_entry_retries: String =
        sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = 'maxEntryRetries'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(max_entry_retries, "4");
    let stale_grace_min: String =
        sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = 'staleDispatchedGraceMin'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stale_grace_min, "5");
    let stale_terminal_statuses: String = sqlx::query_scalar(
        "SELECT value FROM kv_meta WHERE key = 'staleDispatchedTerminalStatuses'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stale_terminal_statuses, "cancelled,failed,expired");
    let stale_recover_null_dispatch: String = sqlx::query_scalar(
        "SELECT value FROM kv_meta WHERE key = 'staleDispatchedRecoverNullDispatch'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stale_recover_null_dispatch, "false");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn put_settings_pg_is_full_replace_and_strips_retired_company_keys() {
    let pg_db = SettingsPgDatabase::create().await;
    let pool = pg_db.migrate().await;
    let db = test_db();
    let state = pg_app_state(db.clone(), pool.clone(), None);

    let (first_status, _) = put_settings(
        State(state.clone()),
        Json(json!({
            "companyName": "AgentDesk",
            "roomThemes": {"dev": {"accent": "#fff"}},
            "autoUpdateEnabled": true,
        })),
    )
    .await;
    assert_eq!(first_status, StatusCode::OK);

    let (_, Json(first_body)) = get_settings(State(state.clone())).await;
    assert_eq!(first_body["companyName"], json!("AgentDesk"));
    assert!(first_body.get("autoUpdateEnabled").is_none());
    assert_eq!(first_body["roomThemes"]["dev"]["accent"], json!("#fff"));

    let (second_status, _) = put_settings(
        State(state.clone()),
        Json(json!({
            "theme": "light",
        })),
    )
    .await;
    assert_eq!(second_status, StatusCode::OK);

    let (_, Json(second_body)) = get_settings(State(state)).await;
    assert_eq!(second_body, json!({"theme": "light"}));

    pool.close().await;
    pg_db.drop().await;
}
