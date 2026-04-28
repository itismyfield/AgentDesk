use std::collections::BTreeSet;
use std::str::FromStr;
use std::time::Duration;

use sqlx::Connection;
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgConnection, PgPool, Row};

use crate::config::{AgentChannel, AgentDef, Config};
use crate::server::routes::settings::{KvSeedAction, config_default_seed_actions};

static POSTGRES_MIGRATOR: Migrator = sqlx::migrate!("./migrations/postgres");
const LEGACY_AGENT_PREFIX: &str = "openclaw-";
const DEFAULT_PG_ACQUIRE_TIMEOUT_SECS: u64 = 3;
const STARTUP_PG_ACQUIRE_TIMEOUT_SECS: u64 = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PoolConnectSettings {
    max_connections: u32,
    acquire_timeout: Duration,
}

/// Session-scoped PostgreSQL advisory lock lease.
///
/// The lock remains held for the lifetime of the dedicated connection.
/// Dropping the lease releases the lock implicitly; callers may also call
/// `unlock()` for an explicit release.
pub struct AdvisoryLockLease {
    conn: PgConnection,
    lock_id: i64,
    label: String,
}

impl AdvisoryLockLease {
    pub async fn try_acquire(
        pool: &PgPool,
        lock_id: i64,
        label: impl Into<String>,
    ) -> Result<Option<Self>, String> {
        let label = label.into();
        let options = (*pool.connect_options()).clone();
        let mut conn = PgConnection::connect_with(&options)
            .await
            .map_err(|error| format!("{label} acquire advisory lock connection: {error}"))?;
        let acquired = sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
            .bind(lock_id)
            .fetch_one(&mut conn)
            .await
            .map_err(|error| format!("{label} try advisory lock: {error}"))?;
        if acquired {
            Ok(Some(Self {
                conn,
                lock_id,
                label,
            }))
        } else {
            Ok(None)
        }
    }

    pub async fn keepalive(&mut self) -> Result<(), String> {
        sqlx::query("SELECT 1")
            .execute(&mut self.conn)
            .await
            .map(|_| ())
            .map_err(|error| format!("{} advisory lock keepalive: {error}", self.label))
    }

    pub async fn unlock(mut self) -> Result<(), String> {
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(self.lock_id)
            .execute(&mut self.conn)
            .await
            .map(|_| ())
            .map_err(|error| format!("{} advisory unlock {}: {error}", self.label, self.lock_id))
    }
}

pub fn database_enabled(config: &Config) -> bool {
    config.database.enabled
}

pub fn database_summary(config: &Config) -> String {
    if !database_enabled(config) {
        return "disabled".to_string();
    }
    if database_url_override().is_some() {
        return "env:DATABASE_URL".to_string();
    }
    config_database_summary(config)
}

fn config_database_summary(config: &Config) -> String {
    format!(
        "{}:{}/{} user={} pool_max={}",
        config.database.host,
        config.database.port,
        config.database.dbname,
        config.database.user,
        config.database.pool_max.max(1)
    )
}

pub fn connect_options(config: &Config) -> Result<Option<PgConnectOptions>, String> {
    if !database_enabled(config) {
        return Ok(None);
    }

    if let Some(url) = database_url_override() {
        return PgConnectOptions::from_str(&url)
            .map(Some)
            .map_err(|error| format!("parse DATABASE_URL: {error}"));
    }

    let mut options = PgConnectOptions::new()
        .host(&config.database.host)
        .port(config.database.port)
        .username(&config.database.user)
        .database(&config.database.dbname);
    if let Some(password) = config.database.password.as_deref() {
        options = options.password(password);
    }
    Ok(Some(options))
}

fn runtime_pool_settings(config: &Config) -> PoolConnectSettings {
    PoolConnectSettings {
        max_connections: config.database.pool_max.max(1),
        acquire_timeout: Duration::from_secs(DEFAULT_PG_ACQUIRE_TIMEOUT_SECS),
    }
}

fn startup_pool_settings(config: &Config) -> PoolConnectSettings {
    let steady_max = config.database.pool_max.max(1);
    PoolConnectSettings {
        max_connections: steady_max.saturating_mul(3).div_ceil(2).max(2),
        acquire_timeout: Duration::from_secs(STARTUP_PG_ACQUIRE_TIMEOUT_SECS),
    }
}

async fn connect_with_settings(
    config: &Config,
    settings: PoolConnectSettings,
    context: &str,
) -> Result<Option<PgPool>, String> {
    let Some(options) = connect_options(config)? else {
        return Ok(None);
    };

    let pool = PgPoolOptions::new()
        .max_connections(settings.max_connections)
        .acquire_timeout(settings.acquire_timeout)
        .connect_with(options)
        .await
        .map_err(|error| format!("{context}: {error}"))?;

    health_check(&pool).await?;
    Ok(Some(pool))
}

pub async fn connect(config: &Config) -> Result<Option<PgPool>, String> {
    connect_with_settings(config, runtime_pool_settings(config), "connect postgres").await
}

pub async fn connect_for_startup(config: &Config) -> Result<Option<PgPool>, String> {
    let settings = startup_pool_settings(config);
    let pool =
        connect_with_settings(config, settings, "connect postgres startup warmup pool").await?;
    if pool.is_some() {
        tracing::info!(
            "[startup] postgres warmup pool ready (max_connections={}, acquire_timeout={}s)",
            settings.max_connections,
            settings.acquire_timeout.as_secs()
        );
    }
    Ok(pool)
}

pub async fn connect_and_migrate(config: &Config) -> Result<Option<PgPool>, String> {
    let Some(pool) = connect(config).await? else {
        return Ok(None);
    };
    migrate(&pool).await?;
    Ok(Some(pool))
}

pub async fn migrate(pool: &PgPool) -> Result<(), String> {
    POSTGRES_MIGRATOR
        .run(pool)
        .await
        .map_err(|error| format!("run postgres migrations: {error}"))?;
    Ok(())
}

pub async fn startup_reseed(pool: &PgPool, config: &Config) -> Result<(), String> {
    apply_kv_seed_actions(pool, &config_default_seed_actions(config)).await?;
    upsert_kv_meta(pool, "server_port", &config.server.port.to_string()).await?;
    crate::services::settings::seed_runtime_config_defaults_pg(pool, config).await?;
    crate::server::routes::escalation::seed_escalation_defaults_pg(pool, config).await?;

    for repo_id in normalized_repo_ids(&config.github.repos) {
        register_repo(pool, &repo_id).await?;
    }

    sync_agents_from_config_pg(pool, &config.agents).await?;
    Ok(())
}

pub async fn health_check(pool: &PgPool) -> Result<(), String> {
    sqlx::query("SELECT 1")
        .fetch_one(pool)
        .await
        .map(|_| ())
        .map_err(|error| format!("postgres health check failed: {error}"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedMigrationInfo {
    pub version: i64,
    pub description: String,
    pub success: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationStatus {
    pub applied: Vec<AppliedMigrationInfo>,
    pub resolved_versions: Vec<i64>,
    pub missing_from_resolved: Vec<i64>,
    pub pending_versions: Vec<i64>,
}

pub async fn query_applied_migrations(pool: &PgPool) -> Result<Vec<AppliedMigrationInfo>, String> {
    let rows = sqlx::query(
        "SELECT version, description, success
         FROM _sqlx_migrations
         ORDER BY version",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("query _sqlx_migrations: {error}"))?;
    Ok(rows
        .into_iter()
        .map(|row| AppliedMigrationInfo {
            version: row.get::<i64, _>("version"),
            description: row.get::<String, _>("description"),
            success: row.get::<bool, _>("success"),
        })
        .collect())
}

pub async fn migration_status(pool: &PgPool) -> Result<MigrationStatus, String> {
    let applied = query_applied_migrations(pool).await?;
    let resolved_versions = POSTGRES_MIGRATOR
        .iter()
        .map(|migration| migration.version)
        .collect::<Vec<_>>();
    let resolved_set = resolved_versions.iter().copied().collect::<BTreeSet<_>>();
    let applied_set = applied
        .iter()
        .filter(|migration| migration.success)
        .map(|migration| migration.version)
        .collect::<BTreeSet<_>>();
    let missing_from_resolved = applied_set
        .difference(&resolved_set)
        .copied()
        .collect::<Vec<_>>();
    let pending_versions = resolved_set
        .difference(&applied_set)
        .copied()
        .collect::<Vec<_>>();
    Ok(MigrationStatus {
        applied,
        resolved_versions,
        missing_from_resolved,
        pending_versions,
    })
}

async fn apply_kv_seed_actions(pool: &PgPool, actions: &[KvSeedAction]) -> Result<(), String> {
    for action in actions {
        match action {
            KvSeedAction::Put { key, value } => {
                upsert_kv_meta(pool, key, value).await?;
            }
            KvSeedAction::PutIfAbsent { key, value } => {
                sqlx::query(
                    "INSERT INTO kv_meta (key, value)
                     VALUES ($1, $2)
                     ON CONFLICT (key) DO NOTHING",
                )
                .bind(key)
                .bind(value)
                .execute(pool)
                .await
                .map_err(|error| format!("seed kv_meta {key}: {error}"))?;
            }
            KvSeedAction::Delete { key } => {
                sqlx::query("DELETE FROM kv_meta WHERE key = $1")
                    .bind(key)
                    .execute(pool)
                    .await
                    .map_err(|error| format!("delete retired kv_meta {key}: {error}"))?;
            }
        }
    }
    Ok(())
}

async fn upsert_kv_meta(pool: &PgPool, key: &str, value: &str) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO kv_meta (key, value)
         VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE
         SET value = EXCLUDED.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await
    .map_err(|error| format!("upsert kv_meta {key}: {error}"))?;
    Ok(())
}

fn normalized_repo_ids(repo_ids: &[String]) -> Vec<String> {
    let mut deduped = BTreeSet::new();
    for raw_repo_id in repo_ids {
        let repo_id = raw_repo_id.trim();
        if repo_id.is_empty() {
            continue;
        }
        if !repo_id.contains('/') {
            tracing::warn!(
                "[startup] skipping invalid github.repos entry {:?}: expected owner/repo",
                raw_repo_id
            );
            continue;
        }
        deduped.insert(repo_id.to_string());
    }
    deduped.into_iter().collect()
}

pub async fn register_repo(pool: &PgPool, repo_id: &str) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO github_repos (id, display_name, sync_enabled)
         VALUES ($1, $1, TRUE)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(repo_id)
    .execute(pool)
    .await
    .map_err(|error| format!("insert github_repos {repo_id}: {error}"))?;

    Ok(())
}

pub async fn sync_agents_from_config_pg(
    pool: &PgPool,
    agents: &[AgentDef],
) -> Result<usize, String> {
    let config_ids: BTreeSet<&str> = agents.iter().map(|agent| agent.id.as_str()).collect();

    for agent in agents {
        upsert_agent_from_config_pg(pool, agent).await?;
    }

    migrate_legacy_agent_aliases_pg(pool, agents, &config_ids).await?;

    let existing_rows = sqlx::query("SELECT id FROM agents")
        .fetch_all(pool)
        .await
        .map_err(|error| format!("list postgres agents: {error}"))?;

    for row in existing_rows {
        let db_id: String = row
            .try_get("id")
            .map_err(|error| format!("read postgres agent id: {error}"))?;
        if !config_ids.contains(db_id.as_str()) {
            clear_agent_fk_references_pg(pool, &db_id).await?;
            sqlx::query("DELETE FROM agents WHERE id = $1")
                .bind(&db_id)
                .execute(pool)
                .await
                .map_err(|error| format!("delete postgres agent {db_id}: {error}"))?;
        }
    }

    Ok(agents.len())
}

/// Clear FK references to `agent_id` before deleting the agent row, so the
/// `kanban_cards.assigned_agent_id` (and similar nullable FK columns) do not
/// trip a NO ACTION constraint when the agent is being removed from config.
/// Mirrors the column list of `move_legacy_agent_references_pg` minus the
/// non-nullable FKs (which are exclusively populated via dispatch creation
/// flows that retire alongside the agent).
async fn clear_agent_fk_references_pg(pool: &PgPool, agent_id: &str) -> Result<(), String> {
    for sql in [
        "UPDATE github_repos SET default_agent_id = NULL WHERE default_agent_id = $1",
        "UPDATE pipeline_stages SET agent_override_id = NULL WHERE agent_override_id = $1",
        "UPDATE kanban_cards SET assigned_agent_id = NULL WHERE assigned_agent_id = $1",
        "UPDATE kanban_cards SET owner_agent_id = NULL WHERE owner_agent_id = $1",
        "UPDATE kanban_cards SET requester_agent_id = NULL WHERE requester_agent_id = $1",
    ] {
        sqlx::query(sql)
            .bind(agent_id)
            .execute(pool)
            .await
            .map_err(|error| format!("clear FK refs to agent {agent_id}: {error}"))?;
    }
    Ok(())
}

fn legacy_agent_alias(agent_id: &str) -> Option<String> {
    if agent_id.starts_with(LEGACY_AGENT_PREFIX) {
        return None;
    }
    Some(format!("{LEGACY_AGENT_PREFIX}{agent_id}"))
}

async fn postgres_agent_exists(pool: &PgPool, agent_id: &str) -> Result<bool, String> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agents WHERE id = $1)")
        .bind(agent_id)
        .fetch_one(pool)
        .await
        .map_err(|error| format!("check postgres agent {agent_id}: {error}"))
}

async fn upsert_agent_from_config_pg(pool: &PgPool, agent: &AgentDef) -> Result<(), String> {
    let discord_channel_cc = agent
        .channels
        .claude
        .as_ref()
        .and_then(AgentChannel::target);
    let discord_channel_cdx = agent.channels.codex.as_ref().and_then(AgentChannel::target);
    let provider_primary = match agent.provider.as_str() {
        "gemini" => agent
            .channels
            .gemini
            .as_ref()
            .and_then(AgentChannel::target),
        "opencode" => agent
            .channels
            .opencode
            .as_ref()
            .and_then(AgentChannel::target),
        "qwen" => agent.channels.qwen.as_ref().and_then(AgentChannel::target),
        _ => None,
    };
    let discord_channel_id = provider_primary.or_else(|| discord_channel_cc.clone());
    let discord_channel_alt = discord_channel_cdx.clone();

    sqlx::query(
        "INSERT INTO agents (
            id, name, name_ko, provider, department, avatar_emoji,
            discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (id) DO UPDATE
         SET name = EXCLUDED.name,
             name_ko = EXCLUDED.name_ko,
             provider = EXCLUDED.provider,
             department = EXCLUDED.department,
             avatar_emoji = EXCLUDED.avatar_emoji,
             discord_channel_id = EXCLUDED.discord_channel_id,
             discord_channel_alt = EXCLUDED.discord_channel_alt,
             discord_channel_cc = EXCLUDED.discord_channel_cc,
             discord_channel_cdx = EXCLUDED.discord_channel_cdx",
    )
    .bind(&agent.id)
    .bind(&agent.name)
    .bind(&agent.name_ko)
    .bind(&agent.provider)
    .bind(&agent.department)
    .bind(&agent.avatar_emoji)
    .bind(discord_channel_id)
    .bind(discord_channel_alt)
    .bind(discord_channel_cc)
    .bind(discord_channel_cdx)
    .execute(pool)
    .await
    .map_err(|error| format!("upsert postgres agent {}: {error}", agent.id))?;

    Ok(())
}

async fn migrate_legacy_agent_aliases_pg(
    pool: &PgPool,
    agents: &[AgentDef],
    config_ids: &BTreeSet<&str>,
) -> Result<(), String> {
    for agent in agents {
        let Some(legacy_id) = legacy_agent_alias(&agent.id) else {
            continue;
        };
        if config_ids.contains(legacy_id.as_str()) {
            tracing::info!(
                "[agent-sync] Preserving configured legacy agent '{}' while syncing '{}'",
                legacy_id,
                agent.id
            );
            continue;
        }

        if postgres_agent_exists(pool, &legacy_id).await? {
            copy_runtime_fields_from_legacy_pg(pool, &legacy_id, &agent.id).await?;
        }
        move_legacy_agent_references_pg(pool, &legacy_id, &agent.id).await?;

        if postgres_agent_exists(pool, &legacy_id).await? {
            sqlx::query("DELETE FROM agents WHERE id = $1")
                .bind(&legacy_id)
                .execute(pool)
                .await
                .map_err(|error| format!("delete postgres legacy agent {legacy_id}: {error}"))?;
            tracing::info!(
                "[agent-sync] Migrated legacy agent '{}' -> '{}'",
                legacy_id,
                agent.id
            );
        }
    }

    Ok(())
}

async fn copy_runtime_fields_from_legacy_pg(
    pool: &PgPool,
    legacy_id: &str,
    canonical_id: &str,
) -> Result<(), String> {
    if !postgres_agent_exists(pool, legacy_id).await?
        || !postgres_agent_exists(pool, canonical_id).await?
    {
        return Ok(());
    }

    sqlx::query(
        "UPDATE agents
         SET status = legacy.status,
             xp = legacy.xp,
             skills = legacy.skills,
             created_at = COALESCE(legacy.created_at, agents.created_at),
             sprite_number = COALESCE(legacy.sprite_number, agents.sprite_number),
             description = COALESCE(legacy.description, agents.description),
             system_prompt = COALESCE(legacy.system_prompt, agents.system_prompt),
             pipeline_config = COALESCE(legacy.pipeline_config, agents.pipeline_config)
         FROM agents AS legacy
         WHERE legacy.id = $1
           AND agents.id = $2",
    )
    .bind(legacy_id)
    .bind(canonical_id)
    .execute(pool)
    .await
    .map_err(|error| {
        format!("copy postgres legacy runtime fields {legacy_id} -> {canonical_id}: {error}")
    })?;

    Ok(())
}

async fn move_legacy_agent_references_pg(
    pool: &PgPool,
    legacy_id: &str,
    canonical_id: &str,
) -> Result<(), String> {
    for sql in [
        "UPDATE github_repos SET default_agent_id = $1 WHERE default_agent_id = $2",
        "UPDATE pipeline_stages SET agent_override_id = $1 WHERE agent_override_id = $2",
        "UPDATE kanban_cards SET assigned_agent_id = $1 WHERE assigned_agent_id = $2",
        "UPDATE kanban_cards SET owner_agent_id = $1 WHERE owner_agent_id = $2",
        "UPDATE kanban_cards SET requester_agent_id = $1 WHERE requester_agent_id = $2",
        "UPDATE task_dispatches SET from_agent_id = $1 WHERE from_agent_id = $2",
        "UPDATE task_dispatches SET to_agent_id = $1 WHERE to_agent_id = $2",
        "UPDATE sessions SET agent_id = $1 WHERE agent_id = $2",
        "UPDATE meeting_transcripts SET speaker_agent_id = $1 WHERE speaker_agent_id = $2",
        "UPDATE skill_usage SET agent_id = $1 WHERE agent_id = $2",
        "UPDATE turns SET agent_id = $1 WHERE agent_id = $2",
        "UPDATE dispatch_outbox SET agent_id = $1 WHERE agent_id = $2",
        "UPDATE auto_queue_runs SET agent_id = $1 WHERE agent_id = $2",
        "UPDATE auto_queue_entries SET agent_id = $1 WHERE agent_id = $2",
        "UPDATE api_friction_events SET agent_id = $1 WHERE agent_id = $2",
        "UPDATE session_transcripts SET agent_id = $1 WHERE agent_id = $2",
        "UPDATE memento_feedback_turn_stats SET agent_id = $1 WHERE agent_id = $2",
    ] {
        sqlx::query(sql)
            .bind(canonical_id)
            .bind(legacy_id)
            .execute(pool)
            .await
            .map_err(|error| {
                format!("rewrite postgres legacy references {legacy_id} -> {canonical_id}: {error}")
            })?;
    }

    sqlx::query(
        "INSERT INTO office_agents (office_id, agent_id, department_id, joined_at)
         SELECT office_id, $1, department_id, joined_at
           FROM office_agents
          WHERE agent_id = $2
         ON CONFLICT (office_id, agent_id) DO NOTHING",
    )
    .bind(canonical_id)
    .bind(legacy_id)
    .execute(pool)
    .await
    .map_err(|error| {
        format!("upsert postgres office_agents {legacy_id} -> {canonical_id}: {error}")
    })?;
    sqlx::query("DELETE FROM office_agents WHERE agent_id = $1")
        .bind(legacy_id)
        .execute(pool)
        .await
        .map_err(|error| format!("delete postgres office_agents {legacy_id}: {error}"))?;

    sqlx::query(
        "INSERT INTO auto_queue_slots (
            agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map, created_at, updated_at
         )
         SELECT $1, slot_index, assigned_run_id, assigned_thread_group, thread_id_map, created_at, updated_at
           FROM auto_queue_slots
          WHERE agent_id = $2
         ON CONFLICT (agent_id, slot_index) DO NOTHING",
    )
    .bind(canonical_id)
    .bind(legacy_id)
    .execute(pool)
    .await
    .map_err(|error| {
        format!("upsert postgres auto_queue_slots {legacy_id} -> {canonical_id}: {error}")
    })?;
    sqlx::query("DELETE FROM auto_queue_slots WHERE agent_id = $1")
        .bind(legacy_id)
        .execute(pool)
        .await
        .map_err(|error| format!("delete postgres auto_queue_slots {legacy_id}: {error}"))?;

    Ok(())
}

fn database_url_override() -> Option<String> {
    std::env::var("DATABASE_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
const TEST_POSTGRES_OP_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(test)]
const TEST_POSTGRES_POOL_MAX_CONNECTIONS: u32 = 1;
#[cfg(test)]
const TEST_POSTGRES_ADMIN_POOL_MAX_CONNECTIONS: u32 = 1;
#[cfg(test)]
static POSTGRES_TEST_SETUP_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
    std::sync::OnceLock::new();
#[cfg(test)]
static POSTGRES_TEST_LIFECYCLE_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
    std::sync::OnceLock::new();

#[cfg(test)]
fn lock_test_setup() -> std::sync::MutexGuard<'static, ()> {
    let mutex = POSTGRES_TEST_SETUP_LOCK.get_or_init(|| std::sync::Mutex::new(()));
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
fn lock_test_lifecycle_raw() -> std::sync::MutexGuard<'static, ()> {
    let mutex = POSTGRES_TEST_LIFECYCLE_LOCK.get_or_init(|| std::sync::Mutex::new(()));
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) struct PostgresTestLifecycleGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
pub(crate) fn lock_test_lifecycle() -> PostgresTestLifecycleGuard {
    PostgresTestLifecycleGuard {
        _guard: lock_test_lifecycle_raw(),
    }
}

#[cfg(test)]
async fn run_test_postgres_sqlx_op_with_timeout<T, F>(
    timeout: Duration,
    label: &str,
    future: F,
) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, sqlx::Error>>,
{
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| format!("{label} timed out after {}s", timeout.as_secs()))?
        .map_err(|error| format!("{label}: {error}"))
}

#[cfg(test)]
async fn run_test_postgres_sqlx_op<T, F>(label: &str, future: F) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, sqlx::Error>>,
{
    run_test_postgres_sqlx_op_with_timeout(TEST_POSTGRES_OP_TIMEOUT, label, future).await
}

#[cfg(test)]
async fn connect_test_pool_with_max_connections(
    database_url: &str,
    label: &str,
    max_connections: u32,
) -> Result<PgPool, String> {
    let options = PgConnectOptions::from_str(database_url)
        .map_err(|error| format!("{label} parse postgres url: {error}"))?;
    run_test_postgres_sqlx_op(
        &format!("{label} connect postgres"),
        PgPoolOptions::new()
            .max_connections(max_connections.max(1))
            .acquire_timeout(TEST_POSTGRES_OP_TIMEOUT)
            .connect_with(options),
    )
    .await
}

#[cfg(test)]
pub(crate) async fn connect_test_pool(database_url: &str, label: &str) -> Result<PgPool, String> {
    // Test helpers frequently create many isolated pools in parallel on CI.
    // Keep the default test pool lean so PG-backed route tests do not exhaust
    // the shared runner database just by setting up fixtures.
    connect_test_pool_with_max_connections(database_url, label, TEST_POSTGRES_POOL_MAX_CONNECTIONS)
        .await
}

#[cfg(test)]
pub(crate) async fn create_test_database(
    admin_url: &str,
    database_name: &str,
    label: &str,
) -> Result<(), String> {
    // CI failures were caused by many PG-backed tests racing to create/drop
    // isolated databases at the same time. Serialize setup/teardown at the
    // shared helper boundary so every test module benefits from the guard.
    let _guard = lock_test_setup();
    let admin_pool = connect_test_pool_with_max_connections(
        admin_url,
        &format!("{label} admin"),
        TEST_POSTGRES_ADMIN_POOL_MAX_CONNECTIONS,
    )
    .await?;
    run_test_postgres_sqlx_op(
        &format!("{label} create postgres test db {database_name}"),
        sqlx::query(&format!("CREATE DATABASE \"{database_name}\"")).execute(&admin_pool),
    )
    .await?;
    close_test_pool(admin_pool, &format!("{label} admin")).await?;
    Ok(())
}

#[cfg(test)]
pub(crate) async fn connect_test_pool_and_migrate(
    database_url: &str,
    label: &str,
) -> Result<PgPool, String> {
    let pool = connect_test_pool(database_url, label).await?;
    tokio::time::timeout(TEST_POSTGRES_OP_TIMEOUT, migrate(&pool))
        .await
        .map_err(|_| {
            format!(
                "{label} migrate postgres timed out after {}s",
                TEST_POSTGRES_OP_TIMEOUT.as_secs()
            )
        })??;
    Ok(pool)
}

#[cfg(test)]
pub(crate) async fn drop_test_database(
    admin_url: &str,
    database_name: &str,
    label: &str,
) -> Result<(), String> {
    let _guard = lock_test_setup();
    let admin_pool = connect_test_pool_with_max_connections(
        admin_url,
        &format!("{label} admin"),
        TEST_POSTGRES_ADMIN_POOL_MAX_CONNECTIONS,
    )
    .await?;
    run_test_postgres_sqlx_op(
        &format!("{label} terminate postgres test db sessions {database_name}"),
        sqlx::query(
            "SELECT pg_terminate_backend(pid)
             FROM pg_stat_activity
             WHERE datname = $1
               AND pid <> pg_backend_pid()",
        )
        .bind(database_name)
        .execute(&admin_pool),
    )
    .await?;
    run_test_postgres_sqlx_op(
        &format!("{label} drop postgres test db {database_name}"),
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{database_name}\"")).execute(&admin_pool),
    )
    .await?;
    close_test_pool(admin_pool, &format!("{label} admin")).await?;
    Ok(())
}

#[cfg(test)]
pub(crate) async fn close_test_pool(pool: PgPool, label: &str) -> Result<(), String> {
    tokio::time::timeout(TEST_POSTGRES_OP_TIMEOUT, pool.close())
        .await
        .map_err(|_| {
            format!(
                "{label} close postgres pool timed out after {}s",
                TEST_POSTGRES_OP_TIMEOUT.as_secs()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::{
        AdvisoryLockLease, STARTUP_PG_ACQUIRE_TIMEOUT_SECS, close_test_pool,
        config_database_summary, connect_and_migrate, connect_options, create_test_database,
        database_enabled, database_summary, health_check, run_test_postgres_sqlx_op_with_timeout,
        startup_pool_settings, startup_reseed,
    };
    use sqlx::Row;
    use std::time::Duration;

    struct TestDatabase {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestDatabase {
        async fn create() -> Self {
            let admin_url = admin_database_url();
            let database_name = format!("agentdesk_pg_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", base_database_url(), database_name);
            create_test_database(&admin_url, &database_name, "db::postgres tests")
                .await
                .expect("create postgres test db");

            Self {
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn drop(self) {
            super::drop_test_database(&self.admin_url, &self.database_name, "db::postgres tests")
                .await
                .expect("drop postgres test db");
        }
    }

    fn base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", base_database_url(), admin_db)
    }

    fn postgres_test_config(test_db: &TestDatabase) -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config.database.enabled = true;
        config.database.pool_max = 4;
        config.database.host = "localhost".to_string();
        config.database.port = std::env::var("PGPORT")
            .ok()
            .and_then(|raw| raw.parse::<u16>().ok())
            .unwrap_or(5432);
        config.database.dbname = test_db.database_name.clone();
        config.database.user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        config.database.password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        config.github.repos = vec!["itismyfield/AgentDesk".to_string()];
        config.agents = vec![crate::config::AgentDef {
            id: "pg-agent".to_string(),
            name: "PG Agent".to_string(),
            name_ko: Some("피지 에이전트".to_string()),
            provider: "codex".to_string(),
            channels: crate::config::AgentChannels {
                codex: Some(crate::config::AgentChannel::from("pg-agent-cdx")),
                ..Default::default()
            },
            keywords: Vec::new(),
            department: Some("platform".to_string()),
            avatar_emoji: Some(":gear:".to_string()),
        }];
        config
    }

    #[test]
    fn postgres_config_is_disabled_by_default() {
        let config = crate::config::Config::default();
        assert!(!database_enabled(&config));
        assert_eq!(database_summary(&config), "disabled");
        assert!(connect_options(&config).unwrap().is_none());
    }

    #[test]
    fn postgres_summary_uses_config_fields_when_enabled() {
        let mut config = crate::config::Config::default();
        config.database.enabled = true;
        config.database.host = "db.internal".to_string();
        config.database.port = 5433;
        config.database.dbname = "agentdesk_dev".to_string();
        config.database.user = "agentdesk_app".to_string();
        config.database.pool_max = 16;

        assert_eq!(
            config_database_summary(&config),
            "db.internal:5433/agentdesk_dev user=agentdesk_app pool_max=16"
        );
        assert!(connect_options(&config).unwrap().is_some());
    }

    #[test]
    fn startup_pool_settings_raise_pool_size_and_acquire_timeout() {
        let mut config = crate::config::Config::default();
        config.database.enabled = true;
        config.database.pool_max = 5;

        let settings = startup_pool_settings(&config);

        assert_eq!(settings.max_connections, 8);
        assert_eq!(
            settings.acquire_timeout,
            Duration::from_secs(STARTUP_PG_ACQUIRE_TIMEOUT_SECS)
        );
    }

    #[tokio::test]
    async fn postgres_migrations_and_startup_reseed_prepare_empty_database() {
        let test_db = TestDatabase::create().await;
        let config = postgres_test_config(&test_db);

        let pool = connect_and_migrate(&config)
            .await
            .expect("connect and migrate postgres")
            .expect("postgres pool");
        startup_reseed(&pool, &config)
            .await
            .expect("startup reseed postgres");

        let migration_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await
            .expect("count sqlx migrations");
        assert!(migration_count >= 1);

        let server_port: String = sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = $1")
            .bind("server_port")
            .fetch_one(&pool)
            .await
            .expect("server_port in kv_meta");
        assert_eq!(server_port, config.server.port.to_string());

        let runtime_config_raw: String =
            sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = $1")
                .bind("runtime-config")
                .fetch_one(&pool)
                .await
                .expect("runtime-config in kv_meta");
        let runtime_config: serde_json::Value =
            serde_json::from_str(&runtime_config_raw).expect("parse runtime-config json");
        assert_eq!(
            runtime_config
                .get("dispatchPollSec")
                .and_then(|value| value.as_u64()),
            Some(30)
        );

        let repo_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM github_repos WHERE id = $1")
            .bind("itismyfield/AgentDesk")
            .fetch_one(&pool)
            .await
            .expect("count github_repos");
        assert_eq!(repo_count, 1);

        let agent_row = sqlx::query(
            "SELECT id, provider, discord_channel_cdx
             FROM agents
             WHERE id = $1",
        )
        .bind("pg-agent")
        .fetch_one(&pool)
        .await
        .expect("load seeded postgres agent");
        assert_eq!(agent_row.get::<String, _>("provider"), "codex");
        assert_eq!(
            agent_row.get::<Option<String>, _>("discord_channel_cdx"),
            Some("pg-agent-cdx".to_string())
        );

        let escalation_override_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM kv_meta WHERE key = $1")
                .bind("escalation-settings-override")
                .fetch_one(&pool)
                .await
                .expect("count escalation override");
        assert_eq!(escalation_override_count, 0);

        close_test_pool(pool, "db::postgres migration test pool")
            .await
            .expect("close postgres pool");
        assert!(test_db.database_url.contains("agentdesk_pg_"));
        test_db.drop().await;
    }

    #[tokio::test]
    async fn postgres_startup_reseed_migrates_legacy_openclaw_agent_ids() {
        let test_db = TestDatabase::create().await;
        let mut config = postgres_test_config(&test_db);
        config.agents = vec![crate::config::AgentDef {
            id: "maker".to_string(),
            name: "Maker".to_string(),
            name_ko: Some("뚝딱이".to_string()),
            provider: "codex".to_string(),
            channels: crate::config::AgentChannels {
                codex: Some(crate::config::AgentChannel::from("maker-cdx")),
                ..Default::default()
            },
            keywords: Vec::new(),
            department: Some("engineering".to_string()),
            avatar_emoji: Some("🛠️".to_string()),
        }];

        let pool = connect_and_migrate(&config)
            .await
            .expect("connect and migrate postgres")
            .expect("postgres pool");

        sqlx::query(
            "INSERT INTO agents (
                id, name, provider, status, xp, sprite_number, description, system_prompt, pipeline_config
             ) VALUES ($1, 'Legacy Maker', 'codex', 'working', 42, 7, 'legacy-desc', 'legacy-prompt', '{\"k\":1}')",
        )
        .bind("openclaw-maker")
        .execute(&pool)
        .await
        .expect("insert legacy agent");
        sqlx::query("INSERT INTO github_repos (id, default_agent_id) VALUES ($1, $2)")
            .bind("owner/repo")
            .bind("openclaw-maker")
            .execute(&pool)
            .await
            .expect("insert github repo");
        sqlx::query("INSERT INTO kanban_cards (id, title, assigned_agent_id) VALUES ($1, $2, $3)")
            .bind("card-1")
            .bind("Card")
            .bind("openclaw-maker")
            .execute(&pool)
            .await
            .expect("insert card");
        sqlx::query("INSERT INTO sessions (session_key, agent_id, status) VALUES ($1, $2, $3)")
            .bind("sess-1")
            .bind("openclaw-maker")
            .bind("working")
            .execute(&pool)
            .await
            .expect("insert session");
        sqlx::query(
            "INSERT INTO office_agents (office_id, agent_id, department_id) VALUES ($1, $2, $3)",
        )
        .bind("office-1")
        .bind("openclaw-maker")
        .bind("engineering")
        .execute(&pool)
        .await
        .expect("insert office agent");

        startup_reseed(&pool, &config)
            .await
            .expect("startup reseed postgres");

        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM agents ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("list agent ids");
        assert_eq!(ids, vec!["maker".to_string()]);

        let (status, xp, sprite_number, description, system_prompt): (
            String,
            i64,
            Option<i64>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT status, xp, sprite_number, description, system_prompt
               FROM agents
              WHERE id = $1",
        )
        .bind("maker")
        .fetch_one(&pool)
        .await
        .expect("load canonical agent");
        assert_eq!(status, "working");
        assert_eq!(xp, 42);
        assert_eq!(sprite_number, Some(7));
        assert_eq!(description.as_deref(), Some("legacy-desc"));
        assert_eq!(system_prompt.as_deref(), Some("legacy-prompt"));

        let github_default: Option<String> =
            sqlx::query_scalar("SELECT default_agent_id FROM github_repos WHERE id = $1")
                .bind("owner/repo")
                .fetch_one(&pool)
                .await
                .expect("load github repo default agent");
        assert_eq!(github_default.as_deref(), Some("maker"));

        let card_agent: Option<String> =
            sqlx::query_scalar("SELECT assigned_agent_id FROM kanban_cards WHERE id = $1")
                .bind("card-1")
                .fetch_one(&pool)
                .await
                .expect("load card agent");
        assert_eq!(card_agent.as_deref(), Some("maker"));

        let session_agent: Option<String> =
            sqlx::query_scalar("SELECT agent_id FROM sessions WHERE session_key = $1")
                .bind("sess-1")
                .fetch_one(&pool)
                .await
                .expect("load session agent");
        assert_eq!(session_agent.as_deref(), Some("maker"));

        let office_agent: String =
            sqlx::query_scalar("SELECT agent_id FROM office_agents WHERE office_id = $1")
                .bind("office-1")
                .fetch_one(&pool)
                .await
                .expect("load office agent");
        assert_eq!(office_agent, "maker");

        close_test_pool(pool, "db::postgres legacy reseed test pool")
            .await
            .expect("close postgres pool");
        test_db.drop().await;
    }

    #[tokio::test]
    async fn postgres_startup_reseed_preserves_configured_legacy_openclaw_agent_id() {
        let test_db = TestDatabase::create().await;
        let mut config = postgres_test_config(&test_db);
        config.agents = vec![
            crate::config::AgentDef {
                id: "maker".to_string(),
                name: "Maker".to_string(),
                name_ko: None,
                provider: "codex".to_string(),
                channels: crate::config::AgentChannels {
                    codex: Some(crate::config::AgentChannel::from("maker-cdx")),
                    ..Default::default()
                },
                keywords: Vec::new(),
                department: Some("engineering".to_string()),
                avatar_emoji: None,
            },
            crate::config::AgentDef {
                id: "openclaw-maker".to_string(),
                name: "Legacy Maker".to_string(),
                name_ko: None,
                provider: "codex".to_string(),
                channels: crate::config::AgentChannels {
                    codex: Some(crate::config::AgentChannel::from("legacy-cdx")),
                    ..Default::default()
                },
                keywords: Vec::new(),
                department: Some("legacy".to_string()),
                avatar_emoji: None,
            },
        ];

        let pool = connect_and_migrate(&config)
            .await
            .expect("connect and migrate postgres")
            .expect("postgres pool");

        sqlx::query(
            "INSERT INTO agents (id, name, provider, status, xp)
             VALUES ($1, 'Configured Legacy Maker', 'codex', 'working', 42)
             ON CONFLICT (id) DO UPDATE SET
                 name = EXCLUDED.name,
                 provider = EXCLUDED.provider,
                 status = EXCLUDED.status,
                 xp = EXCLUDED.xp",
        )
        .bind("openclaw-maker")
        .execute(&pool)
        .await
        .expect("insert configured legacy agent");
        sqlx::query(
            "INSERT INTO github_repos (id, default_agent_id) VALUES ($1, $2)
             ON CONFLICT (id) DO UPDATE SET default_agent_id = EXCLUDED.default_agent_id",
        )
        .bind("owner/repo")
        .bind("openclaw-maker")
        .execute(&pool)
        .await
        .expect("insert github repo");

        startup_reseed(&pool, &config)
            .await
            .expect("startup reseed postgres");

        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM agents ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("list agent ids");
        assert_eq!(ids, vec!["maker".to_string(), "openclaw-maker".to_string()]);

        let (status, xp): (String, i64) =
            sqlx::query_as("SELECT status, xp FROM agents WHERE id = $1")
                .bind("openclaw-maker")
                .fetch_one(&pool)
                .await
                .expect("load configured legacy agent");
        assert_eq!(status, "working");
        assert_eq!(xp, 42);

        let github_default: Option<String> =
            sqlx::query_scalar("SELECT default_agent_id FROM github_repos WHERE id = $1")
                .bind("owner/repo")
                .fetch_one(&pool)
                .await
                .expect("load github repo default agent");
        assert_eq!(github_default.as_deref(), Some("openclaw-maker"));

        close_test_pool(pool, "db::postgres configured legacy reseed test pool")
            .await
            .expect("close postgres pool");
        test_db.drop().await;
    }

    #[tokio::test]
    async fn postgres_advisory_lock_lease_allows_only_one_live_holder() {
        let test_db = TestDatabase::create().await;
        let config = postgres_test_config(&test_db);

        let pool = connect_and_migrate(&config)
            .await
            .expect("connect and migrate postgres")
            .expect("postgres pool");

        let mut first = AdvisoryLockLease::try_acquire(&pool, 91_001, "test advisory lock")
            .await
            .expect("acquire first advisory lock")
            .expect("first advisory lock holder");
        first
            .keepalive()
            .await
            .expect("first advisory lock keepalive");

        let second = AdvisoryLockLease::try_acquire(&pool, 91_001, "test advisory lock")
            .await
            .expect("acquire second advisory lock");
        assert!(second.is_none(), "second holder must be denied");

        first.unlock().await.expect("unlock first advisory lock");

        let third = AdvisoryLockLease::try_acquire(&pool, 91_001, "test advisory lock")
            .await
            .expect("acquire third advisory lock")
            .expect("third advisory lock holder after unlock");
        third.unlock().await.expect("unlock third advisory lock");

        close_test_pool(pool, "db::postgres advisory lock test pool")
            .await
            .expect("close postgres pool");
        test_db.drop().await;
    }

    #[tokio::test]
    async fn postgres_advisory_lock_lease_does_not_exhaust_shared_pool() {
        let test_db = TestDatabase::create().await;
        let mut config = postgres_test_config(&test_db);
        config.database.pool_max = 1;

        let pool = connect_and_migrate(&config)
            .await
            .expect("connect and migrate postgres")
            .expect("postgres pool");

        let lease = AdvisoryLockLease::try_acquire(&pool, 91_003, "test advisory lock")
            .await
            .expect("acquire advisory lock")
            .expect("advisory lock holder");

        health_check(&pool)
            .await
            .expect("shared pool remains usable while advisory lock is held");

        lease.unlock().await.expect("unlock advisory lock");
        close_test_pool(pool, "db::postgres advisory pool exhaustion test")
            .await
            .expect("close postgres pool");
        test_db.drop().await;
    }

    #[tokio::test]
    async fn postgres_advisory_lock_lease_releases_on_drop_across_separate_pools() {
        let test_db = TestDatabase::create().await;
        let config = postgres_test_config(&test_db);

        let pool_a = connect_and_migrate(&config)
            .await
            .expect("connect and migrate postgres pool A")
            .expect("postgres pool A");
        let pool_b = connect_and_migrate(&config)
            .await
            .expect("connect and migrate postgres pool B")
            .expect("postgres pool B");

        let holder_a = AdvisoryLockLease::try_acquire(&pool_a, 91_002, "test advisory lock")
            .await
            .expect("acquire advisory lock on pool A")
            .expect("pool A advisory lock holder");

        let denied_b = AdvisoryLockLease::try_acquire(&pool_b, 91_002, "test advisory lock")
            .await
            .expect("attempt advisory lock on pool B");
        assert!(
            denied_b.is_none(),
            "pool B must be fenced while pool A holds lock"
        );

        drop(holder_a);

        let holder_b = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(holder) =
                    AdvisoryLockLease::try_acquire(&pool_b, 91_002, "test advisory lock")
                        .await
                        .expect("acquire advisory lock on pool B after drop")
                {
                    break holder;
                }

                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("pool B advisory lock holder after drop");
        holder_b
            .unlock()
            .await
            .expect("unlock advisory lock on pool B");

        close_test_pool(pool_b, "db::postgres advisory lock pool B")
            .await
            .expect("close pool B");
        close_test_pool(pool_a, "db::postgres advisory lock pool A")
            .await
            .expect("close pool A");
        test_db.drop().await;
    }

    #[tokio::test]
    async fn postgres_test_sqlx_timeout_wrapper_fails_fast() {
        let err = run_test_postgres_sqlx_op_with_timeout(
            Duration::from_millis(20),
            "synthetic postgres timeout",
            async {
                tokio::time::sleep(Duration::from_millis(80)).await;
                Ok::<(), sqlx::Error>(())
            },
        )
        .await
        .expect_err("sleep should time out");

        assert!(
            err.contains("timed out"),
            "expected timeout wording, got {err}"
        );
    }
}
