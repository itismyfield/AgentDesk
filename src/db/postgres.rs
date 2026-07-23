use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use sqlx::Connection;
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgConnection, PgPool, Row};

use crate::config::{AgentChannel, AgentDef, Config};
use crate::services::settings::{KvSeedAction, config_default_seed_actions};

static POSTGRES_MIGRATOR: Migrator = sqlx::migrate!("./migrations/postgres");
const LEGACY_AGENT_PREFIX: &str = "openclaw-";
const DEFAULT_PG_ACQUIRE_TIMEOUT_SECS: u64 = 10;
const STARTUP_PG_ACQUIRE_TIMEOUT_SECS: u64 = 10;
const DEFAULT_PG_IDLE_TIMEOUT_SECS: u64 = 5 * 60;
const DEFAULT_PG_MAX_LIFETIME_SECS: u64 = 30 * 60;
const STARTUP_INITIALIZATION_ADVISORY_LOCK_ID: i64 = 3_722_000_001;
const STARTUP_INITIALIZATION_LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(120);
const STARTUP_INITIALIZATION_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(250);
const STARTUP_INITIALIZATION_LOCK_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// #3651: process-global count of runtime-pool connections reserved for
/// foreground turn ingestion. Set once when the runtime pool is built
/// (`connect`) from `config.database.foreground_reserve`. `0` (the initial
/// value, and the value left in place for the boot-only startup warmup pool)
/// disables background backpressure entirely — `background_should_yield` then
/// always returns `false`, preserving pre-#3651 behaviour.
static FOREGROUND_RESERVE: AtomicU32 = AtomicU32::new(0);

/// #3651: minimum interval between "yielding under pool pressure" log lines in
/// a single background loop. The predicate is stateless, so each loop holds its
/// own `Instant` and consults this to avoid log spam during sustained pressure.
pub(crate) const BACKPRESSURE_LOG_THROTTLE: Duration = Duration::from_secs(30);

/// #3651: backpressure predicate for background chore loops.
///
/// Returns `true` when the live in-flight connection count has reached the
/// background budget (`max_connections - foreground_reserve`), meaning a
/// background DB operation right now would risk dipping into the slots reserved
/// for foreground turn ingestion. Background loops call this immediately before
/// their DB work and, when it returns `true`, yield the current tick (skip +
/// retry next tick / via adaptive backoff) so foreground acquire() is very
/// likely to find headroom. Best-effort, **not** a hard guarantee: this is an
/// advisory momentary snapshot, not a semaphore-backed reservation, and it
/// gates only the highest-frequency background chore loops. Under churn several
/// background loops may pass the check and transiently dip into the reserved
/// band; that self-heals on the next tick once in_flight falls below the
/// budget. Foreground-visible delivery paths (e.g. the message outbox drain)
/// must NOT call this — they are never backpressured.
///
/// Pure, stateless, O(1), no locks / awaits / allocation — throttled logging of
/// the yield is the caller's responsibility. When the reserve is `0` the
/// function short-circuits to `false`, so every background loop behaves exactly
/// as it did before #3651.
///
/// `size()`/`num_idle()` are momentary sqlx counter snapshots and may be
/// slightly inconsistent under churn; a one-connection error at the `>=`
/// boundary is harmless because a false-positive yield self-heals on the next
/// tick once `in_flight < budget` again.
pub(crate) fn background_should_yield(pool: &PgPool) -> bool {
    let reserve = FOREGROUND_RESERVE.load(Ordering::Acquire);
    should_yield_for_counters(
        reserve,
        pool.size(),
        pool.num_idle() as u32,
        pool.options().get_max_connections(),
    )
}

/// Pure backpressure arithmetic factored out of [`background_should_yield`] so
/// the threshold logic is unit-testable without a live pool. `reserve == 0`
/// short-circuits to `false` (backpressure disabled / behaviour-preserving).
fn should_yield_for_counters(reserve: u32, size: u32, num_idle: u32, max_connections: u32) -> bool {
    if reserve == 0 {
        return false;
    }
    let in_flight = size.saturating_sub(num_idle);
    let budget = max_connections.saturating_sub(reserve);
    in_flight >= budget
}

/// #3651: clamp a configured `foreground_reserve` so the background budget
/// (`pool_max - reserve`) is always at least 1. A reserve `>= pool_max` (e.g.
/// the default 6 against a small `pool_max: 4`) would leave a zero budget and
/// make every background tick yield forever — a backward-compat regression for
/// pre-existing small-pool configs. Pure / unit-testable.
fn clamp_foreground_reserve(requested: u32, pool_max: u32) -> u32 {
    requested.min(pool_max.max(1).saturating_sub(1))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PoolConnectSettings {
    max_connections: u32,
    acquire_timeout: Duration,
    idle_timeout: Duration,
    max_lifetime: Duration,
    test_before_acquire: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PgConnectFailureKind {
    PoolTimedOut,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PgConnectFailure {
    kind: PgConnectFailureKind,
    message: String,
}

impl PgConnectFailure {
    pub(crate) fn other(message: impl Into<String>) -> Self {
        Self {
            kind: PgConnectFailureKind::Other,
            message: message.into(),
        }
    }

    pub(crate) fn from_sqlx(context: &str, error: sqlx::Error) -> Self {
        let kind = match &error {
            sqlx::Error::PoolTimedOut => PgConnectFailureKind::PoolTimedOut,
            _ => PgConnectFailureKind::Other,
        };
        Self {
            kind,
            message: format!("{context}: {error}"),
        }
    }

    pub(crate) fn kind(&self) -> PgConnectFailureKind {
        self.kind
    }
}

impl fmt::Display for PgConnectFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
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
        Self::try_acquire_with_application_name(pool, lock_id, label, None).await
    }

    /// Acquire a lease on a dedicated connection with an optional PostgreSQL
    /// `application_name`. A stable owner identity lets recovery code distinguish
    /// an abandoned AgentDesk lease from an unrelated or live backend.
    pub async fn try_acquire_named(
        pool: &PgPool,
        lock_id: i64,
        label: impl Into<String>,
        application_name: impl Into<String>,
    ) -> Result<Option<Self>, String> {
        Self::try_acquire_with_application_name(pool, lock_id, label, Some(application_name.into()))
            .await
    }

    async fn try_acquire_with_application_name(
        pool: &PgPool,
        lock_id: i64,
        label: impl Into<String>,
        application_name: Option<String>,
    ) -> Result<Option<Self>, String> {
        let label = label.into();
        let mut options = (*pool.connect_options()).clone();
        if let Some(application_name) = application_name {
            options = options.application_name(&application_name);
        }
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
        "{}:{}/{} user={} pool_max={} fg_reserve={}",
        config.database.host,
        config.database.port,
        config.database.dbname,
        config.database.user,
        config.database.pool_max.max(1),
        config.database.foreground_reserve
    )
}

pub fn connect_options(config: &Config) -> Result<Option<PgConnectOptions>, String> {
    if !database_enabled(config) {
        return Ok(None);
    }

    if let Some(url) = database_url_override() {
        return PgConnectOptions::from_str(&url).map(Some).map_err(|error| {
            format!(
                "parse DATABASE_URL {}: {error}",
                crate::utils::redact::mask_dsn_password(&url)
            )
        });
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
        idle_timeout: Duration::from_secs(DEFAULT_PG_IDLE_TIMEOUT_SECS),
        max_lifetime: Duration::from_secs(DEFAULT_PG_MAX_LIFETIME_SECS),
        test_before_acquire: true,
    }
}

fn bootstrap_pool_settings(config: &Config) -> PoolConnectSettings {
    PoolConnectSettings {
        max_connections: config.database.pool_max.max(1),
        acquire_timeout: Duration::from_secs(STARTUP_PG_ACQUIRE_TIMEOUT_SECS),
        idle_timeout: Duration::from_secs(DEFAULT_PG_IDLE_TIMEOUT_SECS),
        max_lifetime: Duration::from_secs(DEFAULT_PG_MAX_LIFETIME_SECS),
        test_before_acquire: true,
    }
}

fn startup_pool_settings(config: &Config) -> PoolConnectSettings {
    let steady_max = config.database.pool_max.max(1);
    PoolConnectSettings {
        max_connections: steady_max.saturating_mul(3).div_ceil(2).max(2),
        acquire_timeout: Duration::from_secs(STARTUP_PG_ACQUIRE_TIMEOUT_SECS),
        idle_timeout: Duration::from_secs(DEFAULT_PG_IDLE_TIMEOUT_SECS),
        max_lifetime: Duration::from_secs(DEFAULT_PG_MAX_LIFETIME_SECS),
        test_before_acquire: true,
    }
}

fn pool_options(settings: PoolConnectSettings) -> PgPoolOptions {
    PgPoolOptions::new()
        .max_connections(settings.max_connections)
        .acquire_timeout(settings.acquire_timeout)
        .idle_timeout(settings.idle_timeout)
        .max_lifetime(settings.max_lifetime)
        .test_before_acquire(settings.test_before_acquire)
}

async fn run_health_check(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1").fetch_one(pool).await.map(|_| ())
}

async fn connect_with_settings_typed(
    config: &Config,
    settings: PoolConnectSettings,
    context: &str,
) -> Result<Option<PgPool>, PgConnectFailure> {
    let Some(options) = connect_options(config).map_err(PgConnectFailure::other)? else {
        return Ok(None);
    };

    let pool = pool_options(settings)
        .connect_with(options)
        .await
        .map_err(|error| PgConnectFailure::from_sqlx(context, error))?;

    run_health_check(&pool)
        .await
        .map_err(|error| PgConnectFailure::from_sqlx("postgres health check failed", error))?;
    Ok(Some(pool))
}

async fn connect_with_settings(
    config: &Config,
    settings: PoolConnectSettings,
    context: &str,
) -> Result<Option<PgPool>, String> {
    connect_with_settings_typed(config, settings, context)
        .await
        .map_err(|error| error.to_string())
}

fn install_foreground_reserve(config: &Config) {
    let pool_max = config.database.pool_max.max(1);
    let requested = config.database.foreground_reserve;
    let effective = clamp_foreground_reserve(requested, pool_max);
    if effective != requested {
        tracing::warn!(
            requested,
            pool_max,
            effective,
            "#3651: foreground_reserve >= pool_max; clamped to preserve a background budget"
        );
    }
    FOREGROUND_RESERVE.store(effective, Ordering::Release);
}

pub async fn connect(config: &Config) -> Result<Option<PgPool>, String> {
    let pool =
        connect_with_settings(config, runtime_pool_settings(config), "connect postgres").await?;
    if pool.is_some() {
        // #3651: install the foreground reservation for the runtime pool. Only
        // the runtime pool participates in background backpressure; the startup
        // warmup pool (`connect_for_startup`) deliberately leaves the reserve at
        // 0 so boot-time background work is never throttled before the runtime
        // pool exists.
        //
        // Clamp the reserve so the background budget (`max - reserve`) is always
        // >= 1. A configured `foreground_reserve >= pool_max` (e.g. the default
        // 6 with a small `pool_max: 4`) would otherwise make the budget 0 and
        // yield every background tick forever — a regression for pre-existing
        // small-pool configs. We keep at least one background slot.
        install_foreground_reserve(config);
    }
    Ok(pool)
}

/// Build the eager pool used by dcserver migration and startup initialization.
///
/// The connection established here is retained and used for real bootstrap DB
/// work. Its 10-second acquire deadline tolerates slow TCP/TLS/auth handshakes;
/// the separate long-lived runtime pool is built only after initialization and
/// retains the normal 10-second acquire timeout.
pub(crate) async fn connect_for_bootstrap(
    config: &Config,
) -> Result<Option<PgPool>, PgConnectFailure> {
    connect_with_settings_typed(
        config,
        bootstrap_pool_settings(config),
        "connect postgres startup/migrate pool",
    )
    .await
}

/// Activate the eager long-lived runtime pool after bootstrap initialization.
/// This stays typed so `PoolTimedOut` classification reaches bootstrap logs.
pub(crate) async fn connect_runtime_after_bootstrap(
    config: &Config,
) -> Result<Option<PgPool>, PgConnectFailure> {
    let pool = connect_with_settings_typed(
        config,
        runtime_pool_settings(config),
        "connect postgres runtime pool after bootstrap",
    )
    .await?;
    if pool.is_some() {
        install_foreground_reserve(config);
    }
    Ok(pool)
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
    with_startup_advisory_lock(&pool, || async { migrate(&pool).await }).await?;
    Ok(Some(pool))
}

pub async fn migrate(pool: &PgPool) -> Result<(), String> {
    POSTGRES_MIGRATOR
        .run(pool)
        .await
        .map_err(|error| format!("run postgres migrations: {error}"))?;
    Ok(())
}

async fn acquire_startup_advisory_lock(pool: &PgPool) -> Result<AdvisoryLockLease, String> {
    let started = Instant::now();
    let mut last_wait_log = started
        .checked_sub(STARTUP_INITIALIZATION_LOCK_LOG_INTERVAL)
        .unwrap_or(started);

    loop {
        match AdvisoryLockLease::try_acquire(
            pool,
            STARTUP_INITIALIZATION_ADVISORY_LOCK_ID,
            "postgres startup initialization",
        )
        .await?
        {
            Some(lease) => {
                tracing::info!(
                    lock_id = STARTUP_INITIALIZATION_ADVISORY_LOCK_ID,
                    wait_ms = started.elapsed().as_millis() as u64,
                    "[startup] acquired postgres startup advisory lock"
                );
                return Ok(lease);
            }
            None => {
                let elapsed = started.elapsed();
                if elapsed >= STARTUP_INITIALIZATION_LOCK_WAIT_TIMEOUT {
                    return Err(format!(
                        "postgres startup initialization advisory lock {} unavailable after {}s",
                        STARTUP_INITIALIZATION_ADVISORY_LOCK_ID,
                        elapsed.as_secs()
                    ));
                }

                if last_wait_log.elapsed() >= STARTUP_INITIALIZATION_LOCK_LOG_INTERVAL {
                    tracing::warn!(
                        lock_id = STARTUP_INITIALIZATION_ADVISORY_LOCK_ID,
                        waited_ms = elapsed.as_millis() as u64,
                        timeout_ms = STARTUP_INITIALIZATION_LOCK_WAIT_TIMEOUT.as_millis() as u64,
                        "[startup] waiting for postgres startup advisory lock"
                    );
                    last_wait_log = Instant::now();
                }

                tokio::time::sleep(STARTUP_INITIALIZATION_LOCK_RETRY_INTERVAL).await;
            }
        }
    }
}

pub async fn with_startup_advisory_lock<T, F, Fut>(pool: &PgPool, operation: F) -> Result<T, String>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let started = Instant::now();
    let lease = acquire_startup_advisory_lock(pool).await?;
    let result = operation().await;
    let release_result = lease.unlock().await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    match (result, release_result) {
        (Ok(value), Ok(())) => {
            tracing::info!(
                lock_id = STARTUP_INITIALIZATION_ADVISORY_LOCK_ID,
                elapsed_ms,
                "[startup] released postgres startup advisory lock"
            );
            Ok(value)
        }
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Ok(())) => {
            tracing::info!(
                lock_id = STARTUP_INITIALIZATION_ADVISORY_LOCK_ID,
                elapsed_ms,
                "[startup] released postgres startup advisory lock after failed startup mutation"
            );
            Err(error)
        }
        (Err(error), Err(unlock_error)) => {
            tracing::warn!(
                lock_id = STARTUP_INITIALIZATION_ADVISORY_LOCK_ID,
                elapsed_ms,
                unlock_error,
                "[startup] failed to explicitly release postgres startup advisory lock after startup mutation error"
            );
            Err(error)
        }
    }
}

pub async fn startup_reseed(pool: &PgPool, config: &Config) -> Result<(), String> {
    apply_kv_seed_actions(pool, &config_default_seed_actions(config)).await?;
    upsert_kv_meta(pool, "server_port", &config.server.port.to_string()).await?;
    crate::services::settings::seed_runtime_config_defaults_pg(pool, config).await?;
    crate::server::routes::escalation::seed_escalation_defaults_pg(pool, config).await?;
    let pipeline_path = config.policies.dir.join("default-pipeline.yaml");
    crate::db::table_metadata::sync_pipeline_stages_from_yaml_pg(pool, &pipeline_path)
        .await
        .map_err(|error| {
            format!(
                "sync pipeline_stages from {}: {error}",
                pipeline_path.display()
            )
        })?;

    for repo_id in normalized_repo_ids(&config.github.repos) {
        register_repo(pool, &repo_id).await?;
    }

    // #3692: in a cluster, the shared `agents` table is owned by the leader.
    // `sync_agents_from_config_pg` is destructive (it DELETEs agents absent from
    // the local config), so if a worker/auto node ran it at boot it would
    // clobber the leader's roster — each node's startup would fight over the
    // shared table and the roster would flip-flop per deploy order. Reseed runs
    // before cluster leadership election, so gate on the configured role: only a
    // single-node deployment (cluster disabled) or the explicitly-configured
    // leader owns the roster sync. Workers/auto nodes trust the shared roster.
    if agent_roster_sync_enabled(config) {
        sync_agents_from_config_pg(pool, &config.agents).await?;
    } else {
        tracing::info!(
            "[agent-sync] skipping config→DB agent roster sync on non-leader node \
             (cluster.role={}); the cluster leader owns the shared agents table (#3692)",
            config.cluster.role
        );
    }
    Ok(())
}

pub async fn startup_reseed_with_warmup_pool(
    runtime_pool: &PgPool,
    config: &Config,
) -> Result<(), String> {
    let startup_pg_pool = match connect_for_startup(config).await {
        Ok(pool) => pool,
        Err(error) => {
            tracing::warn!(
                "[startup] postgres warmup pool unavailable; falling back to runtime pool: {error}"
            );
            None
        }
    };
    let startup_pool = startup_pg_pool.as_ref().unwrap_or(runtime_pool);
    startup_reseed(startup_pool, config).await?;
    drop(startup_pg_pool);
    Ok(())
}

/// Whether this node should run the destructive config→DB agent roster sync.
/// True for single-node deployments (cluster disabled) and for the node
/// explicitly configured as `cluster.role: leader`. Worker/auto nodes return
/// false so they never clobber the leader-owned shared roster (#3692). Shared
/// with the config-audit path (`discord_config_audit`), which reaches the same
/// destructive sync before `startup_reseed` runs.
pub(crate) fn agent_roster_sync_enabled(config: &Config) -> bool {
    !config.cluster.enabled || config.cluster.role.trim().eq_ignore_ascii_case("leader")
}

pub async fn health_check(pool: &PgPool) -> Result<(), String> {
    run_health_check(pool)
        .await
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationChecksumMismatch {
    pub version: i64,
    pub applied_checksum: String,
    pub resolved_checksum: String,
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

pub async fn applied_migration_checksum_mismatch_details(
    pool: &PgPool,
) -> Result<Vec<MigrationChecksumMismatch>, String> {
    let rows = sqlx::query(
        "SELECT version, checksum, success
         FROM _sqlx_migrations
         ORDER BY version",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("query _sqlx_migrations checksums: {error}"))?;

    // sqlx Migrator yields both ReversibleUp and ReversibleDown entries for the
    // same version. The applied checksum stored in `_sqlx_migrations` is the Up
    // checksum (sqlx::run_direct skips Down migrations when applying), so the
    // expected map must mirror that — collecting all entries collapses Up/Down
    // for the same version and a Down checksum can shadow the Up, producing a
    // false-positive drift signal. Mirror sqlx's own `is_down_migration()`
    // filter from `Migrator::run_direct`.
    let resolved_checksums = POSTGRES_MIGRATOR
        .iter()
        .filter(|migration| !migration.migration_type.is_down_migration())
        .map(|migration| (migration.version, migration.checksum.as_ref()))
        .collect::<BTreeMap<_, _>>();

    let mut mismatches = Vec::new();
    for row in rows {
        let version = row.get::<i64, _>("version");
        let success = row.get::<bool, _>("success");
        if !success {
            continue;
        }
        let Some(expected_checksum) = resolved_checksums.get(&version) else {
            continue;
        };
        let applied_checksum = row.get::<Vec<u8>, _>("checksum");
        if applied_checksum.as_slice() != *expected_checksum {
            mismatches.push(MigrationChecksumMismatch {
                version,
                applied_checksum: checksum_hex(&applied_checksum),
                resolved_checksum: checksum_hex(*expected_checksum),
            });
        }
    }

    Ok(mismatches)
}

// reason: public migration-diagnostics wrapper surfaced by maintenance paths,
// not by every compile target. See #3034.
#[allow(dead_code)]
pub async fn applied_migration_checksum_mismatches(pool: &PgPool) -> Result<Vec<i64>, String> {
    Ok(applied_migration_checksum_mismatch_details(pool)
        .await?
        .into_iter()
        .map(|mismatch| mismatch.version)
        .collect())
}

fn checksum_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
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
    crate::voice::commands::validate_agent_alias_collisions(agents)
        .map_err(|error| format!("validate voice aliases: {error}"))?;

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
        "UPDATE sessions SET agent_id = NULL WHERE agent_id = $1",
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

    // #3667: config-declared intake node-affinity labels. `None` (key absent
    // from yaml) binds SQL NULL; the COALESCE then keeps the existing DB value
    // so an out-of-band label (e.g. ch-td's DB-only `["mac-book"]`, not in yaml)
    // is never wiped by a routine restart sync. `Some([...])` sets/overwrites it
    // and `Some([])` explicitly clears it. We reference the bound `$11` (not
    // EXCLUDED) in the conflict branch so the NULL signal survives the VALUES
    // COALESCE used to satisfy the NOT NULL column on insert.
    let preferred_intake_node_labels = agent
        .preferred_intake_node_labels
        .as_ref()
        .map(|labels| serde_json::json!(labels));

    sqlx::query(
        "INSERT INTO agents (
            id, name, name_ko, provider, department, avatar_emoji,
            discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx,
            preferred_intake_node_labels
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, COALESCE($11, '[]'::JSONB))
         ON CONFLICT (id) DO UPDATE
         SET name = EXCLUDED.name,
             name_ko = EXCLUDED.name_ko,
             provider = EXCLUDED.provider,
             department = EXCLUDED.department,
             avatar_emoji = EXCLUDED.avatar_emoji,
             discord_channel_id = EXCLUDED.discord_channel_id,
             discord_channel_alt = EXCLUDED.discord_channel_alt,
             discord_channel_cc = EXCLUDED.discord_channel_cc,
             discord_channel_cdx = EXCLUDED.discord_channel_cdx,
             preferred_intake_node_labels =
                 COALESCE($11, agents.preferred_intake_node_labels)",
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
    .bind(preferred_intake_node_labels)
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
             pipeline_config = COALESCE(legacy.pipeline_config, agents.pipeline_config),
             -- #3667: inherit a legacy DB-only intake label only when the
             -- canonical row has none yet (config-set value wins; the column is
             -- NOT NULL so a plain COALESCE would clobber it with '[]').
             preferred_intake_node_labels = CASE
                 WHEN jsonb_array_length(agents.preferred_intake_node_labels) = 0
                      AND jsonb_array_length(legacy.preferred_intake_node_labels) > 0
                     THEN legacy.preferred_intake_node_labels
                 ELSE agents.preferred_intake_node_labels
             END
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
pub(crate) async fn connect_test_pool_with_max_connections(
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
// SAFETY (await_holding_lock): `lock_test_setup()` is a std Mutex held across
// the pool-connect/query awaits *on purpose* — it serializes concurrent
// PG-backed test DB create/drop so they cannot race. Dropping the guard before
// the awaits would reintroduce the CI race this lock was added to fix. Test-only.
#[allow(clippy::await_holding_lock)]
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
pub(crate) async fn connect_test_pool_with_max_connections_and_migrate(
    database_url: &str,
    label: &str,
    max_connections: u32,
) -> Result<PgPool, String> {
    let pool = connect_test_pool_with_max_connections(database_url, label, max_connections).await?;
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
pub(crate) async fn connect_test_pool_and_migrate_config(
    config: &Config,
    label: &str,
) -> Result<Option<PgPool>, String> {
    if !database_enabled(config) {
        return Ok(None);
    }

    let mut options = PgConnectOptions::new()
        .host(&config.database.host)
        .port(config.database.port)
        .username(&config.database.user)
        .database(&config.database.dbname);
    if let Some(password) = config.database.password.as_deref() {
        options = options.password(password);
    }

    let pool = run_test_postgres_sqlx_op(
        &format!("{label} connect postgres"),
        PgPoolOptions::new()
            .max_connections(config.database.pool_max.max(1))
            .acquire_timeout(TEST_POSTGRES_OP_TIMEOUT)
            .connect_with(options),
    )
    .await?;
    tokio::time::timeout(TEST_POSTGRES_OP_TIMEOUT, migrate(&pool))
        .await
        .map_err(|_| {
            format!(
                "{label} migrate postgres timed out after {}s",
                TEST_POSTGRES_OP_TIMEOUT.as_secs()
            )
        })??;
    Ok(Some(pool))
}

#[cfg(test)]
// SAFETY (await_holding_lock): same serialization rationale as
// `create_test_database` — `lock_test_setup()` is held across the teardown
// awaits to keep concurrent PG test DB drops from racing. Test-only.
#[allow(clippy::await_holding_lock)]
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
    let drop_sql = format!("DROP DATABASE IF EXISTS \"{database_name}\" WITH (FORCE)");
    let mut last_error = None;
    for attempt in 1..=3 {
        match run_test_postgres_sqlx_op(
            &format!("{label} drop postgres test db {database_name}"),
            sqlx::query(&drop_sql).execute(&admin_pool),
        )
        .await
        {
            Ok(_) => {
                last_error = None;
                break;
            }
            Err(error) => {
                last_error = Some(error);
                if attempt < 3 {
                    tokio::time::sleep(Duration::from_millis(100 * attempt)).await;
                }
            }
        }
    }
    if let Some(error) = last_error {
        close_test_pool(admin_pool, &format!("{label} admin")).await?;
        return Err(error);
    }
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
        AdvisoryLockLease, POSTGRES_MIGRATOR, STARTUP_PG_ACQUIRE_TIMEOUT_SECS,
        agent_roster_sync_enabled, bootstrap_pool_settings, checksum_hex, clamp_foreground_reserve,
        close_test_pool, config_database_summary, connect_options,
        connect_test_pool_and_migrate_config, create_test_database, database_enabled,
        database_summary, health_check, run_test_postgres_sqlx_op_with_timeout,
        runtime_pool_settings, should_yield_for_counters, startup_pool_settings, startup_reseed,
        sync_agents_from_config_pg, with_startup_advisory_lock,
    };
    use sqlx::Row;
    use std::collections::BTreeMap;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;
    use tokio::sync::{Mutex, Notify};

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
            aliases: Vec::new(),
            wake_word: None,
            voice_enabled: true,
            sensitivity_mode: None,
            voice: crate::config::AgentVoiceConfig::default(),
            provider: "codex".to_string(),
            channels: crate::config::AgentChannels {
                codex: Some(crate::config::AgentChannel::from("pg-agent-cdx")),
                ..Default::default()
            },
            keywords: Vec::new(),
            department: Some("platform".to_string()),
            avatar_emoji: Some(":gear:".to_string()),
            preferred_intake_node_labels: None,
        }];
        config
    }

    #[test]
    fn checksum_hex_formats_lowercase_byte_pairs() {
        assert_eq!(checksum_hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    }

    #[test]
    fn checksum_resolution_filters_down_migrations_to_avoid_false_positive() {
        // Regression for #1688: applied_migration_checksum_mismatches() builds
        // a `version → expected_checksum` BTreeMap from POSTGRES_MIGRATOR.iter().
        // sqlx yields ReversibleUp + ReversibleDown for the same version, and
        // the applied checksum stored in `_sqlx_migrations` is the Up checksum
        // (sqlx::run_direct skips Down when applying). Without filtering Down
        // out, a Down checksum would shadow the Up in the BTreeMap and the
        // helper would report a false-positive mismatch on every reversible
        // migration in the tree.
        use sqlx::migrate::MigrationType;

        // Identify any reversible pair currently in the tree. As of #1688 this
        // is at least the agent-quality-daily migration (version 13).
        let mut reversible_versions: Vec<i64> = POSTGRES_MIGRATOR
            .iter()
            .filter(|m| matches!(m.migration_type, MigrationType::ReversibleUp))
            .map(|m| m.version)
            .collect();
        reversible_versions.sort();
        reversible_versions.dedup();

        assert!(
            !reversible_versions.is_empty(),
            "test guards a real bug only when the tree contains at least one reversible migration; \
             if all migrations became `Simple` the filter is no longer load-bearing — adjust this test"
        );

        // Helper using the same filter as the production code (mirrors sqlx's
        // own `Migrator::run_direct`).
        let filtered: BTreeMap<i64, &[u8]> = POSTGRES_MIGRATOR
            .iter()
            .filter(|migration| !migration.migration_type.is_down_migration())
            .map(|migration| (migration.version, migration.checksum.as_ref()))
            .collect();

        for version in reversible_versions {
            let up_checksum = POSTGRES_MIGRATOR
                .iter()
                .find(|m| {
                    m.version == version && matches!(m.migration_type, MigrationType::ReversibleUp)
                })
                .map(|m| m.checksum.as_ref())
                .expect("reversible up entry");
            let down_checksum = POSTGRES_MIGRATOR
                .iter()
                .find(|m| {
                    m.version == version
                        && matches!(m.migration_type, MigrationType::ReversibleDown)
                })
                .map(|m| m.checksum.as_ref())
                .expect("reversible down entry");
            assert_ne!(
                up_checksum, down_checksum,
                "test only catches the bug when up/down checksums actually differ"
            );

            let resolved = filtered
                .get(&version)
                .copied()
                .expect("filtered map must retain reversible up version");
            assert_eq!(
                resolved, up_checksum,
                "filtered checksum map for version {version} must keep Up, not Down"
            );
        }
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
        config.database.foreground_reserve = 5;

        assert_eq!(
            config_database_summary(&config),
            "db.internal:5433/agentdesk_dev user=agentdesk_app pool_max=16 fg_reserve=5"
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
        assert_eq!(settings.idle_timeout, Duration::from_secs(5 * 60));
        assert_eq!(settings.max_lifetime, Duration::from_secs(30 * 60));
        assert!(settings.test_before_acquire);
    }

    #[test]
    fn bootstrap_migration_pool_has_longer_scoped_deadline() {
        let mut config = crate::config::Config::default();
        config.database.enabled = true;
        config.database.pool_max = 18;
        let settings = bootstrap_pool_settings(&config);

        assert_eq!(settings.max_connections, 18);
        assert_eq!(settings.acquire_timeout, Duration::from_secs(10));
    }

    #[test]
    fn runtime_pool_settings_enable_dead_peer_detection() {
        let mut config = crate::config::Config::default();
        config.database.enabled = true;
        config.database.pool_max = 5;

        let settings = runtime_pool_settings(&config);

        assert_eq!(settings.max_connections, 5);
        assert_eq!(settings.acquire_timeout, Duration::from_secs(10));
        assert_eq!(settings.idle_timeout, Duration::from_secs(5 * 60));
        assert_eq!(settings.max_lifetime, Duration::from_secs(30 * 60));
        assert!(settings.test_before_acquire);
    }

    #[test]
    fn background_backpressure_disabled_when_reserve_zero() {
        // #3651 behaviour-preserving guard: reserve=0 → always false regardless
        // of how saturated the pool looks. This is the no-config / startup-pool
        // / pre-#3651 path.
        assert!(!should_yield_for_counters(0, 18, 0, 18));
        assert!(!should_yield_for_counters(0, 18, 18, 18));
        // Even a fully in-flight pool does not yield when disabled.
        assert!(!should_yield_for_counters(0, 100, 0, 18));
    }

    #[test]
    fn background_backpressure_yields_only_at_or_past_budget() {
        // pool_max=18, reserve=6 → background budget=12.
        let max = 18;
        let reserve = 6;
        // Idle pool (in_flight=0) → never yields.
        assert!(!should_yield_for_counters(reserve, 0, 0, max));
        // size grows but all idle → in_flight=0 → no yield.
        assert!(!should_yield_for_counters(reserve, 18, 18, max));
        // in_flight=11 (< budget 12) → no yield.
        assert!(!should_yield_for_counters(reserve, 11, 0, max));
        // in_flight=12 (== budget) → yield (protect the reserved 6).
        assert!(should_yield_for_counters(reserve, 12, 0, max));
        // in_flight=18 (pool fully checked out) → yield.
        assert!(should_yield_for_counters(reserve, 18, 0, max));
        // Mixed: size=15, idle=4 → in_flight=11 < 12 → no yield.
        assert!(!should_yield_for_counters(reserve, 15, 4, max));
        // Mixed: size=15, idle=3 → in_flight=12 == 12 → yield.
        assert!(should_yield_for_counters(reserve, 15, 3, max));
    }

    #[test]
    fn background_backpressure_saturating_boundaries() {
        // num_idle > size cannot happen in practice, but the saturating
        // subtraction must not panic and must clamp in_flight to 0 → no yield.
        assert!(!should_yield_for_counters(6, 2, 5, 18));
        // reserve >= max_connections → budget saturates to 0 → any in_flight
        // (>= 0) yields, but an idle pool (in_flight=0 >= 0) also yields. This
        // is the pathological "reserve too large" config; it is documented as a
        // misconfiguration risk and handled without panic.
        assert!(should_yield_for_counters(18, 1, 0, 18));
        assert!(should_yield_for_counters(20, 1, 0, 18));
        // reserve == max with a fully-idle pool: in_flight=0 >= budget 0 → yield.
        assert!(should_yield_for_counters(18, 5, 5, 18));
    }

    #[test]
    fn clamp_foreground_reserve_always_leaves_a_background_slot() {
        // Normal: a reserve comfortably below pool_max is unchanged.
        assert_eq!(clamp_foreground_reserve(6, 18), 6);
        // reserve == pool_max → clamp to max-1 so the background budget stays >= 1.
        assert_eq!(clamp_foreground_reserve(6, 6), 5);
        // reserve > pool_max (default 6 against a small pool_max: 4) → clamp to 3.
        // This is the backward-compat regression case codex flagged.
        assert_eq!(clamp_foreground_reserve(6, 4), 3);
        // pool_max 1 → no slot can be reserved → 0 (disables backpressure entirely).
        assert_eq!(clamp_foreground_reserve(6, 1), 0);
        // pool_max 0 is normalised to 1 → reserve 0 (no panic, no infinite yield).
        assert_eq!(clamp_foreground_reserve(6, 0), 0);
        // reserve 0 stays 0 (backpressure disabled / behaviour-preserving).
        assert_eq!(clamp_foreground_reserve(0, 18), 0);
    }

    #[tokio::test]
    async fn postgres_migrations_and_startup_reseed_prepare_empty_database() {
        let test_db = TestDatabase::create().await;
        let config = postgres_test_config(&test_db);

        let pool =
            connect_test_pool_and_migrate_config(&config, "db::postgres migration test pool")
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

        let pipeline_stage_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pipeline_stages WHERE repo_id = '__default__'",
        )
        .fetch_one(&pool)
        .await
        .expect("count default pipeline_stages");
        assert!(
            pipeline_stage_count >= 1,
            "startup reseed must materialize default pipeline_stages from YAML"
        );

        let (first_stage, first_stage_order): (String, i64) = sqlx::query_as(
            "SELECT stage_name, stage_order
             FROM pipeline_stages
             WHERE repo_id = '__default__'
             ORDER BY stage_order
             LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .expect("load first default pipeline stage");
        assert_eq!(first_stage, "backlog");
        assert_eq!(first_stage_order, 1);

        let (source_of_truth, file_path, last_synced): (String, Option<String>, bool) =
            sqlx::query_as(
                "SELECT source_of_truth, file_path, last_synced_at IS NOT NULL AS last_synced
                 FROM db_table_metadata
                 WHERE table_name = 'pipeline_stages'",
            )
            .fetch_one(&pool)
            .await
            .expect("load pipeline_stages source metadata");
        assert_eq!(source_of_truth, "file-canonical");
        assert!(
            file_path
                .as_deref()
                .is_some_and(|path| path.ends_with("default-pipeline.yaml")),
            "pipeline_stages metadata should record the source YAML path"
        );
        assert!(last_synced, "startup reseed must stamp last_synced_at");

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

    #[test]
    fn agent_roster_sync_gated_to_leader_or_single_node() {
        // #3692: only a single-node deployment or the configured leader owns the
        // destructive config→DB agent roster sync.
        let mut config = crate::config::Config::default();

        config.cluster.enabled = false; // single-node: always owns the roster
        config.cluster.role = "auto".to_string();
        assert!(agent_roster_sync_enabled(&config));

        config.cluster.enabled = true;
        for (role, expected) in [
            ("leader", true),
            ("Leader", true),
            ("  leader  ", true),
            ("worker", false),
            ("auto", false),
            ("", false),
        ] {
            config.cluster.role = role.to_string();
            assert_eq!(
                agent_roster_sync_enabled(&config),
                expected,
                "cluster.role={role:?}"
            );
        }
    }

    #[tokio::test]
    async fn worker_node_reseed_does_not_clobber_shared_agent_roster() {
        // #3692: a cluster worker/auto node must NOT run the destructive agent
        // sync at boot — doing so would delete leader-owned agents from the
        // shared table and re-add its own, causing roster flip-flop per deploy.
        let test_db = TestDatabase::create().await;
        let mut config = postgres_test_config(&test_db);

        let pool = connect_test_pool_and_migrate_config(
            &config,
            "db::postgres worker-reseed gating test pool",
        )
        .await
        .expect("connect and migrate postgres")
        .expect("postgres pool");

        // Simulate a leader-owned agent already present in the shared table.
        sqlx::query("INSERT INTO agents (id, name, provider) VALUES ($1, $2, $3)")
            .bind("leader-owned")
            .bind("Leader Owned")
            .bind("claude")
            .execute(&pool)
            .await
            .expect("seed leader-owned agent");

        // This node is a cluster worker: reseed must skip the agent sync.
        config.cluster.enabled = true;
        config.cluster.role = "worker".to_string();
        startup_reseed(&pool, &config).await.expect("worker reseed");

        let leader_owned: i64 =
            sqlx::query_scalar("SELECT count(*) FROM agents WHERE id = 'leader-owned'")
                .fetch_one(&pool)
                .await
                .expect("count leader-owned");
        assert_eq!(
            leader_owned, 1,
            "worker reseed must not delete leader-owned agents"
        );
        let own_agent: i64 =
            sqlx::query_scalar("SELECT count(*) FROM agents WHERE id = 'pg-agent'")
                .fetch_one(&pool)
                .await
                .expect("count pg-agent");
        assert_eq!(
            own_agent, 0,
            "worker reseed must not sync its own config agents into the shared roster"
        );

        close_test_pool(pool, "db::postgres worker-reseed gating test pool")
            .await
            .expect("close postgres pool");
        test_db.drop().await;
    }

    #[tokio::test]
    async fn sync_agents_writes_and_preserves_preferred_intake_node_labels() {
        // #3667: config-declared node-affinity labels (`Option<Vec<String>>`)
        // are synced to the agents.preferred_intake_node_labels JSONB column:
        //   Some([...]) sets/overwrites, Some([]) clears, None preserves an
        //   out-of-band value (the ch-td DB-only `["mac-book"]` case, absent
        //   from yaml).
        let test_db = TestDatabase::create().await;
        let mut config = postgres_test_config(&test_db);

        let pool = connect_test_pool_and_migrate_config(
            &config,
            "db::postgres intake-label sync test pool",
        )
        .await
        .expect("connect and migrate postgres")
        .expect("postgres pool");

        let read_labels = |id: &'static str| {
            let pool = pool.clone();
            async move {
                let value: serde_json::Value = sqlx::query_scalar(
                    "SELECT preferred_intake_node_labels FROM agents WHERE id = $1",
                )
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("read preferred_intake_node_labels");
                value
            }
        };

        // 1. Config declares Some(["mac-mini"]) -> written verbatim on insert.
        config.agents[0].preferred_intake_node_labels = Some(vec!["mac-mini".to_string()]);
        sync_agents_from_config_pg(&pool, &config.agents)
            .await
            .expect("sync with labels");
        assert_eq!(
            read_labels("pg-agent").await,
            serde_json::json!(["mac-mini"])
        );

        // 2. Seed an out-of-band label, then sync with the field ABSENT (None:
        //    agent in yaml but no key). COALESCE($11=NULL, existing) must
        //    preserve the existing value instead of wiping it.
        sqlx::query("UPDATE agents SET preferred_intake_node_labels = $1 WHERE id = $2")
            .bind(serde_json::json!(["mac-book"]))
            .bind("pg-agent")
            .execute(&pool)
            .await
            .expect("seed out-of-band label");
        config.agents[0].preferred_intake_node_labels = None;
        sync_agents_from_config_pg(&pool, &config.agents)
            .await
            .expect("sync with absent field");
        assert_eq!(
            read_labels("pg-agent").await,
            serde_json::json!(["mac-book"]),
            "absent field (None) must not wipe an out-of-band label"
        );

        // 3. A non-empty config list is authoritative again on conflict/update.
        config.agents[0].preferred_intake_node_labels =
            Some(vec!["mac-mini".to_string(), "release".to_string()]);
        sync_agents_from_config_pg(&pool, &config.agents)
            .await
            .expect("sync overwrite labels");
        assert_eq!(
            read_labels("pg-agent").await,
            serde_json::json!(["mac-mini", "release"])
        );

        // 4. Some([]) is an explicit clear, distinct from None.
        config.agents[0].preferred_intake_node_labels = Some(Vec::new());
        sync_agents_from_config_pg(&pool, &config.agents)
            .await
            .expect("sync explicit clear");
        assert_eq!(
            read_labels("pg-agent").await,
            serde_json::json!([]),
            "Some([]) must explicitly clear the label"
        );

        close_test_pool(pool, "db::postgres intake-label sync test pool")
            .await
            .expect("close postgres pool");
        test_db.drop().await;
    }

    #[tokio::test]
    async fn core_status_constraints_reject_invalid_values_and_allow_valid_states() {
        let test_db = TestDatabase::create().await;
        let config = postgres_test_config(&test_db);

        let pool =
            connect_test_pool_and_migrate_config(&config, "db::postgres status check test pool")
                .await
                .expect("connect and migrate postgres")
                .expect("postgres pool");

        let agent_error = sqlx::query(
            "INSERT INTO agents (id, name, provider, status)
             VALUES ('agent-bad-status', 'Bad Status', 'claude', 'busy-now')",
        )
        .execute(&pool)
        .await
        .expect_err("unknown agents.status must be rejected");
        assert!(
            agent_error
                .to_string()
                .contains("agents_status_known_check"),
            "expected agents.status CHECK violation, got: {agent_error}"
        );

        sqlx::query(
            "INSERT INTO agents (id, name, provider, status)
             VALUES ('agent-valid-status', 'Valid Status', 'claude', 'working')",
        )
        .execute(&pool)
        .await
        .expect("valid agents.status should insert");

        let kanban_error = sqlx::query(
            "INSERT INTO kanban_cards (id, title, status)
             VALUES ('card-bad-status', 'Bad Status', 'in progress')",
        )
        .execute(&pool)
        .await
        .expect_err("non-slug kanban_cards.status must be rejected");
        assert!(
            kanban_error
                .to_string()
                .contains("kanban_cards_status_slug_check"),
            "expected kanban status CHECK violation, got: {kanban_error}"
        );

        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status)
             VALUES ('card-valid-status', 'Valid Status', 'qa_test')",
        )
        .execute(&pool)
        .await
        .expect("valid custom kanban slug should insert");

        close_test_pool(pool, "db::postgres status check test pool")
            .await
            .expect("close postgres pool");
        test_db.drop().await;
    }

    #[tokio::test]
    async fn postgres_json_extract_0058_up_and_down_preserve_array_path_behavior() {
        let test_db = TestDatabase::create().await;
        let config = postgres_test_config(&test_db);

        let pool =
            connect_test_pool_and_migrate_config(&config, "db::postgres json_extract test pool")
                .await
                .expect("connect and migrate postgres")
                .expect("postgres pool");

        let (array_value, strict_mismatch, malformed_path, volatility): (
            Option<String>,
            Option<String>,
            Option<String>,
            String,
        ) = sqlx::query_as(
            "SELECT
                json_extract('{\"items\":[{\"id\":\"first\"}]}'::jsonb, '$.items[0].id'),
                json_extract('{\"phase_gate\":[{\"run_id\":\"run-array\"}]}'::jsonb, '$.phase_gate.run_id'),
                json_extract('{}'::jsonb, '$['),
                (SELECT provolatile::text
                 FROM pg_proc
                 WHERE oid = 'json_extract(jsonb,text)'::regprocedure)",
        )
        .fetch_one(&pool)
        .await
        .expect("query 0058 json_extract up behavior");
        assert_eq!(array_value.as_deref(), Some("first"));
        assert_eq!(strict_mismatch, None);
        assert_eq!(malformed_path, None);
        assert_eq!(volatility, "s");

        POSTGRES_MIGRATOR
            .undo(&pool, 57)
            .await
            .expect("undo 0058 json_extract migration");

        let (
            down_literal_value,
            down_array_value,
            down_lax_value,
            down_malformed_path,
            down_volatility,
        ): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
        ) = sqlx::query_as(
            "SELECT
                json_extract('{\"phase_gate\":{\"run_id\":\"run-object\"}}'::jsonb, '$.phase_gate.run_id'),
                json_extract('{\"items\":[{\"id\":\"first\"}]}'::jsonb, '$.items[0].id'),
                json_extract('{\"phase_gate\":[{\"run_id\":\"run-array\"}]}'::jsonb, '$.phase_gate.run_id'),
                json_extract('{}'::jsonb, '$['),
                (SELECT provolatile::text
                 FROM pg_proc
                 WHERE oid = 'json_extract(jsonb,text)'::regprocedure)",
        )
        .fetch_one(&pool)
        .await
        .expect("query 0058 json_extract down behavior");
        // After `down`, json_extract is restored verbatim from
        // 0002_sqlite_compat_functions.sql: literal-key navigation only
        // (the path regex `^\$((\.[A-Za-z0-9_]+)*)$` rejects anything with
        // brackets), no jsonpath operators, and IMMUTABLE volatility.
        // Positive case: pure literal-key navigation still works (this is
        // the legacy 0057-era contract we are restoring).
        assert_eq!(down_literal_value.as_deref(), Some("run-object"));
        // Negative cases: bracket paths and lax auto-unwrapping are gone.
        assert_eq!(down_array_value, None);
        assert_eq!(down_lax_value, None);
        assert_eq!(down_malformed_path, None);
        assert_eq!(down_volatility, "i");

        close_test_pool(pool, "db::postgres json_extract test pool")
            .await
            .expect("close postgres pool");
        test_db.drop().await;
    }

    /// #2224 follow-up: the migration 0058 fix replaced `WHEN OTHERS` with
    /// an explicit list of jsonpath SQLSTATE classes so non-JSON failure
    /// modes (statement timeouts, cancellations, OOM) propagate to the
    /// caller instead of being swallowed into a silent NULL. Without a
    /// regression test the original concern — operators losing visibility
    /// into a timed-out json_extract query — could be silently reintroduced
    /// by a future "let's go back to WHEN OTHERS" refactor.
    ///
    /// We can't easily simulate OOM, but `statement_timeout` is the
    /// canonical signal we care about and is cheap to trigger: a tight
    /// timeout + a jsonpath against a deeply-nested payload (or a
    /// pg_sleep wrapper) will exhaust the budget mid-evaluation.
    #[tokio::test]
    async fn postgres_json_extract_0058_propagates_statement_timeout_not_silent_null() {
        let test_db = TestDatabase::create().await;
        let config = postgres_test_config(&test_db);

        let pool =
            connect_test_pool_and_migrate_config(&config, "db::postgres json_extract timeout pool")
                .await
                .expect("connect and migrate postgres")
                .expect("postgres pool");

        // Tight per-statement timeout. Run the timeout-bound query in a
        // separate `query` call so the SET LOCAL stays scoped to the
        // dedicated statement.
        let mut conn = pool.acquire().await.expect("acquire pg connection");
        sqlx::query("BEGIN")
            .execute(&mut *conn)
            .await
            .expect("begin");
        sqlx::query("SET LOCAL statement_timeout = '5ms'")
            .execute(&mut *conn)
            .await
            .expect("set short statement_timeout");

        // Force a wait long enough to outlast the 5ms budget. We can't
        // make `json_extract` itself slow in a deterministic way, but we
        // can compose with `pg_sleep` inside the same statement so the
        // post-fix code path observes the `query_canceled` SQLSTATE
        // *during* the json_extract evaluation chain. If a future
        // refactor reintroduces `WHEN OTHERS`, this errors becomes a
        // silent NULL and the assertion below fails.
        let result: Result<Option<String>, sqlx::Error> = sqlx::query_scalar(
            "SELECT json_extract(
                ('{\"x\":' || (SELECT pg_sleep(0.5)::text) || '}')::jsonb,
                '$.x'
            )",
        )
        .fetch_one(&mut *conn)
        .await;

        // Roll back the test tx whether or not the statement aborted so
        // we don't leave a poisoned tx behind for `close_test_pool`.
        let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;

        match result {
            Err(sqlx::Error::Database(db_err)) if db_err.code().as_deref() == Some("57014") => {
                // 57014 = `query_canceled` — the SQLSTATE PostgreSQL
                // raises for statement_timeout. This is exactly the
                // signal the WHEN OTHERS fix was meant to preserve.
            }
            Err(sqlx::Error::Database(db_err)) => {
                panic!(
                    "expected statement_timeout (SQLSTATE 57014) to surface; got {:?} (code={:?})",
                    db_err.message(),
                    db_err.code()
                );
            }
            Err(other) => {
                panic!("expected DatabaseError; got {other:?}");
            }
            Ok(value) => {
                panic!(
                    "json_extract silently swallowed statement_timeout — got Ok({value:?}) instead of query_canceled. \
                     Regression of #2224 fix: WHEN OTHERS catch-all reintroduced."
                );
            }
        }

        drop(conn);
        close_test_pool(pool, "db::postgres json_extract timeout pool")
            .await
            .expect("close postgres pool");
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
            aliases: Vec::new(),
            wake_word: None,
            voice_enabled: true,
            sensitivity_mode: None,
            voice: crate::config::AgentVoiceConfig::default(),
            provider: "codex".to_string(),
            channels: crate::config::AgentChannels {
                codex: Some(crate::config::AgentChannel::from("maker-cdx")),
                ..Default::default()
            },
            keywords: Vec::new(),
            department: Some("engineering".to_string()),
            avatar_emoji: Some("🛠️".to_string()),
            preferred_intake_node_labels: None,
        }];

        let pool =
            connect_test_pool_and_migrate_config(&config, "db::postgres legacy reseed test pool")
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
            .bind("turn_active")
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
                aliases: Vec::new(),
                wake_word: None,
                voice_enabled: true,
                sensitivity_mode: None,
                voice: crate::config::AgentVoiceConfig::default(),
                provider: "codex".to_string(),
                channels: crate::config::AgentChannels {
                    codex: Some(crate::config::AgentChannel::from("maker-cdx")),
                    ..Default::default()
                },
                keywords: Vec::new(),
                department: Some("engineering".to_string()),
                avatar_emoji: None,
                preferred_intake_node_labels: None,
            },
            crate::config::AgentDef {
                id: "openclaw-maker".to_string(),
                name: "Legacy Maker".to_string(),
                name_ko: None,
                aliases: Vec::new(),
                wake_word: None,
                voice_enabled: true,
                sensitivity_mode: None,
                voice: crate::config::AgentVoiceConfig::default(),
                provider: "codex".to_string(),
                channels: crate::config::AgentChannels {
                    codex: Some(crate::config::AgentChannel::from("legacy-cdx")),
                    ..Default::default()
                },
                keywords: Vec::new(),
                department: Some("legacy".to_string()),
                avatar_emoji: None,
                preferred_intake_node_labels: None,
            },
        ];

        let pool = connect_test_pool_and_migrate_config(
            &config,
            "db::postgres configured legacy reseed test pool",
        )
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
    async fn postgres_startup_reseed_clears_session_fk_for_removed_agent() {
        // Regression: a stale agent left in postgres with a sessions row referencing it
        // used to break startup with
        //   `delete postgres agent <id>: ... foreign key constraint "sessions_agent_id_fkey"`
        // because clear_agent_fk_references_pg() forgot to NULL out sessions.agent_id.
        let test_db = TestDatabase::create().await;
        let config = postgres_test_config(&test_db);

        let pool = connect_test_pool_and_migrate_config(
            &config,
            "db::postgres stale agent session fk test pool",
        )
        .await
        .expect("connect and migrate postgres")
        .expect("postgres pool");

        sqlx::query(
            "INSERT INTO agents (
                id, name, provider, status, xp, sprite_number, description, system_prompt, pipeline_config
             ) VALUES ($1, 'Stale E2E', 'codex', 'idle', 0, 1, '', '', '{}')",
        )
        .bind("adk-dashboard-e2e")
        .execute(&pool)
        .await
        .expect("insert stale agent");

        sqlx::query("INSERT INTO sessions (session_key, agent_id, status) VALUES ($1, $2, $3)")
            .bind("stale-sess-1")
            .bind("adk-dashboard-e2e")
            .bind("disconnected")
            .execute(&pool)
            .await
            .expect("insert session referencing stale agent");

        startup_reseed(&pool, &config)
            .await
            .expect("startup reseed must succeed even when sessions reference the removed agent");

        let stale_agent_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agents WHERE id = $1")
                .bind("adk-dashboard-e2e")
                .fetch_one(&pool)
                .await
                .expect("count stale agents");
        assert_eq!(
            stale_agent_count, 0,
            "stale agent must be deleted by reseed"
        );

        let session_agent: Option<String> =
            sqlx::query_scalar("SELECT agent_id FROM sessions WHERE session_key = $1")
                .bind("stale-sess-1")
                .fetch_one(&pool)
                .await
                .expect("load session agent after reseed");
        assert!(
            session_agent.is_none(),
            "sessions.agent_id must be nulled when the referenced agent is removed"
        );

        close_test_pool(pool, "db::postgres stale agent session fk test pool")
            .await
            .expect("close postgres pool");
        test_db.drop().await;
    }

    #[tokio::test]
    async fn postgres_advisory_lock_lease_allows_only_one_live_holder() {
        let test_db = TestDatabase::create().await;
        let config = postgres_test_config(&test_db);

        let pool = connect_test_pool_and_migrate_config(
            &config,
            "db::postgres advisory singleton test pool",
        )
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

        let pool = connect_test_pool_and_migrate_config(
            &config,
            "db::postgres advisory pool exhaustion test pool",
        )
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

        let pool_a =
            connect_test_pool_and_migrate_config(&config, "db::postgres advisory drop test pool A")
                .await
                .expect("connect and migrate postgres pool A")
                .expect("postgres pool A");
        let pool_b =
            connect_test_pool_and_migrate_config(&config, "db::postgres advisory drop test pool B")
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn postgres_startup_advisory_lock_serializes_concurrent_startup_sections() {
        let test_db = TestDatabase::create().await;
        let config = postgres_test_config(&test_db);

        let pool_a =
            connect_test_pool_and_migrate_config(&config, "db::postgres startup lock test pool A")
                .await
                .expect("connect and migrate postgres pool A")
                .expect("postgres pool A");
        let pool_b =
            connect_test_pool_and_migrate_config(&config, "db::postgres startup lock test pool B")
                .await
                .expect("connect and migrate postgres pool B")
                .expect("postgres pool B");

        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let events = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let first_entered = Arc::new(Notify::new());
        let release_first = Arc::new(Notify::new());

        let pool_a_task = pool_a.clone();
        let active_a = Arc::clone(&active);
        let max_active_a = Arc::clone(&max_active);
        let events_a = Arc::clone(&events);
        let first_entered_a = Arc::clone(&first_entered);
        let release_first_a = Arc::clone(&release_first);
        let config_a = config.clone();
        let first = tokio::spawn(async move {
            with_startup_advisory_lock(&pool_a_task, || async {
                let now = active_a.fetch_add(1, Ordering::SeqCst) + 1;
                max_active_a.fetch_max(now, Ordering::SeqCst);
                events_a.lock().await.push("first-enter");
                first_entered_a.notify_one();
                release_first_a.notified().await;
                startup_reseed(&pool_a_task, &config_a).await?;
                events_a.lock().await.push("first-exit");
                active_a.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            })
            .await
        });

        first_entered.notified().await;

        let pool_b_task = pool_b.clone();
        let active_b = Arc::clone(&active);
        let max_active_b = Arc::clone(&max_active);
        let events_b = Arc::clone(&events);
        let config_b = config.clone();
        let second = tokio::spawn(async move {
            with_startup_advisory_lock(&pool_b_task, || async {
                let now = active_b.fetch_add(1, Ordering::SeqCst) + 1;
                max_active_b.fetch_max(now, Ordering::SeqCst);
                events_b.lock().await.push("second-enter");
                startup_reseed(&pool_b_task, &config_b).await?;
                active_b.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            })
            .await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            active.load(Ordering::SeqCst),
            1,
            "the second startup section must wait while the first lock holder is active"
        );

        release_first.notify_one();
        first
            .await
            .expect("first startup task joins")
            .expect("first startup lock section");
        second
            .await
            .expect("second startup task joins")
            .expect("second startup lock section");

        assert_eq!(
            max_active.load(Ordering::SeqCst),
            1,
            "startup lock must prevent overlapping startup mutation sections"
        );
        assert_eq!(
            events.lock().await.as_slice(),
            ["first-enter", "first-exit", "second-enter"]
        );

        close_test_pool(pool_b, "db::postgres startup lock pool B")
            .await
            .expect("close pool B");
        close_test_pool(pool_a, "db::postgres startup lock pool A")
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
