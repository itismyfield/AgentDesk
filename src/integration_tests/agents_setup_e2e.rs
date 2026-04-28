//! #1111 (912-6): End-to-end smoke test for the agent setup wizard
//! (`/api/agents/setup`).
//!
//! What this lane covers (see `docs/agent-onboarding.md` §8):
//!
//! 1. Mock Discord channel — wire up [`super::discord_flow::mock_discord::MockDiscord`]
//!    as the outbound transport, pre-record an "existing" channel snowflake.
//! 2. Dry-run + execute through the public HTTP surface (`POST
//!    /agents/setup`); assert all six mutation steps land on disk and DB.
//! 3. Confirm the bound channel actually receives a Discord message via
//!    [`crate::services::discord::outbound::deliver_outbound`] with the mock
//!    transport — exactly one POST is recorded for that channel.
//! 4. Inject a forced failure
//!    (`AGENTDESK_TEST_AGENT_SETUP_FAIL_AFTER=prompt_file`) and assert the
//!    server-side rollback runs to completion: HTTP 500, non-empty
//!    `rolled_back[]`, no agent left on disk or in DB.
//! 5. Archive → unarchive round-trip — `agentdesk.yaml` is restored
//!    byte-for-byte from the snapshot in `agent_archive`.
//! 6. Duplicate without leaking sensitive fields — caller-supplied `id`,
//!    `agent_id`, `discord_channel_id` (source channel), `token`, `api_key`,
//!    `system_prompt` must all be stripped by the duplicate route.
//!
//! These scenarios mirror the existing per-route tests in
//! `routes_tests.rs`, but are wired together here at the integration level
//! so a regression in *any* of them breaks a single, named test.

use super::*;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

// Reuse the mock Discord transport from the discord_flow harness instead of
// instantiating a second copy under this module — keeps the dedup invariants
// observed by both lanes identical.
use super::discord_flow::mock_discord::MockDiscord;

use crate::services::discord::outbound::{
    DeliveryResult, DiscordOutboundMessage, DiscordOutboundPolicy, OutboundDeduper,
    deliver_outbound,
};

/// Lock used to serialize env-var mutations across the harness. Mirrors the
/// `routes_tests::env_lock` pattern so we never race with other `#[cfg(test)]`
/// code that touches `AGENTDESK_ROOT_DIR` or
/// `AGENTDESK_TEST_AGENT_SETUP_FAIL_AFTER`.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::services::discord::runtime_store::lock_test_env()
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        // SAFETY: process-wide env mutation, serialized by env_lock().
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }

    fn set_path(key: &'static str, value: &std::path::Path) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

/// PG-backed app builder used by the wizard E2E lane. Wires the router with
/// a Postgres pool so the `/agents/setup` route — PG-only after #1306 — can
/// persist its mutations.
fn build_app_with_pg(
    db: db::Db,
    engine: crate::engine::PolicyEngine,
    pg_pool: sqlx::PgPool,
) -> axum::Router {
    let tx = crate::server::ws::new_broadcast();
    let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
    crate::server::routes::api_router_with_pg_for_tests(
        db,
        engine,
        crate::config::Config::default(),
        tx,
        buf,
        None,
        Some(pg_pool),
    )
}

/// Per-test Postgres database lifecycle — each migrated wizard E2E test
/// creates an isolated DB so concurrent runs don't share `agents` /
/// `agent_archive` rows. Modeled on the `PgRecoveryTestDatabase` pattern in
/// `integration_tests/tests/high_risk_recovery.rs`.
struct WizardPgDatabase {
    _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
    admin_url: String,
    database_name: String,
    database_url: String,
}

impl WizardPgDatabase {
    async fn create() -> Self {
        let lifecycle = crate::db::postgres::lock_test_lifecycle();
        let admin_url = pg_test_admin_database_url();
        let database_name = format!("agentdesk_wizard_e2e_{}", uuid::Uuid::new_v4().simple());
        let database_url = format!("{}/{}", pg_test_base_database_url(), database_name);
        crate::db::postgres::create_test_database(
            &admin_url,
            &database_name,
            "agents_setup_e2e wizard pg",
        )
        .await
        .expect("create wizard postgres test db");

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
            "agents_setup_e2e wizard pg",
        )
        .await
        .expect("connect + migrate wizard postgres test db")
    }

    async fn drop(self) {
        crate::db::postgres::drop_test_database(
            &self.admin_url,
            &self.database_name,
            "agents_setup_e2e wizard pg",
        )
        .await
        .expect("drop wizard postgres test db");
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

/// Materializes the runtime-root layout the wizard expects: empty
/// `agentdesk.yaml`, a shared prompt template, and a fake skill directory.
fn seed_runtime_root(runtime_root: &std::path::Path) {
    let config_path = crate::runtime_layout::config_file_path(runtime_root);
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    crate::config::save_to_path(&config_path, &crate::config::Config::default()).unwrap();

    let prompt_template = crate::runtime_layout::shared_prompt_path(runtime_root);
    std::fs::create_dir_all(prompt_template.parent().unwrap()).unwrap();
    std::fs::write(&prompt_template, "shared prompt\n").unwrap();

    let skill_dir = runtime_root.join("skills").join("memory-read");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "# memory-read\n").unwrap();
}

async fn post_json(
    app: &axum::Router,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice::<serde_json::Value>(&bytes)
        .unwrap_or_else(|err| panic!("non-JSON response from {uri}: {err}: {bytes:?}"));
    (status, json)
}

/// DoD scenario 1: dashboard wizard happy path (dry-run + execute) ends with
/// the agent on disk + in DB, **and** a relayed message reaches the mock
/// Discord transport for the bound channel. Migrated to a Postgres fixture
/// for #1238 — the SQLite-only ancestor panicked in `/agents/setup` after
/// PR #1306 made the route PG-only.
#[tokio::test]
async fn wizard_pg_creates_agent_and_delivers_to_bound_channel() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());

    seed_runtime_root(runtime_root.path());

    let pg_db = WizardPgDatabase::create().await;
    let pool = pg_db.migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(pool.clone());
    let app = build_app_with_pg(db.clone(), engine, pool.clone());
    let agent_id = "wiz-agent";
    let channel_id = "1473922824350601301";

    // Step 1 — dry run.
    let body = serde_json::json!({
        "agent_id": agent_id,
        "channel_id": channel_id,
        "provider": "codex",
        "prompt_template_path": "config/agents/_shared.prompt.md",
        "skills": ["memory-read"],
        "dry_run": true,
    });
    let (status, json) = post_json(&app, "/agents/setup", body.clone()).await;
    assert_eq!(status, StatusCode::OK, "dry_run should return 200");
    assert_eq!(json["dry_run"], true);
    assert!(json["created"].as_array().unwrap().is_empty());
    let planned = json["planned"].as_array().unwrap();
    let planned_steps: std::collections::BTreeSet<&str> =
        planned.iter().filter_map(|p| p["step"].as_str()).collect();
    for required in [
        "agentdesk_yaml",
        "discord_binding",
        "prompt_file",
        "workspace_seed",
        "db_seed",
        "skill_mapping",
    ] {
        assert!(
            planned_steps.contains(required),
            "dry_run plan missing step {required}: {planned:?}"
        );
    }
    // Nothing on disk yet.
    assert!(
        !runtime_root
            .path()
            .join("config/agents")
            .join(agent_id)
            .join("IDENTITY.md")
            .exists(),
        "dry_run must not create prompt file"
    );

    // Step 2 — execute.
    let mut body_exec = body.clone();
    body_exec["dry_run"] = serde_json::Value::Bool(false);
    let (status, json) = post_json(&app, "/agents/setup", body_exec).await;
    assert_eq!(status, StatusCode::CREATED, "execute should return 201");
    assert_eq!(json["ok"], true);
    assert!(
        !json["created"].as_array().unwrap().is_empty(),
        "execute must report created mutations"
    );

    // All six mutation surfaces persisted.
    let config = crate::config::load_from_path(&crate::runtime_layout::config_file_path(
        runtime_root.path(),
    ))
    .unwrap();
    let agent = config
        .agents
        .iter()
        .find(|a| a.id == agent_id)
        .expect("agent block in agentdesk.yaml");
    assert_eq!(agent.provider, "codex");
    assert_eq!(
        agent
            .channels
            .codex
            .as_ref()
            .and_then(|c| c.channel_id())
            .as_deref(),
        Some(channel_id),
        "codex channel must be bound to {channel_id}"
    );
    assert!(
        runtime_root
            .path()
            .join("workspaces")
            .join(agent_id)
            .is_dir()
    );
    assert!(
        runtime_root
            .path()
            .join("config/agents")
            .join(agent_id)
            .join("IDENTITY.md")
            .is_file()
    );

    let db_channel: Option<String> =
        sqlx::query_scalar("SELECT discord_channel_cdx FROM agents WHERE id = $1")
            .bind(agent_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(db_channel.as_deref(), Some(channel_id));

    let manifest_path = crate::runtime_layout::managed_skills_manifest_path(runtime_root.path());
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["skills"]["memory-read"]["workspaces"][0], agent_id);

    // Step 3 — Discord delivery via mock transport against the bound channel.
    let mock = MockDiscord::new();
    let dedup = OutboundDeduper::new();
    let policy = DiscordOutboundPolicy::default();
    let msg = DiscordOutboundMessage::new(channel_id.to_string(), "wizard ready: hello agent")
        .with_correlation(format!("e2e:{agent_id}"), "watcher:agent-setup-e2e");
    let delivery = deliver_outbound(&mock, &dedup, msg, policy).await;
    assert!(
        matches!(delivery, DeliveryResult::Success { .. }),
        "outbound delivery must succeed against bound channel: {delivery:?}"
    );
    assert_eq!(
        mock.calls_to(channel_id),
        1,
        "exactly one POST recorded for the bound channel"
    );
    assert_eq!(mock.call_count(), 1);

    pool.close().await;
    pg_db.drop().await;
}

/// DoD scenario 2: forcing a step to fail must produce a clean rollback —
/// HTTP 500, `rolled_back[]` populated, *no* trace of the agent on disk or
/// in DB. PG-fixture migration of the original SQLite-only test.
#[tokio::test]
async fn wizard_pg_failure_injection_rolls_back_to_clean_state() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let _fail_env = EnvVarGuard::set("AGENTDESK_TEST_AGENT_SETUP_FAIL_AFTER", "prompt_file");
    seed_runtime_root(runtime_root.path());

    let pg_db = WizardPgDatabase::create().await;
    let pool = pg_db.migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(pool.clone());
    let app = build_app_with_pg(db.clone(), engine, pool.clone());
    let agent_id = "wiz-rollback";
    let body = serde_json::json!({
        "agent_id": agent_id,
        "channel_id": "1473922824350601302",
        "provider": "codex",
        "prompt_template_path": "config/agents/_shared.prompt.md",
    });

    let (status, json) = post_json(&app, "/agents/setup", body).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json["ok"], false);
    let rolled_back = json["rolled_back"].as_array().unwrap();
    let rb_steps: std::collections::BTreeSet<&str> = rolled_back
        .iter()
        .filter_map(|r| r["step"].as_str())
        .collect();
    assert!(
        rb_steps.contains("prompt_file"),
        "rolled_back must include the failing step prompt_file: {rolled_back:?}"
    );
    assert!(
        rb_steps.contains("agentdesk_yaml"),
        "rolled_back must include the earlier agentdesk_yaml mutation: {rolled_back:?}"
    );

    // Disk + DB must be clean.
    let config = crate::config::load_from_path(&crate::runtime_layout::config_file_path(
        runtime_root.path(),
    ))
    .unwrap();
    assert!(config.agents.iter().all(|a| a.id != agent_id));
    assert!(
        !runtime_root
            .path()
            .join("config/agents")
            .join(agent_id)
            .join("IDENTITY.md")
            .exists()
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agents WHERE id = $1")
        .bind(agent_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "rolled-back DB row must be absent");

    pool.close().await;
    pg_db.drop().await;
}

/// DoD scenario 3: archive then unarchive must round-trip the
/// `agentdesk.yaml` block byte-for-byte. PG-fixture migration of the
/// original SQLite-only test.
#[tokio::test]
async fn wizard_pg_archive_unarchive_roundtrip() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    seed_runtime_root(runtime_root.path());

    let pg_db = WizardPgDatabase::create().await;
    let pool = pg_db.migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(pool.clone());
    let app = build_app_with_pg(db.clone(), engine, pool.clone());
    let agent_id = "wiz-archive";
    let channel_id = "1473922824350601303";

    let (status, _) = post_json(
        &app,
        "/agents/setup",
        serde_json::json!({
            "agent_id": agent_id,
            "channel_id": channel_id,
            "provider": "codex",
            "prompt_template_path": "config/agents/_shared.prompt.md",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let config_path = crate::runtime_layout::config_file_path(runtime_root.path());
    let snapshot_yaml = std::fs::read(&config_path).unwrap();

    // Archive.
    let (status, json) = post_json(
        &app,
        &format!("/agents/{agent_id}/archive"),
        serde_json::json!({"reason": "e2e archive", "discord_action": "none"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["archive_state"], "archived");
    let post_archive_config = crate::config::load_from_path(&config_path).unwrap();
    assert!(
        post_archive_config.agents.iter().all(|a| a.id != agent_id),
        "archived agent must be removed from agentdesk.yaml"
    );

    // Unarchive (route takes no body).
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/agents/{agent_id}/unarchive"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let post_unarchive_yaml = std::fs::read(&config_path).unwrap();
    assert_eq!(
        post_unarchive_yaml, snapshot_yaml,
        "unarchive must restore agentdesk.yaml byte-for-byte"
    );
    let archive_state: String =
        sqlx::query_scalar("SELECT state FROM agent_archive WHERE agent_id = $1")
            .bind(agent_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(archive_state, "unarchived");

    pool.close().await;
    pg_db.drop().await;
}

/// DoD scenario 4: duplicate must strip every sensitive field caller might
/// try to inject; the new agent gets the new id + new channel and never
/// inherits the source channel or any caller-supplied `system_prompt`.
/// PG-fixture migration of the original SQLite-only test.
#[tokio::test]
async fn wizard_pg_duplicate_strips_sensitive_fields() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    seed_runtime_root(runtime_root.path());

    let pg_db = WizardPgDatabase::create().await;
    let pool = pg_db.migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(pool.clone());
    let app = build_app_with_pg(db.clone(), engine, pool.clone());
    let source_id = "wiz-source";
    let source_channel = "1473922824350601304";
    let new_id = "wiz-source-copy";
    let new_channel = "1473922824350601305";

    let (status, _) = post_json(
        &app,
        "/agents/setup",
        serde_json::json!({
            "agent_id": source_id,
            "channel_id": source_channel,
            "provider": "codex",
            "prompt_template_path": "config/agents/_shared.prompt.md",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    // Give the source a distinctive prompt so we can prove it was copied,
    // and prove the body-supplied "leaked personality" was not.
    std::fs::write(
        runtime_root
            .path()
            .join("config/agents")
            .join(source_id)
            .join("IDENTITY.md"),
        "source identity body\n",
    )
    .unwrap();

    // Duplicate body intentionally embeds five sensitive fields the
    // allow-listed struct does not deserialize.
    let (status, _) = post_json(
        &app,
        &format!("/agents/{source_id}/duplicate"),
        serde_json::json!({
            "new_agent_id": new_id,
            "channel_id": new_channel,
            "name": "Wizard Copy",
            // Forbidden injections — must all be ignored:
            "id": "attacker-override",
            "agent_id": "attacker-override",
            "discord_channel_id": source_channel,
            "token": "secret-token",
            "api_key": "secret-key",
            "system_prompt": "leaked personality",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // The new agent uses the *new* id + *new* channel, source values do not
    // bleed through.
    let (copied_id, ch_primary, ch_alt, ch_cc, ch_cdx, system_prompt): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT id, discord_channel_id, discord_channel_alt, discord_channel_cc,
                discord_channel_cdx, system_prompt
         FROM agents WHERE id = $1",
    )
    .bind(new_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(copied_id, new_id);
    let all_channels = [&ch_primary, &ch_alt, &ch_cc, &ch_cdx];
    assert!(
        all_channels
            .iter()
            .any(|c| c.as_deref() == Some(new_channel)),
        "new_channel must populate one of the four channel columns: {all_channels:?}"
    );
    assert!(
        all_channels
            .iter()
            .all(|c| c.as_deref() != Some(source_channel)),
        "source_channel must NOT leak into any channel column: {all_channels:?}"
    );
    assert!(
        system_prompt.as_deref() != Some("leaked personality"),
        "system_prompt from body must NOT be carried into duplicate: {system_prompt:?}"
    );

    // Attacker-override id must not have created a row.
    let attacker_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agents WHERE id = 'attacker-override'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attacker_rows, 0);

    // Prompt was copied from the source's IDENTITY.md — not the body's
    // `system_prompt`.
    let copied_prompt = std::fs::read_to_string(
        runtime_root
            .path()
            .join("config/agents")
            .join(new_id)
            .join("IDENTITY.md"),
    )
    .unwrap();
    assert_eq!(copied_prompt, "source identity body\n");

    pool.close().await;
    pg_db.drop().await;
}
