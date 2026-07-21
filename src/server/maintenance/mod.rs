use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone, Utc};
use serde::Serialize;
use sqlx::{PgPool, Row};

mod storage_jobs;

use self::storage_jobs::{
    MemoryMementoConsolidationJob, ProgressTtsCacheSweepJob, StorageDbRetentionJob,
    StorageHangDumpCleanupJob, StorageTargetSweepJob, StorageTmpPipelineSweepJob,
    StorageWorktreeOrphanSweepJob,
};

type MaintenanceFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;
type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

const DEFAULT_SCHEDULER_TICK: Duration = Duration::from_secs(1);
const EXTRA_STAGGER_PER_JOB: Duration = Duration::from_secs(15);

// ── Per-job startup stagger offsets ─────────────────────────────────────────
//
// Every job carries a *base* startup stagger (the second argument to
// `MaintenanceSchedule::every`). On a cold start `startup_stagger_for_index`
// adds `EXTRA_STAGGER_PER_JOB * <registry index>` on top of this base, so the
// two mechanisms compose: the index-based auto-stagger fans the jobs out, and
// the per-job bases below fine-tune the ordering *within* that fan-out (e.g.
// keeping the quality rollup ahead of the alerters that read its aggregates on
// the same tick). They are intentionally NOT folded into the auto-stagger —
// doing so would change the real boot timings. The values here are byte-for-byte
// identical to the historical inline literals; naming them just makes the
// existing schedule self-describing (cf. `REREVIEW_TRANSIENT_DIRTY_RETRY_DELAY`
// in `dispatch_context.rs`).

/// `noop_heartbeat` (registry index 0): a small boot offset so the liveness
/// heartbeat is among the earliest maintenance signals without racing the pool.
const NOOP_HEARTBEAT_STARTUP_STAGGER: Duration = Duration::from_secs(10);

/// `agent_quality_rollup`: zero base so the hourly rollup runs *before* the
/// regression + relay alerters below that read its aggregates on the same tick.
const AGENT_QUALITY_ROLLUP_STARTUP_STAGGER: Duration = Duration::from_secs(0);

/// `quality_regression_alerter`: sequenced after `agent_quality_rollup` (base 0)
/// on the same hourly tick so alerts evaluate against the freshest aggregates.
const QUALITY_REGRESSION_ALERTER_STARTUP_STAGGER: Duration = Duration::from_secs(15);

/// `relay_signal_alerter`: sequenced after the quality alerter on the same
/// hourly tick so the two alert pipelines do not race for a connection at boot.
const RELAY_SIGNAL_ALERTER_STARTUP_STAGGER: Duration = Duration::from_secs(30);

/// `storage.cancel_tombstone_prune`: small boot offset so the tombstone prune
/// does not pile on top of the other storage jobs at startup.
const CANCEL_TOMBSTONE_PRUNE_STARTUP_STAGGER: Duration = Duration::from_secs(20);

/// `voice.turn_link_gc`: staggered ~25s after boot so it does not pile on top
/// of the other storage jobs (see the job's rationale comment).
const VOICE_TURN_LINK_GC_STARTUP_STAGGER: Duration = Duration::from_secs(25);

/// `storage.prompt_manifest_retention`: small startup stagger so the daily
/// retention sweep does not pile on top of the other storage jobs at boot.
const PROMPT_MANIFEST_RETENTION_STARTUP_STAGGER: Duration = Duration::from_secs(45);

/// `storage.voice_transcript_announcement_meta_gc`: boot offset that keeps this
/// voice-meta GC clear of the sibling voice GC jobs on the same tick.
const VOICE_TRANSCRIPT_ANNOUNCEMENT_META_GC_STARTUP_STAGGER: Duration = Duration::from_secs(60);

/// `storage.voice_background_handoff_meta_gc`: a touch longer than the
/// announce-meta GC (60s) so the two voice storage GCs do not race for the same
/// connection on the same tick.
const VOICE_BACKGROUND_HANDOFF_META_GC_STARTUP_STAGGER: Duration = Duration::from_secs(75);

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
        // Status/registry-only path (never executes the sweep) → default dirs.
        Self::static_registry_with_config(
            crate::config::PromptManifestRetentionConfig::default(),
            crate::services::maintenance::jobs::voice_cache_sweep::Config::default_runtime(),
        )
    }

    fn static_registry_with_config(
        prompt_manifest_retention: crate::config::PromptManifestRetentionConfig,
        voice_cache_sweep: crate::services::maintenance::jobs::voice_cache_sweep::Config,
    ) -> Self {
        Self::new(vec![
            Arc::new(NoopHeartbeatJob),
            Arc::new(AgentQualityRollupJob),
            Arc::new(QualityRegressionAlerterJob),
            Arc::new(RelaySignalAlerterJob),
            Arc::new(CancelTombstonePruneJob),
            Arc::new(PromptManifestRetentionJob::new(prompt_manifest_retention)),
            Arc::new(VoiceTurnLinkGcJob),
            Arc::new(VoiceTranscriptAnnouncementMetaGcJob),
            Arc::new(VoiceBackgroundHandoffMetaGcJob),
            // #3909 — leader-only voice TTS cache/temp sweep.
            Arc::new(ProgressTtsCacheSweepJob {
                config: voice_cache_sweep,
            }),
            // #3231 — disk-GC / memory maintenance jobs. Implemented in
            // `services::maintenance::jobs::*` but historically never wired
            // (zero callers of `spawn_storage_maintenance_jobs`), so the
            // disk-full prevention from #3231 never ran. Registered here so
            // the live leader-only scheduler executes them.
            Arc::new(StorageTargetSweepJob),
            Arc::new(StorageWorktreeOrphanSweepJob),
            Arc::new(StorageTmpPipelineSweepJob),
            Arc::new(StorageHangDumpCleanupJob),
            Arc::new(StorageDbRetentionJob),
            Arc::new(MemoryMementoConsolidationJob),
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
        MaintenanceSchedule::every(Duration::from_secs(15 * 60), NOOP_HEARTBEAT_STARTUP_STAGGER)
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
        MaintenanceSchedule::every(
            Duration::from_secs(60 * 60),
            AGENT_QUALITY_ROLLUP_STARTUP_STAGGER,
        )
    }

    fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a> {
        Box::pin(async move {
            let report = crate::services::observability::run_agent_quality_rollup_pg(pool).await?;
            tracing::info!(
                job = self.name(),
                upserted_rows = report.upserted_rows,
                "agent quality aggregation rollup completed; alerts are owned by quality_regression_alerter"
            );
            Ok(())
        })
    }
}

/// #1104 (911-4) hourly regression rule engine.
///
/// Sole regression-alert authority. Runs the rule engine in
/// `services::agent_quality::regression_alerts`
/// against the `agent_quality_daily` rollup. The 15s startup stagger keeps
/// it sequenced AFTER `agent_quality_rollup` (stagger=0) on the same tick
/// so alerts always evaluate against the freshest aggregates.
struct QualityRegressionAlerterJob;

impl MaintenanceJob for QualityRegressionAlerterJob {
    fn name(&self) -> &'static str {
        "quality_regression_alerter"
    }

    fn schedule(&self) -> MaintenanceSchedule {
        MaintenanceSchedule::every(
            Duration::from_secs(60 * 60),
            QUALITY_REGRESSION_ALERTER_STARTUP_STAGGER,
        )
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

/// #3561 — hourly relay-loss signal monitor + operator alert.
///
/// Aggregates the restart-safe `observability_events` stream (relay root-cause
/// counters + offset invariant violations) over the trailing hour and enqueues
/// a single de-duplicated Discord alert per signal when its count crosses the
/// (conservative, config-overridable) threshold. Double off-switch: no alert
/// target configured ⇒ no enqueue, so unconfigured deploys never spam. The 30s
/// startup stagger sequences it after the quality alerter on the same hourly
/// tick so the two alert pipelines do not race for a connection at boot.
struct RelaySignalAlerterJob;

impl MaintenanceJob for RelaySignalAlerterJob {
    fn name(&self) -> &'static str {
        "relay_signal_alerter"
    }

    fn schedule(&self) -> MaintenanceSchedule {
        MaintenanceSchedule::every(
            Duration::from_secs(60 * 60),
            RELAY_SIGNAL_ALERTER_STARTUP_STAGGER,
        )
    }

    fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a> {
        Box::pin(async move {
            let alerts =
                crate::services::observability::enqueue_relay_signal_alerts_pg(pool).await?;
            tracing::info!(
                job = self.name(),
                alerts_dispatched = alerts,
                "relay signal alerter completed"
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
        MaintenanceSchedule::every(
            Duration::from_secs(30 * 60),
            CANCEL_TOMBSTONE_PRUNE_STARTUP_STAGGER,
        )
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

/// #2362 / #2164 Voice A — hourly sweep of terminal `voice_turn_link`
/// rows older than 24h. Active and cancelled rows are intentionally
/// preserved because background turns can live 24h+ and the cancelled
/// tombstones support reverse lookup during late reconciliation. The
/// schedule mirrors the existing 30-minute cadence used by
/// `CancelTombstonePruneJob`; staggered 25s after boot so it does not
/// pile on top of other storage jobs.
struct VoiceTurnLinkGcJob;

/// Retention horizon for `voice_turn_link` terminal rows. 24h is long
/// enough that any reasonable late-arriving barge-in / cancel /
/// completion signal still finds the row.
const VOICE_TURN_LINK_GC_RETENTION_SECS: i64 = 24 * 60 * 60;

impl MaintenanceJob for VoiceTurnLinkGcJob {
    fn name(&self) -> &'static str {
        "voice.turn_link_gc"
    }

    fn schedule(&self) -> MaintenanceSchedule {
        MaintenanceSchedule::every(
            Duration::from_secs(60 * 60),
            VOICE_TURN_LINK_GC_STARTUP_STAGGER,
        )
    }

    fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a> {
        Box::pin(async move {
            let cutoff = Utc::now() - chrono::Duration::seconds(VOICE_TURN_LINK_GC_RETENTION_SECS);
            let deleted =
                crate::voice::turn_link::gc_terminal_voice_turn_links_pg(pool, cutoff).await?;
            if deleted > 0 {
                tracing::info!(
                    job = self.name(),
                    deleted,
                    retention_secs = VOICE_TURN_LINK_GC_RETENTION_SECS,
                    "[maintenance] voice_turn_link GC swept terminal rows"
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
        MaintenanceSchedule::every(
            Duration::from_secs(24 * 60 * 60),
            PROMPT_MANIFEST_RETENTION_STARTUP_STAGGER,
        )
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

/// #2209 — leader-only GC for durable voice transcript announcement metadata.
/// Rows are short-lived during normal operation, but a send failure or process
/// crash can leave pending rows behind.
struct VoiceTranscriptAnnouncementMetaGcJob;

impl MaintenanceJob for VoiceTranscriptAnnouncementMetaGcJob {
    fn name(&self) -> &'static str {
        "storage.voice_transcript_announcement_meta_gc"
    }

    fn schedule(&self) -> MaintenanceSchedule {
        MaintenanceSchedule::every(
            Duration::from_secs(15 * 60),
            VOICE_TRANSCRIPT_ANNOUNCEMENT_META_GC_STARTUP_STAGGER,
        )
    }

    fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a> {
        Box::pin(async move {
            let ttl = Duration::from_secs(
                crate::voice::announce_meta::DURABLE_ANNOUNCEMENT_META_TTL_SECS as u64,
            );
            let deleted =
                crate::voice::announce_meta::gc_expired_voice_announcement_meta_pg(pool, ttl)
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("voice_transcript_announcement_meta_gc failed: {error}")
                    })?;
            if deleted > 0 {
                tracing::info!(
                    job = self.name(),
                    deleted,
                    "[maintenance] voice_transcript_announcement_meta_gc removed expired rows"
                );
            }
            Ok(())
        })
    }
}

/// #2274 — leader-only GC for `voice_background_handoff_meta`. Cleans
/// rows whose `created_at` is older than the durable handoff TTL (~1
/// hour). Consumed rows are also removed by the same age-based filter
/// since terminal-delivery rows are consumed within minutes; anything
/// older almost certainly represents a turn that crashed before terminal
/// delivery and never claimed the row. Conservative 75-second stagger
/// keeps it sequenced after the announce-meta GC at boot.
struct VoiceBackgroundHandoffMetaGcJob;

impl MaintenanceJob for VoiceBackgroundHandoffMetaGcJob {
    fn name(&self) -> &'static str {
        "storage.voice_background_handoff_meta_gc"
    }

    fn schedule(&self) -> MaintenanceSchedule {
        // 15-minute cadence matches the announce-meta GC pattern; the
        // stagger is a touch longer so the two storage GCs do not race
        // for the same connection on the same tick.
        MaintenanceSchedule::every(
            Duration::from_secs(15 * 60),
            VOICE_BACKGROUND_HANDOFF_META_GC_STARTUP_STAGGER,
        )
    }

    fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a> {
        Box::pin(async move {
            let ttl = Duration::from_secs(
                crate::voice::announce_meta::DURABLE_HANDOFF_META_TTL_SECS as u64,
            );
            let deleted =
                crate::voice::announce_meta::gc_expired_voice_background_handoff_meta_pg(pool, ttl)
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("voice_background_handoff_meta_gc failed: {error}")
                    })?;
            if deleted > 0 {
                tracing::info!(
                    job = self.name(),
                    deleted,
                    "[maintenance] voice_background_handoff_meta_gc removed expired rows"
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
    voice_cache_sweep: crate::services::maintenance::jobs::voice_cache_sweep::Config,
) {
    let registry = MaintenanceJobRegistry::static_registry_with_config(
        prompt_manifest_retention,
        voice_cache_sweep,
    );
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

/// Registry-membership assertion (#2362 / #2164 Voice A). Lives outside
/// the removed SQLite-only gate so the production scheduler is checked in the
/// normal test path.
#[cfg(test)]
mod registry_membership_tests {
    use super::*;

    #[test]
    fn static_registry_includes_voice_turn_link_gc() {
        let registry = MaintenanceJobRegistry::static_registry();
        let names: Vec<&'static str> = registry.jobs().iter().map(|job| job.name()).collect();
        assert!(
            names.contains(&"voice.turn_link_gc"),
            "voice.turn_link_gc must be registered on the production \
             MaintenanceJobRegistry so the leader scheduler sweeps \
             terminal voice_turn_link rows (#2362). present jobs: {names:?}"
        );
    }

    #[test]
    fn static_registry_includes_voice_transcript_announcement_meta_gc() {
        let registry = MaintenanceJobRegistry::static_registry();
        let names: Vec<&'static str> = registry.jobs().iter().map(|job| job.name()).collect();
        assert!(
            names.contains(&"storage.voice_transcript_announcement_meta_gc"),
            "storage.voice_transcript_announcement_meta_gc must be registered so \
             durable voice announcement metadata cannot accumulate indefinitely. \
             present jobs: {names:?}"
        );
    }

    /// #3231 — the disk-GC / memory maintenance jobs were implemented in
    /// `services::maintenance::jobs::*` but never wired into the live
    /// scheduler (zero callers of `spawn_storage_maintenance_jobs`; absent
    /// from this static registry), so the disk-full prevention never ran.
    /// These assertions fail on origin/main and pass once the wrapper
    /// structs are registered.
    #[test]
    fn static_registry_includes_disk_gc_jobs() {
        let registry = MaintenanceJobRegistry::static_registry();
        let names: Vec<&'static str> = registry.jobs().iter().map(|job| job.name()).collect();
        for expected in [
            "storage.worktree_orphan_sweep",
            "storage.tmp_pipeline_sweep",
            "storage.target_sweep",
            "storage.hang_dump_cleanup",
            "storage.db_retention",
            "memory.memento_consolidation",
        ] {
            assert!(
                names.contains(&expected),
                "{expected} must be registered on the production \
                 MaintenanceJobRegistry so the disk-GC maintenance jobs from \
                 #3231 actually run on the leader scheduler. present jobs: {names:?}"
            );
        }
    }

    #[test]
    fn static_registry_includes_progress_tts_cache_sweep() {
        // #3909 — the leader-only voice cache/temp sweeper must be wired into
        // the production registry so the unbounded-growth leaks are bounded.
        let registry = MaintenanceJobRegistry::static_registry();
        assert!(
            registry
                .jobs()
                .iter()
                .any(|job| job.name() == "voice.progress_tts_cache_sweep"),
            "voice.progress_tts_cache_sweep must be registered on the production \
             leader-only maintenance scheduler (#3909)"
        );
    }

    #[test]
    fn voice_turn_link_gc_schedule_is_hourly() {
        let job = VoiceTurnLinkGcJob;
        assert_eq!(job.name(), "voice.turn_link_gc");
        let schedule = job.schedule();
        assert_eq!(schedule.every_ms(), 60 * 60 * 1_000);
    }

    /// #3561 — the relay-loss operator monitor must be registered so the
    /// leader scheduler actually evaluates the relay signal thresholds hourly;
    /// otherwise the alert pipeline silently never runs.
    #[test]
    fn static_registry_includes_relay_signal_alerter() {
        let registry = MaintenanceJobRegistry::static_registry();
        let names: Vec<&'static str> = registry.jobs().iter().map(|job| job.name()).collect();
        assert!(
            names.contains(&"relay_signal_alerter"),
            "relay_signal_alerter must be registered so the leader scheduler \
             evaluates relay-loss signal thresholds hourly (#3561). \
             present jobs: {names:?}"
        );
    }

    #[test]
    fn relay_signal_alerter_schedule_is_hourly() {
        let job = RelaySignalAlerterJob;
        assert_eq!(job.name(), "relay_signal_alerter");
        assert_eq!(job.schedule().every_ms(), 60 * 60 * 1_000);
    }
}
