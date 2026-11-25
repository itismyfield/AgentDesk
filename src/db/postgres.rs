use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Row};

use crate::config::Config;

mod advisory_lock;
mod agent_sync;
mod backpressure;
#[cfg(test)]
use backpressure::should_yield_for_counters;
pub(crate) use backpressure::{PressureSnapshot, pressure_snapshot};
#[cfg(test)]
mod intake_timing_pg_tests;
mod pool_return;
#[cfg(test)]
mod pool_return_pg_tests;
pub use advisory_lock::AdvisoryLockLease;
pub(crate) use advisory_lock::PooledAdvisoryLock;
pub use agent_sync::sync_agents_from_config_pg;
use pool_return::{RETURN_CLEANUP_BUDGET, with_return_cleanup};
pub(crate) mod shared_config;

static POSTGRES_MIGRATOR: Migrator = sqlx::migrate!("../../migrations/postgres");
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
    pressure_snapshot(pool).should_yield()
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
    /// The database carries migrations newer than this binary embeds; retrying cannot help.
    SchemaAhead,
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

    pub(crate) fn schema_ahead(message: impl Into<String>) -> Self {
        Self {
            kind: PgConnectFailureKind::SchemaAhead,
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
    let options = PgPoolOptions::new()
        .max_connections(settings.max_connections)
        .acquire_timeout(settings.acquire_timeout)
        .idle_timeout(settings.idle_timeout)
        .max_lifetime(settings.max_lifetime)
        .test_before_acquire(settings.test_before_acquire)
        // Emitted only when a subscriber enables it: the intake timing layer, under an intake trace.
        .acquire_time_level(log::LevelFilter::Debug);
    with_return_cleanup(options, RETURN_CLEANUP_BUDGET)
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
    async {
        let mut connection = pool.acquire().await?;
        // SQLx can exit before releasing its session advisory lock on error or cancellation.
        connection.close_on_drop();
        // A direct call avoids the generic Acquire lifetime in the spawned boot future.
        POSTGRES_MIGRATOR
            .run_direct(None, &mut *connection, false)
            .await
    }
    .await
    .map_err(|error| format!("run postgres migrations: {error}"))
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

#[cfg(test)]
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

fn checksum_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
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

fn database_url_override() -> Option<String> {
    std::env::var("DATABASE_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(any(test, feature = "test-support"))]
/// Read the shared PG fixture base; required PG lanes must not silently turn
/// a missing base into a soft-skip. Every fixture that creates a database must
/// use this authority; callers that can skip return `None`, while required
/// lanes still get the hard failure below when `AGENTDESK_REQUIRE_PG=1`.
pub(crate) fn postgres_test_database_url_base() -> Option<String> {
    let override_value = FIXTURE_BASE_OVERRIDE.with(|slot| slot.borrow().clone());
    let base = override_value.unwrap_or_else(|| {
        std::env::var("POSTGRES_TEST_DATABASE_URL_BASE")
            .ok()
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty())
    });
    if base.is_none() && std::env::var(AGENTDESK_REQUIRE_PG_ENV).ok().as_deref() == Some("1") {
        panic!("PG required but POSTGRES_TEST_DATABASE_URL_BASE unset"); // agentdesk-audit: allow-unwrap — AGENTDESK_REQUIRE_PG=1 CI 계약: PG fixture base 누락을 soft-skip green으로 붕괴시키지 않는 의도적 hard-fail (#4979 S2)
    }
    base
}

#[cfg(any(test, feature = "test-support"))]
mod test_db_reclaim;
#[cfg(any(test, feature = "test-support"))]
const TEST_POSTGRES_OP_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(any(test, feature = "test-support"))]
const TEST_POSTGRES_POOL_MAX_CONNECTIONS: u32 = 1;
#[cfg(any(test, feature = "test-support"))]
const TEST_POSTGRES_ADMIN_POOL_MAX_CONNECTIONS: u32 = 1;
#[cfg(any(test, feature = "test-support"))]
const AGENTDESK_REQUIRE_PG_ENV: &str = "AGENTDESK_REQUIRE_PG";
#[cfg(any(test, feature = "test-support"))]
const TEST_POSTGRES_ACQUIRE_TIMEOUT_ENV: &str = "AGENTDESK_TEST_POSTGRES_ACQUIRE_TIMEOUT_MS";
#[cfg(any(test, feature = "test-support"))]
static POSTGRES_TEST_SETUP_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
    std::sync::OnceLock::new();
#[cfg(any(test, feature = "test-support"))]
static POSTGRES_TEST_LIFECYCLE_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
    std::sync::OnceLock::new();
#[cfg(any(test, feature = "test-support"))]
static POSTGRES_TEST_DATABASE_OWNERS: std::sync::OnceLock<
    std::sync::Mutex<BTreeMap<(String, String), TestDatabaseOwnershipToken>>,
> = std::sync::OnceLock::new();
#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static FIXTURE_BASE_OVERRIDE: std::cell::RefCell<Option<Option<String>>> =
        const { std::cell::RefCell::new(None) };

    /// How many `PostgresTestLifecycleGuard`s this thread currently holds.
    ///
    /// The harness has exactly two process-global test mutexes that a `_pg`
    /// test routinely wants at the same time: `config::shared_test_env_lock`
    /// (E) and `POSTGRES_TEST_LIFECYCLE_LOCK` (P). **The canonical hierarchy is
    /// `E -> P`** — the majority of `_pg` tests already take the env lock first
    /// and only then build their database. A test that reverses it closes an
    /// ABBA cycle with any concurrently in-flight `E -> P` test, and because
    /// both are `std::sync::Mutex` the process parks in `__psynch_mutexwait`
    /// forever with no timeout, no deadlock detection, and no frame naming
    /// either offender. That is how a full `cargo test --lib` run stopped dead
    /// at test 4176 with fourteen threads stranded.
    ///
    /// This counter is the P half of the tripwire that replaces that silence:
    /// `config::shared_test_env_lock` reads it and panics — naming the test
    /// thread — the moment a P holder reaches for E. See
    /// `test_lifecycle_lock_held` /
    /// `assert_test_lifecycle_lock_not_held_before_env_lock` below.
    static PG_LIFECYCLE_HELD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Whether this thread currently holds `POSTGRES_TEST_LIFECYCLE_LOCK` (P).
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn test_lifecycle_lock_held() -> bool {
    PG_LIFECYCLE_HELD.with(|depth| depth.get() > 0)
}

/// Panic if this thread is about to take E while already holding P.
///
/// Called from `config::shared_test_env_lock`, so it covers both the canonical
/// `test_env_lock::acquire_shared_test_env_lock` path and the direct
/// `shared_test_env_lock().lock()` sites. It fires *before* the blocking
/// `lock()`, which is what converts a silent, unattributable hang into a red
/// test that names itself.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn assert_test_lifecycle_lock_not_held_before_env_lock() {
    if !test_lifecycle_lock_held() {
        return;
    }
    // libtest names each test thread after the test it runs, so this is the
    // offending test's own name in every normal `cargo test` invocation.
    let offender = std::thread::current()
        .name()
        .unwrap_or("<unnamed test thread>")
        .to_string();
    let message = format!(
        "test lock order inversion in `{offender}`: POSTGRES_TEST_LIFECYCLE_LOCK is already \
         held while acquiring config::shared_test_env_lock. The canonical hierarchy is \
         E -> P: take the shared test-env lock (or the fixture that owns it) *before* \
         creating the PostgreSQL test database, not after."
    );
    panic!("{message}"); // agentdesk-audit: allow-unwrap — #[cfg(test)]-gated tripwire, never compiled into production; the panic converts a silent ABBA hang into a red test naming the offender
}

#[cfg(any(test, feature = "test-support"))]
struct TestDatabaseOwnershipToken {
    registry_key: (String, String),
    admin_url: String,
    database_name: String,
}

#[cfg(any(test, feature = "test-support"))]
fn test_database_owners()
-> &'static std::sync::Mutex<BTreeMap<(String, String), TestDatabaseOwnershipToken>> {
    POSTGRES_TEST_DATABASE_OWNERS.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()))
}

#[cfg(any(test, feature = "test-support"))]
fn test_database_server_identity(options: &PgConnectOptions) -> String {
    super::fixture_target::server_identity(options)
}

#[cfg(any(test, feature = "test-support"))]
fn test_database_registry_key(options: &PgConnectOptions, database_name: &str) -> (String, String) {
    (
        test_database_server_identity(options),
        database_name.to_string(),
    )
}

#[cfg(any(test, feature = "test-support"))]
fn register_test_database_ownership(
    admin_options: &PgConnectOptions,
    admin_url: &str,
    database_name: &str,
) {
    let registry_key = test_database_registry_key(admin_options, database_name);
    let mut owners = test_database_owners()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        !owners.contains_key(&registry_key),
        "a database ownership token must not be overwritten"
    );
    owners.insert(
        registry_key.clone(),
        TestDatabaseOwnershipToken {
            registry_key,
            admin_url: admin_url.to_string(),
            database_name: database_name.to_string(),
        },
    );
}

#[cfg(any(test, feature = "test-support"))]
fn take_test_database_ownership(
    database_options: &PgConnectOptions,
    database_name: &str,
) -> Option<TestDatabaseOwnershipToken> {
    let registry_key = test_database_registry_key(database_options, database_name);
    test_database_owners()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&registry_key)
}

#[cfg(any(test, feature = "test-support"))]
fn restore_test_database_ownership(ownership: TestDatabaseOwnershipToken) {
    let mut owners = test_database_owners()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if owners.contains_key(&ownership.registry_key) {
        tracing::warn!(
            database_name = ownership.database_name,
            "refusing to overwrite an existing postgres test database ownership token"
        );
        return;
    }
    owners.insert(ownership.registry_key.clone(), ownership);
}

#[cfg(any(test, feature = "test-support"))]
fn lock_test_setup() -> std::sync::MutexGuard<'static, ()> {
    let mutex = POSTGRES_TEST_SETUP_LOCK.get_or_init(|| std::sync::Mutex::new(()));
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) struct PostgresTestLifecycleGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
}

/// Keeps `PG_LIFECYCLE_HELD` in step with real ownership so the E-after-P
/// tripwire cannot go stale. The counter is decremented here and the inner
/// `MutexGuard` releases P immediately afterwards, both on this thread.
#[cfg(any(test, feature = "test-support"))]
impl Drop for PostgresTestLifecycleGuard {
    fn drop(&mut self) {
        crate::test_support::lock_holder::PG_LIFECYCLE_LOCK.release();
        PG_LIFECYCLE_HELD.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

#[cfg(any(test, feature = "test-support"))]
#[track_caller]
pub(crate) fn lock_test_lifecycle() -> PostgresTestLifecycleGuard {
    let mutex = POSTGRES_TEST_LIFECYCLE_LOCK.get_or_init(|| std::sync::Mutex::new(()));
    let guard = crate::test_support::lock_holder::PG_LIFECYCLE_LOCK.hold(|| {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    });
    PG_LIFECYCLE_HELD.with(|depth| depth.set(depth.get() + 1));
    PostgresTestLifecycleGuard { _guard: guard }
}

#[cfg(any(test, feature = "test-support"))]
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

#[cfg(any(test, feature = "test-support"))]
async fn run_test_postgres_sqlx_op<T, F>(label: &str, future: F) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, sqlx::Error>>,
{
    run_test_postgres_sqlx_op_with_timeout(TEST_POSTGRES_OP_TIMEOUT, label, future).await
}

#[cfg(any(test, feature = "test-support"))]
/// Escalate failed shared PostgreSQL test-helper results in PG-backed CI lanes.
/// Direct SQLx connections are outside this funnel and remain hard-fail-only.
/// The six public shared-helper entrypoints call this exactly once at their
/// final `Result` boundary; inner operations stay return-based so cleanup can
/// finish before a required-lane panic.
fn require_pg_guard<T>(result: Result<T, String>) -> Result<T, String> {
    match result {
        Ok(value) => Ok(value),
        Err(error) if std::env::var(AGENTDESK_REQUIRE_PG_ENV).ok().as_deref() == Some("1") => {
            let message = format!(
                "PostgreSQL required by this CI lane (AGENTDESK_REQUIRE_PG=1) but unavailable: {error}"
            );
            panic!("{message}"); // agentdesk-audit: allow-unwrap — AGENTDESK_REQUIRE_PG=1 CI 계약: PG 연결 실패를 soft-skip green으로 붕괴시키지 않기 위한 의도적 hard-fail (#4979 S2)
        }
        Err(error) => Err(error),
    }
}

#[cfg(any(test, feature = "test-support"))]
fn parse_test_postgres_options(
    database_url: &str,
    label: &str,
) -> Result<PgConnectOptions, String> {
    super::fixture_target::parse_fixture_url(database_url, label).map(|parsed| parsed.options)
}

#[cfg(any(test, feature = "test-support"))]
async fn connect_test_pool_with_options(
    options: PgConnectOptions,
    label: &str,
    max_connections: u32,
) -> Result<PgPool, String> {
    super::fixture_target::enforce_configured_target(
        postgres_test_database_url_base().as_deref(),
        &options,
    )?;
    let acquire_timeout = std::env::var(TEST_POSTGRES_ACQUIRE_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(TEST_POSTGRES_OP_TIMEOUT);
    run_test_postgres_sqlx_op(
        &format!("{label} connect postgres"),
        with_return_cleanup(
            PgPoolOptions::new()
                .max_connections(max_connections.max(1))
                .acquire_timeout(acquire_timeout),
            RETURN_CLEANUP_BUDGET,
        )
        .connect_with(options),
    )
    .await
}

#[cfg(any(test, feature = "test-support"))]
async fn connect_test_pool_with_max_connections_inner(
    database_url: &str,
    label: &str,
    max_connections: u32,
) -> Result<PgPool, String> {
    let options = parse_test_postgres_options(database_url, label)?;
    connect_test_pool_with_options(options, label, max_connections).await
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) async fn connect_test_pool_with_max_connections(
    database_url: &str,
    label: &str,
    max_connections: u32,
) -> Result<PgPool, String> {
    require_pg_guard(
        connect_test_pool_with_max_connections_inner(database_url, label, max_connections).await,
    )
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) async fn connect_test_pool(database_url: &str, label: &str) -> Result<PgPool, String> {
    // Test helpers frequently create many isolated pools in parallel on CI.
    // Keep the default test pool lean so PG-backed route tests do not exhaust
    // the shared runner database just by setting up fixtures.
    require_pg_guard(
        connect_test_pool_with_max_connections_inner(
            database_url,
            label,
            TEST_POSTGRES_POOL_MAX_CONNECTIONS,
        )
        .await,
    )
}

#[cfg(any(test, feature = "test-support"))]
// SAFETY (await_holding_lock): `lock_test_setup()` is a std Mutex held across
// the pool-connect/query awaits *on purpose* — it serializes concurrent
// PG-backed test DB create/drop so they cannot race. Dropping the guard before
// the awaits would reintroduce the CI race this lock was added to fix. Test-only.
#[allow(clippy::await_holding_lock)]
/// Created through `test_db_reclaim`, whose first-create sweep reclaims databases of provably dead test processes.
pub(crate) async fn create_test_database(
    admin_url: &str,
    database_name: &str,
    label: &str,
) -> Result<(), String> {
    // CI failures were caused by many PG-backed tests racing to create/drop
    // isolated databases at the same time. Serialize setup/teardown at the
    // shared helper boundary so every test module benefits from the guard.
    let _guard = lock_test_setup();
    let result = async {
        let admin_options = parse_test_postgres_options(admin_url, &format!("{label} admin"))?;
        let admin_pool = connect_test_pool_with_options(
            admin_options.clone(),
            &format!("{label} admin"),
            TEST_POSTGRES_ADMIN_POOL_MAX_CONNECTIONS,
        )
        .await?;
        test_db_reclaim::reclaim_once_per_process(&admin_pool, label).await;
        let create_result = test_db_reclaim::create_marked(&admin_pool, database_name, label).await;

        if let Err(error) = create_result {
            if let Err(close_error) = close_test_pool(admin_pool, &format!("{label} admin")).await {
                tracing::warn!(
                    label,
                    close_error,
                    "failed to close postgres admin pool after test database create error"
                );
            }
            // CREATE failed, so this test never owned `database_name`; in
            // particular, DuplicateDatabase must not trigger a destructive
            // cleanup of a pre-existing database.
            return Err(error);
        }

        // Only a successful CREATE grants this test an ownership token. Every
        // later best-effort or explicit cleanup path must consume that token.
        register_test_database_ownership(&admin_options, admin_url, database_name);

        if let Err(error) = close_test_pool(admin_pool, &format!("{label} admin")).await {
            best_effort_drop_owned_test_database(&admin_options, database_name, label).await;
            return Err(error);
        }
        Ok(())
    }
    .await;
    require_pg_guard(result)
}

#[cfg(any(test, feature = "test-support"))]
async fn migrate_test_pool(pool: &PgPool, label: &str) -> Result<(), String> {
    tokio::time::timeout(TEST_POSTGRES_OP_TIMEOUT, migrate(pool))
        .await
        .map_err(|_| {
            format!(
                "{label} migrate postgres timed out after {}s",
                TEST_POSTGRES_OP_TIMEOUT.as_secs()
            )
        })?
}

#[cfg(any(test, feature = "test-support"))]
fn is_safe_test_database_name(database_name: &str) -> bool {
    !database_name.is_empty()
        && database_name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

#[cfg(any(test, feature = "test-support"))]
async fn best_effort_drop_test_database_with_options(
    admin_options: PgConnectOptions,
    database_name: &str,
    label: &str,
) -> Result<(), TestDatabaseDropError> {
    if !is_safe_test_database_name(database_name) {
        return Err(TestDatabaseDropError::before_drop(format!(
            "{label} unsafe postgres test database name {database_name}"
        )));
    }
    drop_test_database_with_options(admin_options, database_name, label).await
}

#[cfg(any(test, feature = "test-support"))]
struct TestDatabaseDropError {
    message: String,
    drop_succeeded: bool,
}

#[cfg(any(test, feature = "test-support"))]
impl TestDatabaseDropError {
    fn before_drop(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            drop_succeeded: false,
        }
    }

    fn after_drop(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            drop_succeeded: true,
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
async fn drop_owned_test_database_with_options(
    database_options: &PgConnectOptions,
    database_name: &str,
    label: &str,
) -> Result<(), String> {
    if !is_safe_test_database_name(database_name) {
        return Err(format!(
            "{label} unsafe postgres test database name {database_name}"
        ));
    }
    // Every caller is inside the setup-lock critical section (directly or via
    // the wrapper), so this take/restore reservation window is serialized with
    // all test-database create/drop helpers.
    let Some(ownership) = take_test_database_ownership(database_options, database_name) else {
        tracing::debug!(
            label,
            database_name,
            "skipping postgres test database cleanup without an ownership token"
        );
        return Ok(());
    };
    let admin_options =
        match parse_test_postgres_options(&ownership.admin_url, &format!("{label} admin")) {
            Ok(options) => options,
            Err(error) => {
                restore_test_database_ownership(ownership);
                return Err(error);
            }
        };
    let result =
        best_effort_drop_test_database_with_options(admin_options, &ownership.database_name, label)
            .await;
    if let Err(error) = result {
        // Keep the token available for the caller's unwind/backstop retry only
        // when DROP was not confirmed. Once DROP succeeds, a later admin-pool
        // close timeout is still an error, but the already-deleted database's
        // ownership token must remain consumed.
        if !error.drop_succeeded {
            restore_test_database_ownership(ownership);
        }
        return Err(error.message);
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
async fn best_effort_drop_owned_test_database(
    database_options: &PgConnectOptions,
    database_name: &str,
    label: &str,
) {
    if let Err(error) =
        drop_owned_test_database_with_options(database_options, database_name, label).await
    {
        tracing::warn!(
            label,
            database_name,
            error,
            "best-effort postgres test database cleanup failed"
        );
    }
}

#[cfg(any(test, feature = "test-support"))]
async fn best_effort_drop_owned_test_database_for_database_options(
    database_options: PgConnectOptions,
    label: &str,
) {
    let Some(database_name) = database_options.get_database().map(str::to_owned) else {
        return;
    };
    best_effort_drop_owned_test_database(&database_options, &database_name, label).await;
}

#[cfg(any(test, feature = "test-support"))]
/// Migration setup does not hold `POSTGRES_TEST_SETUP_LOCK`; acquire it here
/// before the owned DROP. The create close-failure path calls the unlocked
/// helper while it already holds that lock, and public `drop_test_database`
/// does too; neither path may re-enter this wrapper.
// SAFETY (await_holding_lock): `lock_test_setup()` is a test-only
// serialization mutex; only this PostgreSQL test-helper family contends for it,
// so holding it across the cleanup await cannot block live application code.
#[allow(clippy::await_holding_lock)]
async fn best_effort_drop_owned_test_database_with_setup_lock(
    database_options: PgConnectOptions,
    label: &str,
) {
    let _guard = lock_test_setup();
    best_effort_drop_owned_test_database_for_database_options(database_options, label).await;
}

#[cfg(any(test, feature = "test-support"))]
/// Close a failed migration pool and best-effort drop only a database whose
/// successful CREATE left an ownership token in this test process.
async fn best_effort_cleanup_after_migration_failure(
    pool: PgPool,
    database_options: PgConnectOptions,
    label: &str,
) {
    if let Err(error) = close_test_pool(pool, &format!("{label} migration failure cleanup")).await {
        tracing::warn!(
            label,
            error,
            "failed to close postgres test pool after migration failure"
        );
    }
    best_effort_drop_owned_test_database_with_setup_lock(database_options, label).await;
}

#[cfg(any(test, feature = "test-support"))]
async fn connect_test_pool_and_migrate_inner(
    database_url: &str,
    label: &str,
) -> Result<PgPool, String> {
    let database_options = parse_test_postgres_options(database_url, label)?;
    let pool = connect_test_pool_with_options(
        database_options.clone(),
        label,
        TEST_POSTGRES_POOL_MAX_CONNECTIONS,
    )
    .await?;
    if let Err(error) = migrate_test_pool(&pool, label).await {
        best_effort_cleanup_after_migration_failure(pool, database_options, label).await;
        return Err(error);
    }
    Ok(pool)
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) async fn connect_test_pool_and_migrate(
    database_url: &str,
    label: &str,
) -> Result<PgPool, String> {
    require_pg_guard(connect_test_pool_and_migrate_inner(database_url, label).await)
}

#[cfg(any(test, feature = "test-support"))]
async fn connect_test_pool_with_max_connections_and_migrate_inner(
    database_url: &str,
    label: &str,
    max_connections: u32,
) -> Result<PgPool, String> {
    let database_options = parse_test_postgres_options(database_url, label)?;
    let pool =
        connect_test_pool_with_options(database_options.clone(), label, max_connections).await?;
    if let Err(error) = migrate_test_pool(&pool, label).await {
        best_effort_cleanup_after_migration_failure(pool, database_options, label).await;
        return Err(error);
    }
    Ok(pool)
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) async fn connect_test_pool_with_max_connections_and_migrate(
    database_url: &str,
    label: &str,
    max_connections: u32,
) -> Result<PgPool, String> {
    require_pg_guard(
        connect_test_pool_with_max_connections_and_migrate_inner(
            database_url,
            label,
            max_connections,
        )
        .await,
    )
}

#[cfg(test)]
fn test_database_config_options(config: &Config) -> PgConnectOptions {
    let mut options = PgConnectOptions::new()
        .host(&config.database.host)
        .port(config.database.port)
        .username(&config.database.user)
        .database(&config.database.dbname);
    if let Some(password) = config.database.password.as_deref() {
        options = options.password(password);
    }
    options
}

#[cfg(test)]
async fn connect_test_pool_and_migrate_config_inner(
    config: &Config,
    label: &str,
) -> Result<Option<PgPool>, String> {
    if !database_enabled(config) {
        return Ok(None);
    }

    let options = test_database_config_options(config);

    let pool =
        connect_test_pool_with_options(options.clone(), label, config.database.pool_max.max(1))
            .await?;
    if let Err(error) = migrate_test_pool(&pool, label).await {
        best_effort_cleanup_after_migration_failure(pool, options, label).await;
        return Err(error);
    }
    Ok(Some(pool))
}

#[cfg(test)]
/// `database.enabled = false` intentionally returns `Ok(None)` before any
/// PostgreSQL connection attempt, even when `AGENTDESK_REQUIRE_PG=1`.
pub(crate) async fn connect_test_pool_and_migrate_config(
    config: &Config,
    label: &str,
) -> Result<Option<PgPool>, String> {
    require_pg_guard(connect_test_pool_and_migrate_config_inner(config, label).await)
}

#[cfg(any(test, feature = "test-support"))]
/// The primitive intentionally does not acquire `POSTGRES_TEST_SETUP_LOCK`:
/// create's close-failure path calls it while that lock is held. Callers
/// outside that critical section must use the setup-lock wrapper above.
async fn drop_test_database_with_options(
    admin_options: PgConnectOptions,
    database_name: &str,
    label: &str,
) -> Result<(), TestDatabaseDropError> {
    let admin_pool = match connect_test_pool_with_options(
        admin_options,
        &format!("{label} admin"),
        TEST_POSTGRES_ADMIN_POOL_MAX_CONNECTIONS,
    )
    .await
    {
        Ok(pool) => pool,
        Err(error) => return Err(TestDatabaseDropError::before_drop(error)),
    };
    if let Err(error) = run_test_postgres_sqlx_op(
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
    .await
    {
        let message = match close_test_pool(admin_pool, &format!("{label} admin")).await {
            Ok(()) => error,
            Err(close_error) => {
                format!("{error}; failed to close postgres admin pool: {close_error}")
            }
        };
        return Err(TestDatabaseDropError::before_drop(message));
    }
    let drop_sql = format!("DROP DATABASE IF EXISTS \"{database_name}\" WITH (FORCE)");
    let mut last_error = None;
    for attempt in 1..=3 {
        match run_test_postgres_sqlx_op(
            &format!("{label} drop postgres test db {database_name}"),
            sqlx::query(sqlx::AssertSqlSafe(drop_sql.as_str())).execute(&admin_pool),
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
        let message = match close_test_pool(admin_pool, &format!("{label} admin")).await {
            Ok(()) => error,
            Err(close_error) => {
                format!("{error}; failed to close postgres admin pool: {close_error}")
            }
        };
        return Err(TestDatabaseDropError::before_drop(message));
    }
    // The SQL DROP has succeeded at this point. A later pool-close failure is
    // reported as an error, but must not make the caller restore the token for
    // a database that is already gone.
    if let Err(error) = close_test_pool(admin_pool, &format!("{label} admin")).await {
        return Err(TestDatabaseDropError::after_drop(error));
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
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
    let admin_options = parse_test_postgres_options(admin_url, &format!("{label} admin"))?;
    // The token's admin URL remains authoritative inside the owned helper:
    // callers cannot redirect cleanup to a database they did not create.
    drop_owned_test_database_with_options(&admin_options, database_name, label).await
}

#[cfg(any(test, feature = "test-support"))]
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
mod tests;
