use std::collections::BTreeSet;
use std::str::FromStr;
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::sync::OnceLock;
use std::time::Duration;

use sqlx::migrate::Migrator;
use sqlx::pool::PoolConnection;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
#[cfg(test)]
use sqlx::{Connection, PgConnection};
use sqlx::{PgPool, Postgres, Row};
#[cfg(test)]
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::{AgentChannel, AgentDef, Config};
use crate::server::routes::settings::{KvSeedAction, config_default_seed_actions};

static POSTGRES_MIGRATOR: Migrator = sqlx::migrate!("./migrations/postgres");

/// Session-scoped PostgreSQL advisory lock lease.
///
/// The lock remains held for the lifetime of the dedicated connection.
/// Dropping the lease closes that connection so PostgreSQL releases the
/// session lock when the backend exits; callers may also call `unlock()`
/// for an explicit release without waiting for connection teardown.
pub struct AdvisoryLockLease {
    conn: PoolConnection<Postgres>,
    lock_id: i64,
    label: String,
}

impl Drop for AdvisoryLockLease {
    fn drop(&mut self) {
        // Advisory locks are session-scoped. Returning the checked-out connection
        // back to the pool would keep the same backend alive and retain the lock,
        // which breaks singleton failover semantics on runtime death.
        self.conn.close_on_drop();
    }
}

impl AdvisoryLockLease {
    pub async fn try_acquire(
        pool: &PgPool,
        lock_id: i64,
        label: impl Into<String>,
    ) -> Result<Option<Self>, String> {
        let label = label.into();
        let mut conn = pool
            .acquire()
            .await
            .map_err(|error| format!("{label} acquire advisory lock connection: {error}"))?;
        let acquired = sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
            .bind(lock_id)
            .fetch_one(&mut *conn)
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
            .execute(&mut *self.conn)
            .await
            .map(|_| ())
            .map_err(|error| format!("{} advisory lock keepalive: {error}", self.label))
    }

    pub async fn unlock(mut self) -> Result<(), String> {
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(self.lock_id)
            .execute(&mut *self.conn)
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

pub async fn connect(config: &Config) -> Result<Option<PgPool>, String> {
    let Some(options) = connect_options(config)? else {
        return Ok(None);
    };

    let pool = PgPoolOptions::new()
        .max_connections(config.database.pool_max.max(1))
        .acquire_timeout(Duration::from_secs(3))
        .connect_with(options)
        .await
        .map_err(|error| format!("connect postgres: {error}"))?;

    health_check(&pool).await?;
    Ok(Some(pool))
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
    let existing_rows = sqlx::query("SELECT id FROM agents")
        .fetch_all(pool)
        .await
        .map_err(|error| format!("list postgres agents: {error}"))?;

    for row in existing_rows {
        let db_id: String = row
            .try_get("id")
            .map_err(|error| format!("read postgres agent id: {error}"))?;
        if !config_ids.contains(db_id.as_str()) {
            sqlx::query("DELETE FROM agents WHERE id = $1")
                .bind(&db_id)
                .execute(pool)
                .await
                .map_err(|error| format!("delete postgres agent {db_id}: {error}"))?;
        }
    }

    for agent in agents {
        let discord_channel_cc = agent
            .channels
            .claude
            .as_ref()
            .and_then(AgentChannel::target);
        let discord_channel_cdx = agent.channels.codex.as_ref().and_then(AgentChannel::target);
        let discord_channel_id = discord_channel_cc.clone();
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
    }

    Ok(agents.len())
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
const TEST_POSTGRES_POOL_MAX: u32 = 2;
#[cfg(test)]
const TEST_POSTGRES_SETUP_CONCURRENCY: usize = 4;

#[cfg(test)]
fn test_postgres_setup_semaphore() -> &'static Arc<Semaphore> {
    static TEST_POSTGRES_SETUP_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    TEST_POSTGRES_SETUP_SEMAPHORE
        .get_or_init(|| Arc::new(Semaphore::new(TEST_POSTGRES_SETUP_CONCURRENCY)))
}

#[cfg(test)]
async fn acquire_test_postgres_setup_slot(label: &str) -> Result<OwnedSemaphorePermit, String> {
    tokio::time::timeout(
        TEST_POSTGRES_OP_TIMEOUT,
        test_postgres_setup_semaphore().clone().acquire_owned(),
    )
    .await
    .map_err(|_| {
        format!(
            "{label} wait for postgres test setup slot timed out after {}s",
            TEST_POSTGRES_OP_TIMEOUT.as_secs()
        )
    })?
    .map_err(|error| format!("{label} acquire postgres test setup slot: {error}"))
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
async fn connect_test_connection(database_url: &str, label: &str) -> Result<PgConnection, String> {
    run_test_postgres_sqlx_op(
        &format!("{label} connect postgres"),
        PgConnection::connect(database_url),
    )
    .await
}

#[cfg(test)]
pub(crate) async fn connect_test_pool(database_url: &str, label: &str) -> Result<PgPool, String> {
    let options = PgConnectOptions::from_str(database_url)
        .map_err(|error| format!("{label} parse postgres url: {error}"))?;
    run_test_postgres_sqlx_op(
        &format!("{label} connect postgres"),
        PgPoolOptions::new()
            // CI runs many PostgreSQL-backed tests in parallel; keeping the
            // per-test pool lean prevents transient pool starvation.
            .max_connections(TEST_POSTGRES_POOL_MAX)
            .acquire_timeout(TEST_POSTGRES_OP_TIMEOUT)
            .connect_with(options),
    )
    .await
}

#[cfg(test)]
pub(crate) async fn create_test_database(
    admin_url: &str,
    database_name: &str,
    label: &str,
) -> Result<(), String> {
    let _setup_slot = acquire_test_postgres_setup_slot(label).await?;
    let mut admin_conn = connect_test_connection(admin_url, &format!("{label} admin")).await?;
    run_test_postgres_sqlx_op(
        &format!("{label} create postgres test db {database_name}"),
        sqlx::query(&format!("CREATE DATABASE \"{database_name}\"")).execute(&mut admin_conn),
    )
    .await?;
    run_test_postgres_sqlx_op(
        &format!("{label} admin close postgres connection"),
        admin_conn.close(),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
pub(crate) async fn connect_test_pool_and_migrate(
    database_url: &str,
    label: &str,
) -> Result<PgPool, String> {
    let _setup_slot = acquire_test_postgres_setup_slot(label).await?;
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
    let _setup_slot = acquire_test_postgres_setup_slot(label).await?;
    let mut admin_conn = connect_test_connection(admin_url, &format!("{label} admin")).await?;
    run_test_postgres_sqlx_op(
        &format!("{label} terminate postgres test db sessions {database_name}"),
        sqlx::query(
            "SELECT pg_terminate_backend(pid)
             FROM pg_stat_activity
             WHERE datname = $1
               AND pid <> pg_backend_pid()",
        )
        .bind(database_name)
        .execute(&mut admin_conn),
    )
    .await?;
    run_test_postgres_sqlx_op(
        &format!("{label} drop postgres test db {database_name}"),
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{database_name}\""))
            .execute(&mut admin_conn),
    )
    .await?;
    run_test_postgres_sqlx_op(
        &format!("{label} admin close postgres connection"),
        admin_conn.close(),
    )
    .await?;
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
        AdvisoryLockLease, close_test_pool, config_database_summary, connect_and_migrate,
        connect_options, create_test_database, database_enabled, database_summary,
        run_test_postgres_sqlx_op_with_timeout, startup_reseed,
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
