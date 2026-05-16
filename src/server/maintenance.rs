use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone, Utc};
use serde::Serialize;
use sqlx::{PgPool, Row};

type MaintenanceFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;
type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

const DEFAULT_SCHEDULER_TICK: Duration = Duration::from_secs(1);
const EXTRA_STAGGER_PER_JOB: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug)]
pub(crate) struct MaintenanceSchedule {
    every: Duration,
    startup_stagger: Duration,
}

impl MaintenanceSchedule {
    pub(crate) const fn every(every: Duration, startup_stagger: Duration) -> Self {
        Self {
            every,
            startup_stagger,
        }
    }

    fn every_ms(self) -> i64 {
        duration_millis_i64(self.every)
    }

    fn startup_stagger_ms(self) -> i64 {
        duration_millis_i64(self.startup_stagger)
    }
}

pub(crate) trait MaintenanceJob: Send + Sync {
    fn name(&self) -> &'static str;
    fn schedule(&self) -> MaintenanceSchedule;
    fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a>;
}

#[derive(Clone)]
pub(crate) struct MaintenanceJobRegistry {
    jobs: Arc<Vec<Arc<dyn MaintenanceJob>>>,
}

impl MaintenanceJobRegistry {
    pub(crate) fn new(jobs: Vec<Arc<dyn MaintenanceJob>>) -> Self {
        Self {
            jobs: Arc::new(jobs),
        }
    }

    fn static_registry() -> Self {
        Self::static_registry_with_config(crate::config::PromptManifestRetentionConfig::default())
    }

    fn static_registry_with_config(
        prompt_manifest_retention: crate::config::PromptManifestRetentionConfig,
    ) -> Self {
        Self::new(vec![
            Arc::new(NoopHeartbeatJob),
            Arc::new(AgentQualityRollupJob),
            Arc::new(QualityRegressionAlerterJob),
            Arc::new(CancelTombstonePruneJob),
            Arc::new(PromptManifestRetentionJob::new(prompt_manifest_retention)),
            Arc::new(VoiceAnnounceMetaGcJob),
        ])
    }

    fn jobs(&self) -> &[Arc<dyn MaintenanceJob>] {
        self.jobs.as_ref().as_slice()
    }
}

struct NoopHeartbeatJob;

impl MaintenanceJob for NoopHeartbeatJob {
    fn name(&self) -> &'static str {
        "maintenance.noop_heartbeat"
    }

    fn schedule(&self) -> MaintenanceSchedule {
        MaintenanceSchedule::every(Duration::from_secs(15 * 60), Duration::from_secs(10))
    }

    fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a> {
        Box::pin(async move {
            let _ = pool;
            tracing::info!(job = self.name(), "maintenance noop heartbeat fired");
            Ok(())
        })
    }
}

struct AgentQualityRollupJob;

impl MaintenanceJob for AgentQualityRollupJob {
    fn name(&self) -> &'static str {
        "agent_quality_rollup"
    }

    fn schedule(&self) -> MaintenanceSchedule {
        MaintenanceSchedule::every(Duration::from_secs(60 * 60), Duration::from_secs(0))
    }

    fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a> {
        Box::pin(async move {
            let report = crate::services::observability::run_agent_quality_rollup_pg(pool).await?;
            tracing::info!(
                job = self.name(),
                upserted_rows = report.upserted_rows,
                alert_count = report.alert_count,
                "agent quality rollup completed"
            );
            Ok(())
        })
    }
}

/// #1104 (911-4) hourly regression rule engine.
///
/// Runs the rule engine in `services::agent_quality::regression_alerts`
/// against the `agent_quality_daily` rollup. The 15s startup stagger keeps
/// it sequenced AFTER `agent_quality_rollup` (stagger=0) on the same tick
/// so alerts always evaluate against the freshest aggregates.
struct QualityRegressionAlerterJob;

impl MaintenanceJob for QualityRegressionAlerterJob {
    fn name(&self) -> &'static str {
        "quality_regression_alerter"
    }

    fn schedule(&self) -> MaintenanceSchedule {
        MaintenanceSchedule::every(Duration::from_secs(60 * 60), Duration::from_secs(15))
    }

    fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a> {
        Box::pin(async move {
            let sent =
                crate::services::agent_quality::regression_alerts::run_regression_alerter_pg(pool)
                    .await?;
            tracing::info!(
                job = self.name(),
                alerts_dispatched = sent,
                "agent quality regression alerter completed"
            );
            Ok(())
        })
    }
}

/// #1309 — sweep expired `cancel_tombstones` rows every 30 minutes so the
/// table cannot grow without bound when no watcher ever observes the
/// cancel-induced death. The 10-minute TTL × 3 safety margin is generous so
/// repeated PG-enabled idle channels do not retain stale rows.
struct CancelTombstonePruneJob;

impl MaintenanceJob for CancelTombstonePruneJob {
    fn name(&self) -> &'static str {
        "storage.cancel_tombstone_prune"
    }

    fn schedule(&self) -> MaintenanceSchedule {
        MaintenanceSchedule::every(Duration::from_secs(30 * 60), Duration::from_secs(20))
    }

    fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a> {
        Box::pin(async move {
            let deleted = crate::db::cancel_tombstones::prune_expired_cancel_tombstones(pool)
                .await
                .map_err(|error| anyhow::anyhow!("cancel_tombstone_prune failed: {error}"))?;
            if deleted > 0 {
                tracing::info!(
                    job = self.name(),
                    deleted,
                    "[maintenance] cancel_tombstone_prune removed expired rows"
                );
            }
            Ok(())
        })
    }
}

/// #1699 — daily sweep over `prompt_manifest_layers` to enforce the
/// retention policy from `agentdesk.yaml::prompt_manifest_retention`.
/// Trims `full_content` for rows older than `full_content_days`, preserves
/// `content_sha256` and metadata.
struct PromptManifestRetentionJob {
    config: crate::config::PromptManifestRetentionConfig,
}

impl PromptManifestRetentionJob {
    fn new(config: crate::config::PromptManifestRetentionConfig) -> Self {
        Self { config }
    }
}

impl MaintenanceJob for PromptManifestRetentionJob {
    fn name(&self) -> &'static str {
        "storage.prompt_manifest_retention"
    }

    fn schedule(&self) -> MaintenanceSchedule {
        // Daily cadence with a small startup stagger so it doesn't pile on
        // top of the other storage jobs at boot.
        MaintenanceSchedule::every(Duration::from_secs(24 * 60 * 60), Duration::from_secs(45))
    }

    fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a> {
        Box::pin(async move {
            if !self.config.enabled {
                tracing::debug!(
                    job = self.name(),
                    "skipping prompt_manifest_retention (disabled by config)"
                );
                return Ok(());
            }
            let report =
                crate::db::prompt_manifests::apply_retention_policy(pool, &self.config, false)
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("prompt_manifest_retention failed: {error}")
                    })?;
            if report.trimmed_full_content > 0 {
                tracing::info!(
                    job = self.name(),
                    trimmed = report.trimmed_full_content,
                    horizon_at = ?report.horizon_at,
                    "[maintenance] prompt_manifest_retention trimmed full content"
                );
            }
            Ok(())
        })
    }
}

/// #2209 finding #3 — sweep `voice_transcript_announce_meta` rows older
/// than the durable-announce TTL (10 minutes). Runs every 15 minutes on
/// the leader so the table cannot grow without bound when producers
/// occasionally insert metadata that no intake ever consumes (e.g. shard
/// failover, intake crash before `consume_durable`). Conservative 60-second
/// stagger keeps it sequenced after the other storage jobs at boot.
struct VoiceAnnounceMetaGcJob;

impl MaintenanceJob for VoiceAnnounceMetaGcJob {
    fn name(&self) -> &'static str {
        "storage.voice_announce_meta_gc"
    }

    fn schedule(&self) -> MaintenanceSchedule {
        MaintenanceSchedule::every(Duration::from_secs(15 * 60), Duration::from_secs(60))
    }

    fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a> {
        Box::pin(async move {
            let ttl = Duration::from_secs(
                crate::voice::announce_meta::DURABLE_ANNOUNCE_META_TTL_SECS as u64,
            );
            let deleted = crate::voice::announce_meta::gc_expired_voice_announce_meta_pg(pool, ttl)
                .await
                .map_err(|error| anyhow::anyhow!("voice_announce_meta_gc failed: {error}"))?;
            if deleted > 0 {
                tracing::info!(
                    job = self.name(),
                    deleted,
                    "[maintenance] voice_announce_meta_gc removed expired rows"
                );
            }
            Ok(())
        })
    }
}

trait MaintenanceJobStore: Send + Sync {
    fn read<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<String>>;
    fn upsert<'a>(&'a self, key: &'a str, value: &'a str) -> StoreFuture<'a, ()>;
}

struct PgMaintenanceJobStore {
    pool: PgPool,
}

impl PgMaintenanceJobStore {
    fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl MaintenanceJobStore for PgMaintenanceJobStore {
    fn read<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<String>> {
        Box::pin(async move {
            let row = sqlx::query("SELECT value FROM kv_meta WHERE key = $1")
                .bind(key)
                .fetch_optional(&self.pool)
                .await
                .map_err(|error| format!("read maintenance kv_meta {key}: {error}"))?;

            match row {
                Some(row) => row
                    .try_get::<Option<String>, _>("value")
                    .map_err(|error| format!("decode maintenance kv_meta {key}: {error}")),
                None => Ok(None),
            }
        })
    }

    fn upsert<'a>(&'a self, key: &'a str, value: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            sqlx::query(
                "INSERT INTO kv_meta (key, value)
                 VALUES ($1, $2)
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            )
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|error| format!("upsert maintenance kv_meta {key}: {error}"))
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MaintenanceJobStatus {
    id: String,
    name: String,
    enabled: bool,
    schedule: MaintenanceScheduleStatus,
    state: MaintenanceJobState,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MaintenanceScheduleStatus {
    kind: &'static str,
    every_ms: i64,
    startup_stagger_ms: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MaintenanceJobState {
    status: &'static str,
    last_status: String,
    last_run_at_ms: Option<i64>,
    last_run_at: Option<String>,
    next_run_at_ms: Option<i64>,
    next_run_at: Option<String>,
    last_duration_ms: Option<i64>,
    last_error: Option<String>,
    run_count: i64,
    failure_count: i64,
}

pub(crate) async fn scheduler_loop(
    pg_pool: Arc<PgPool>,
    prompt_manifest_retention: crate::config::PromptManifestRetentionConfig,
) {
    let registry = MaintenanceJobRegistry::static_registry_with_config(prompt_manifest_retention);
    let store: Arc<dyn MaintenanceJobStore> =
        Arc::new(PgMaintenanceJobStore::new(pg_pool.as_ref().clone()));

    tracing::info!(
        job_count = registry.jobs().len(),
        "maintenance scheduler started"
    );
    run_scheduler_loop(
        pg_pool.as_ref().clone(),
        registry,
        store,
        DEFAULT_SCHEDULER_TICK,
        None,
    )
    .await;
}

pub(crate) async fn list_job_statuses_pg(pg_pool: PgPool) -> Vec<MaintenanceJobStatus> {
    let store: Arc<dyn MaintenanceJobStore> = Arc::new(PgMaintenanceJobStore::new(pg_pool));
    build_job_statuses(&MaintenanceJobRegistry::static_registry(), store).await
}

async fn run_scheduler_loop(
    pg_pool: PgPool,
    registry: MaintenanceJobRegistry,
    store: Arc<dyn MaintenanceJobStore>,
    tick_interval: Duration,
    max_completed_runs: Option<usize>,
) {
    let mut next_runs = initialize_next_runs(&registry, store.clone()).await;
    let mut completed_runs = 0usize;

    loop {
        let now = Utc::now();
        for (index, job) in registry.jobs().iter().enumerate() {
            let job_name = job.name().to_string();
            let next_run = match next_runs.get(&job_name).copied() {
                Some(value) => value,
                None => startup_next_run(job.schedule(), None, now, index),
            };

            if now < next_run {
                continue;
            }

            run_job_once(pg_pool.clone(), job.clone(), store.clone()).await;
            completed_runs = completed_runs.saturating_add(1);

            let next = add_duration(Utc::now(), job.schedule().every);
            next_runs.insert(job_name, next);
            write_metric_ignore(
                store.as_ref(),
                &kv_key(job.name(), "next_run_ms"),
                &datetime_to_millis(next).to_string(),
            )
            .await;

            if max_completed_runs.is_some_and(|limit| completed_runs >= limit) {
                return;
            }
        }

        tokio::time::sleep(tick_interval).await;
    }
}

async fn initialize_next_runs(
    registry: &MaintenanceJobRegistry,
    store: Arc<dyn MaintenanceJobStore>,
) -> std::collections::HashMap<String, DateTime<Utc>> {
    let now = Utc::now();
    let mut next_runs = std::collections::HashMap::new();

    for (index, job) in registry.jobs().iter().enumerate() {
        let last_run_ms = read_i64(store.as_ref(), &kv_key(job.name(), "last_run_ms")).await;
        let last_run = last_run_ms.and_then(datetime_from_millis);
        let next_run = startup_next_run(job.schedule(), last_run, now, index);

        write_metric_ignore(
            store.as_ref(),
            &kv_key(job.name(), "next_run_ms"),
            &datetime_to_millis(next_run).to_string(),
        )
        .await;
        next_runs.insert(job.name().to_string(), next_run);
    }

    next_runs
}

async fn run_job_once(
    pg_pool: PgPool,
    job: Arc<dyn MaintenanceJob>,
    store: Arc<dyn MaintenanceJobStore>,
) {
    let started_at = Utc::now();
    let start = Instant::now();
    let job_name = job.name();
    let started_ms = datetime_to_millis(started_at).to_string();

    tracing::info!(
        job = job_name,
        every_ms = job.schedule().every_ms(),
        "maintenance job started"
    );

    write_metric_ignore(store.as_ref(), &kv_key(job_name, "last_status"), "running").await;
    write_metric_ignore(
        store.as_ref(),
        &kv_key(job_name, "last_started_ms"),
        &started_ms,
    )
    .await;

    let result = job.run(&pg_pool).await;
    let elapsed = start.elapsed();
    let elapsed_ms = duration_millis_i64(elapsed).to_string();
    let finished_ms = datetime_to_millis(Utc::now()).to_string();

    increment_counter(store.as_ref(), &kv_key(job_name, "run_count")).await;
    write_metric_ignore(
        store.as_ref(),
        &kv_key(job_name, "last_run_ms"),
        &finished_ms,
    )
    .await;
    write_metric_ignore(
        store.as_ref(),
        &kv_key(job_name, "last_duration_ms"),
        &elapsed_ms,
    )
    .await;

    match result {
        Ok(()) => {
            write_metric_ignore(store.as_ref(), &kv_key(job_name, "last_status"), "ok").await;
            write_metric_ignore(store.as_ref(), &kv_key(job_name, "last_error"), "").await;
            tracing::info!(
                job = job_name,
                duration_ms = duration_millis_i64(elapsed),
                outcome = "ok",
                "maintenance job completed"
            );
        }
        Err(error) => {
            let message = error.to_string();
            increment_counter(store.as_ref(), &kv_key(job_name, "failure_count")).await;
            write_metric_ignore(store.as_ref(), &kv_key(job_name, "last_status"), "error").await;
            write_metric_ignore(store.as_ref(), &kv_key(job_name, "last_error"), &message).await;
            tracing::warn!(
                job = job_name,
                duration_ms = duration_millis_i64(elapsed),
                outcome = "error",
                error = %message,
                "maintenance job completed"
            );
        }
    }
}

async fn build_job_statuses(
    registry: &MaintenanceJobRegistry,
    store: Arc<dyn MaintenanceJobStore>,
) -> Vec<MaintenanceJobStatus> {
    let mut statuses = Vec::with_capacity(registry.jobs().len());
    let now = Utc::now();

    for (index, job) in registry.jobs().iter().enumerate() {
        let last_run_ms = read_i64(store.as_ref(), &kv_key(job.name(), "last_run_ms")).await;
        let persisted_next_ms = read_i64(store.as_ref(), &kv_key(job.name(), "next_run_ms")).await;
        let last_duration_ms =
            read_i64(store.as_ref(), &kv_key(job.name(), "last_duration_ms")).await;
        let run_count = match read_i64(store.as_ref(), &kv_key(job.name(), "run_count")).await {
            Some(value) => value,
            None => 0,
        };
        let failure_count =
            match read_i64(store.as_ref(), &kv_key(job.name(), "failure_count")).await {
                Some(value) => value,
                None => 0,
            };
        let last_status = match read_string(store.as_ref(), &kv_key(job.name(), "last_status"))
            .await
            .filter(|value| !value.trim().is_empty())
        {
            Some(value) => value,
            None => "never".to_string(),
        };
        let last_error = read_string(store.as_ref(), &kv_key(job.name(), "last_error"))
            .await
            .filter(|value| !value.trim().is_empty());

        let fallback_next = startup_next_run(
            job.schedule(),
            last_run_ms.and_then(datetime_from_millis),
            now,
            index,
        );
        let next_run_ms = persisted_next_ms.or_else(|| Some(datetime_to_millis(fallback_next)));

        statuses.push(MaintenanceJobStatus {
            id: job.name().to_string(),
            name: job.name().to_string(),
            enabled: true,
            schedule: MaintenanceScheduleStatus {
                kind: "every",
                every_ms: job.schedule().every_ms(),
                startup_stagger_ms: job.schedule().startup_stagger_ms(),
            },
            state: MaintenanceJobState {
                status: "active",
                last_status,
                last_run_at_ms: last_run_ms,
                last_run_at: last_run_ms.and_then(datetime_millis_to_rfc3339),
                next_run_at_ms: next_run_ms,
                next_run_at: next_run_ms.and_then(datetime_millis_to_rfc3339),
                last_duration_ms,
                last_error,
                run_count,
                failure_count,
            },
        });
    }

    statuses
}

async fn read_i64(store: &dyn MaintenanceJobStore, key: &str) -> Option<i64> {
    match store.read(key).await {
        Ok(Some(value)) => value.parse::<i64>().ok(),
        Ok(None) => None,
        Err(error) => {
            tracing::warn!("[maintenance] failed to read {key}: {error}");
            None
        }
    }
}

async fn read_string(store: &dyn MaintenanceJobStore, key: &str) -> Option<String> {
    match store.read(key).await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!("[maintenance] failed to read {key}: {error}");
            None
        }
    }
}

async fn write_metric_ignore(store: &dyn MaintenanceJobStore, key: &str, value: &str) {
    if let Err(error) = store.upsert(key, value).await {
        tracing::warn!("[maintenance] failed to write {key}: {error}");
    }
}

async fn increment_counter(store: &dyn MaintenanceJobStore, key: &str) {
    let current = match read_i64(store, key).await {
        Some(value) => value,
        None => 0,
    };
    let next = current.saturating_add(1).to_string();
    write_metric_ignore(store, key, &next).await;
}

fn startup_next_run(
    schedule: MaintenanceSchedule,
    last_run: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    index: usize,
) -> DateTime<Utc> {
    if let Some(next_after_last) = last_run.map(|last| add_duration(last, schedule.every)) {
        if next_after_last > now {
            return next_after_last;
        }
    }

    add_duration(
        now,
        startup_stagger_for_index(schedule.startup_stagger, index),
    )
}

fn startup_stagger_for_index(base: Duration, index: usize) -> Duration {
    let index_u32 = match u32::try_from(index) {
        Ok(value) => value,
        Err(_) => u32::MAX,
    };
    base.saturating_add(EXTRA_STAGGER_PER_JOB.saturating_mul(index_u32))
}

fn add_duration(at: DateTime<Utc>, duration: Duration) -> DateTime<Utc> {
    let chrono_duration = chrono::Duration::milliseconds(duration_millis_i64(duration));
    match at.checked_add_signed(chrono_duration) {
        Some(value) => value,
        None => at,
    }
}

fn duration_millis_i64(duration: Duration) -> i64 {
    match i64::try_from(duration.as_millis()) {
        Ok(value) => value,
        Err(_) => i64::MAX,
    }
}

fn datetime_to_millis(value: DateTime<Utc>) -> i64 {
    value.timestamp_millis()
}

fn datetime_from_millis(value: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(value).single()
}

fn datetime_millis_to_rfc3339(value: i64) -> Option<String> {
    datetime_from_millis(value).map(|datetime| datetime.to_rfc3339())
}

fn kv_key(job_name: &str, field: &str) -> String {
    format!("maintenance_job:{job_name}:{field}")
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
async fn run_scheduler_loop_for_test(
    pg_pool: PgPool,
    registry: MaintenanceJobRegistry,
    store: Arc<dyn MaintenanceJobStore>,
    tick_interval: Duration,
    max_completed_runs: usize,
) {
    run_scheduler_loop(
        pg_pool,
        registry,
        store,
        tick_interval,
        Some(max_completed_runs),
    )
    .await;
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[derive(Default)]
struct InMemoryMaintenanceJobStore {
    values: tokio::sync::Mutex<std::collections::HashMap<String, String>>,
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
impl MaintenanceJobStore for InMemoryMaintenanceJobStore {
    fn read<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<String>> {
        Box::pin(async move {
            let values = self.values.lock().await;
            Ok(values.get(key).cloned())
        })
    }

    fn upsert<'a>(&'a self, key: &'a str, value: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let mut values = self.values.lock().await;
            values.insert(key.to_string(), value.to_string());
            Ok(())
        })
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use std::io::{self, Write};
    use std::sync::Mutex;

    struct FastLogJob;

    impl MaintenanceJob for FastLogJob {
        fn name(&self) -> &'static str {
            "test.fast_log"
        }

        fn schedule(&self) -> MaintenanceSchedule {
            MaintenanceSchedule::every(Duration::from_secs(60), Duration::from_millis(1))
        }

        fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a> {
            Box::pin(async move {
                let _ = pool;
                tracing::info!(job = self.name(), "test maintenance job body ran");
                Ok(())
            })
        }
    }

    #[derive(Clone)]
    struct TestLogWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for TestLogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut buffer = self
                .buffer
                .lock()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "log buffer poisoned"))?;
            buffer.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct TestPostgresDb {
        admin_url: String,
        database_name: String,
        database_url: String,
        cleanup_armed: bool,
    }

    impl TestPostgresDb {
        async fn create() -> Result<Self, String> {
            let admin_url = postgres_admin_database_url();
            let database_name = format!("agentdesk_maintenance_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "maintenance tests",
            )
            .await?;
            Ok(Self {
                admin_url,
                database_name,
                database_url,
                cleanup_armed: true,
            })
        }

        async fn connect_and_migrate(&self) -> Result<PgPool, String> {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "maintenance tests",
            )
            .await
        }

        async fn drop(mut self) -> Result<(), String> {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "maintenance tests",
            )
            .await?;
            self.cleanup_armed = false;
            Ok(())
        }
    }

    impl Drop for TestPostgresDb {
        fn drop(&mut self) {
            if !self.cleanup_armed {
                return;
            }
            let admin_url = self.admin_url.clone();
            let database_name = self.database_name.clone();
            std::thread::spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                let _ = runtime.block_on(crate::db::postgres::drop_test_database(
                    &admin_url,
                    &database_name,
                    "maintenance tests cleanup",
                ));
            });
        }
    }

    fn postgres_base_database_url() -> String {
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

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scheduler_loop_starts_fires_one_tick_and_logs_outcome() -> Result<(), String> {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let log_buffer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .without_time()
            .with_writer(move || TestLogWriter {
                buffer: log_buffer.clone(),
            })
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let pg_pool = PgPoolOptions::new().connect_lazy_with(
            PgConnectOptions::new()
                .host("localhost")
                .username("agentdesk")
                .database("agentdesk"),
        );
        let registry = MaintenanceJobRegistry::new(vec![Arc::new(FastLogJob)]);
        let store: Arc<dyn MaintenanceJobStore> = Arc::new(InMemoryMaintenanceJobStore::default());

        tokio::time::timeout(
            Duration::from_millis(250),
            run_scheduler_loop_for_test(pg_pool, registry, store, Duration::from_millis(1), 1),
        )
        .await
        .map_err(|_| "maintenance scheduler test timed out".to_string())?;

        let captured = {
            let buffer = buffer
                .lock()
                .map_err(|_| "log buffer poisoned".to_string())?;
            String::from_utf8_lossy(&buffer).to_string()
        };

        if !captured.contains("maintenance job started")
            || !captured.contains("maintenance job completed")
            || !captured.contains("test.fast_log")
        {
            return Err(format!(
                "expected maintenance run logs, captured:\n{captured}"
            ));
        }

        Ok(())
    }

    #[tokio::test]
    async fn status_builder_uses_persisted_next_run() -> Result<(), String> {
        let registry = MaintenanceJobRegistry::new(vec![Arc::new(FastLogJob)]);
        let store: Arc<dyn MaintenanceJobStore> = Arc::new(InMemoryMaintenanceJobStore::default());
        store
            .upsert(&kv_key("test.fast_log", "next_run_ms"), "1700000000000")
            .await?;

        let statuses = build_job_statuses(&registry, store).await;
        let status = statuses
            .first()
            .ok_or_else(|| "missing maintenance job status".to_string())?;

        if status.state.next_run_at_ms != Some(1_700_000_000_000) {
            return Err(format!(
                "unexpected next run: {:?}",
                status.state.next_run_at_ms
            ));
        }

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn agent_quality_rollup_pg_job_runs_one_tick() -> Result<(), String> {
        let pg_db = TestPostgresDb::create().await?;
        let pg_pool = pg_db.connect_and_migrate().await?;
        for event_type in [
            "turn_complete",
            "turn_complete",
            "turn_complete",
            "turn_error",
            "review_pass",
            "review_fail",
        ] {
            sqlx::query(
                "INSERT INTO agent_quality_event (
                    agent_id,
                    provider,
                    channel_id,
                    event_type,
                    payload,
                    created_at
                 ) VALUES ($1, 'codex', '42', $2::agent_quality_event_type, '{}'::jsonb, NOW())",
            )
            .bind("agent-rollup")
            .bind(event_type)
            .execute(&pg_pool)
            .await
            .map_err(|error| format!("insert quality event: {error}"))?;
        }

        let registry = MaintenanceJobRegistry::new(vec![Arc::new(AgentQualityRollupJob)]);
        let store: Arc<dyn MaintenanceJobStore> = Arc::new(InMemoryMaintenanceJobStore::default());
        tokio::time::timeout(
            Duration::from_secs(5),
            run_scheduler_loop_for_test(
                pg_pool.clone(),
                registry,
                store,
                Duration::from_millis(1),
                1,
            ),
        )
        .await
        .map_err(|_| "agent quality rollup job timed out".to_string())?;

        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM agent_quality_daily WHERE agent_id = 'agent-rollup'",
        )
        .fetch_one(&pg_pool)
        .await
        .map_err(|error| format!("count quality daily rows: {error}"))?;
        if count < 1 {
            return Err("agent quality rollup did not write daily rows".to_string());
        }

        pg_pool.close().await;
        pg_db.drop().await?;
        Ok(())
    }
}
