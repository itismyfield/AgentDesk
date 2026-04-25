pub(crate) mod cron_catalog;
pub(crate) mod maintenance;
pub mod routes;
mod worker_registry;
pub mod ws;

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::routing::get;
use sqlx::pool::PoolConnection;
use sqlx::{PgPool, Postgres, Row};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tower_http::services::{ServeDir, ServeFile};

use crate::config::Config;
use crate::engine::PolicyEngine;
use crate::services::discord::health::HealthRegistry;

const MEMORY_HEALTH_STARTUP_REASON: &str = "startup";
const MEMORY_HEALTH_FIVE_MIN_REASON: &str = "OnTick5min";
const FIVE_MIN_POLICY_TICK_INTERVAL: u64 = 10;
const MESSAGE_OUTBOX_CLAIM_STALE_SECS: i64 = 300;
const POLICY_TICK_ADVISORY_LOCK_ID: i64 = 7_801_001;
const GITHUB_SYNC_ADVISORY_LOCK_ID: i64 = 7_801_002;
const POLICY_TICK_WARN_MS: u128 = 500;
const POLICY_TICK_HOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// Monotonically increasing count of policy tick hook timeouts (#747).
/// Incremented each time `fire_tick_hook_by_name_with_timeout` returns
/// because the wall-clock timeout elapsed before the spawn_blocking task
/// finished. Observable via `policy_tick_timeout_count()` in tests or logs.
static POLICY_TICK_TIMEOUT_COUNT: AtomicU64 = AtomicU64::new(0);

/// Monotonically increasing count of tick hooks that *did* finish, but only
/// after their owning call already timed out (#747). Helps operators notice
/// when the tick actor is holding onto work well past the user-visible
/// deadline, which is the failure mode this counter was added to track.
static POLICY_TICK_POST_TIMEOUT_COMPLETIONS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
pub(crate) fn policy_tick_timeout_count() -> u64 {
    POLICY_TICK_TIMEOUT_COUNT.load(Ordering::Acquire)
}

#[cfg(test)]
pub(crate) fn policy_tick_post_timeout_completions() -> u64 {
    POLICY_TICK_POST_TIMEOUT_COMPLETIONS.load(Ordering::Acquire)
}

struct PolicyTickHookGuard {
    in_flight: Arc<AtomicBool>,
}

impl PolicyTickHookGuard {
    fn acquire(engine: &PolicyEngine) -> Option<Self> {
        let in_flight = engine.tick_hook_in_flight();
        in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self { in_flight })
    }
}

impl Drop for PolicyTickHookGuard {
    fn drop(&mut self) {
        self.in_flight.store(false, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolicyTickHookOutcome {
    Ok,
    Error,
    Timeout,
    SkippedInFlight,
    JoinError,
}

impl PolicyTickHookOutcome {
    fn status(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Timeout => "timeout",
            Self::SkippedInFlight => "skipped_inflight",
            Self::JoinError => "join_error",
        }
    }
}

struct PolicyTickHookExecution {
    outcome: PolicyTickHookOutcome,
    elapsed: Duration,
    error: Option<String>,
}

async fn try_acquire_pg_singleton_lock(
    pool: &PgPool,
    lock_id: i64,
    job_name: &str,
) -> std::result::Result<Option<PoolConnection<Postgres>>, String> {
    let mut conn = pool
        .acquire()
        .await
        .map_err(|error| format!("{job_name} acquire advisory lock connection: {error}"))?;
    let acquired = sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
        .bind(lock_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(|error| format!("{job_name} try advisory lock: {error}"))?;
    if acquired { Ok(Some(conn)) } else { Ok(None) }
}

async fn release_pg_singleton_lock(
    mut conn: PoolConnection<Postgres>,
    lock_id: i64,
    job_name: &str,
) {
    if let Err(error) = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(lock_id)
        .execute(&mut *conn)
        .await
    {
        tracing::warn!("[{job_name}] failed to release advisory lock {lock_id}: {error}");
    }
}

async fn refresh_memory_health_for_startup() {
    crate::services::memory::refresh_backend_health(MEMORY_HEALTH_STARTUP_REASON).await;
}

async fn refresh_memory_health_for_five_min_tick() {
    crate::services::memory::refresh_backend_health(MEMORY_HEALTH_FIVE_MIN_REASON).await;
}

fn is_five_min_policy_tick(count: u64) -> bool {
    count != 0 && count % FIVE_MIN_POLICY_TICK_INTERVAL == 0
}

pub async fn run(
    config: Config,
    legacy_db: crate::db::Db,
    engine: PolicyEngine,
    health_registry: Option<Arc<HealthRegistry>>,
) -> Result<()> {
    let pg_pool = crate::db::postgres::connect_and_migrate(&config)
        .await
        .map_err(anyhow::Error::msg)?;
    crate::services::observability::init_observability(legacy_db.clone(), pg_pool.clone());
    let startup_pg_pool = if pg_pool.is_some() {
        match crate::db::postgres::connect_for_startup(&config).await {
            Ok(pool) => pool,
            Err(error) => {
                tracing::warn!(
                    "[startup] postgres warmup pool unavailable; falling back to runtime pool: {error}"
                );
                None
            }
        }
    } else {
        None
    };
    let startup_pool = startup_pg_pool.as_ref().or(pg_pool.as_ref());
    if let Some(pool) = startup_pool {
        crate::db::postgres::startup_reseed(pool, &config)
            .await
            .map_err(anyhow::Error::msg)?;
        seed_sqlite_startup_runtime_state(&legacy_db, &config, false);
    } else {
        seed_startup_runtime_state(&legacy_db, &config);
    }
    crate::pipeline::refresh_override_health_report(&legacy_db, pg_pool.as_ref()).await;
    let boot_reconcile_engine = match startup_pg_pool.as_ref() {
        Some(pool) => Some(crate::engine::PolicyEngine::new_with_pg(
            &config,
            Some(pool.clone()),
        )?),
        None => None,
    };
    crate::reconcile::reconcile_boot_runtime(
        &legacy_db,
        boot_reconcile_engine.as_ref().unwrap_or(&engine),
        startup_pool,
    )
    .await?;
    drop(boot_reconcile_engine);
    drop(startup_pg_pool);

    let mut worker_registry = worker_registry::SupervisedWorkerRegistry::new(
        config.clone(),
        engine.clone(),
        health_registry.clone(),
        pg_pool.clone().map(Arc::new),
    );
    worker_registry.run_boot_only_steps().await?;
    worker_registry.start_after_boot_reconcile()?;

    // Resolve dashboard dist path relative to runtime root or binary location
    let dashboard_dir = crate::cli::agentdesk_runtime_root()
        .map(|r| r.join("dashboard/dist"))
        .unwrap_or_else(|| std::path::PathBuf::from("dashboard/dist"));

    // Auto-provision: if runtime dist is missing, copy from workspace source
    if !dashboard_dir.join("index.html").exists() {
        let workspace_dist =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("dashboard/dist");
        if workspace_dist.join("index.html").exists() {
            tracing::info!(
                "Dashboard dist missing at {:?}, copying from workspace {:?}",
                dashboard_dir,
                workspace_dist
            );
            if let Some(parent) = dashboard_dir.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Remove stale dist dir if it exists but is incomplete
            let _ = std::fs::remove_dir_all(&dashboard_dir);
            match copy_dir_recursive(&workspace_dist, &dashboard_dir) {
                Ok(n) => tracing::info!("Dashboard dist copied ({n} files)"),
                Err(e) => tracing::warn!("Failed to copy dashboard dist: {e}"),
            }
        } else {
            tracing::warn!(
                "Dashboard dist not found at {:?} or {:?} — dashboard will be unavailable",
                dashboard_dir,
                workspace_dist
            );
        }
    }

    tracing::info!("Serving dashboard from {:?}", dashboard_dir);

    let broadcast_tx = ws::new_broadcast();
    let batch_buffer = worker_registry.start_after_websocket_broadcast(broadcast_tx.clone())?;
    let dashboard_service = ServeDir::new(&dashboard_dir)
        .append_index_html_on_directories(true)
        .fallback(ServeFile::new(dashboard_dir.join("index.html")));

    let app = Router::new()
        .route("/ws", get(ws::ws_handler).with_state(broadcast_tx.clone()))
        .nest(
            "/api",
            routes::api_router_with_pg(
                legacy_db.clone(),
                engine.clone(),
                config.clone(),
                broadcast_tx.clone(),
                batch_buffer,
                health_registry,
                pg_pool,
            ),
        )
        .fallback_service(dashboard_service);

    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("HTTP server listening on {addr}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}

fn seed_startup_runtime_state(legacy_db: &crate::db::Db, config: &Config) {
    seed_sqlite_startup_runtime_state(legacy_db, config, true);
}

fn seed_sqlite_startup_runtime_state(
    legacy_db: &crate::db::Db,
    config: &Config,
    seed_runtime_kv: bool,
) {
    if seed_runtime_kv {
        if let Ok(conn) = legacy_db.lock() {
            routes::settings::seed_config_defaults(&conn, config);
            // server_port is always overwritten (not INSERT OR IGNORE) to match current config
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('server_port', ?1)",
                [config.server.port.to_string()],
            )
            .ok();
        } else {
            tracing::warn!("[startup] failed to lock db for config default seeding");
        }
    }

    if let Err(error) = seed_github_repos_from_config(legacy_db, config) {
        tracing::warn!("[startup] failed to seed github repos from config: {error}");
    }
    if let Err(error) = crate::db::agents::sync_agents_from_config(legacy_db, &config.agents) {
        tracing::warn!("[startup] failed to sync agents from config: {error}");
    }
}

fn seed_github_repos_from_config(
    legacy_db: &crate::db::Db,
    config: &Config,
) -> std::result::Result<(), String> {
    use std::collections::BTreeSet;

    let mut repo_ids = BTreeSet::new();
    for raw_repo_id in &config.github.repos {
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
        repo_ids.insert(repo_id.to_string());
    }

    for repo_id in repo_ids {
        crate::github::register_repo(legacy_db, &repo_id)?;
    }

    Ok(())
}

/// Background task that fires tiered OnTick hooks at different intervals (#127).
///
/// 3 tiers to prevent slow sections from blocking time-critical recovery:
/// - OnTick30s (30s): retry, unsent notification recovery, deadlock detection [I], orphan recovery [K]
/// - OnTick1min (1m): non-critical timeouts [A][C][D][E][L], stale detection
/// - OnTick5min (5m): non-critical reconciliation [R][B][F][G][H][M][O], idle session cleanup
/// - OnTick (legacy, 5m): backward compat for policies that only register onTick
async fn policy_tick_loop(engine: PolicyEngine, pg_pool: Option<Arc<PgPool>>) {
    tracing::info!("[policy-tick] 3-tier tick started: 30s / 1min / 5min");

    let mut interval_30s = tokio::time::interval(Duration::from_secs(30));
    let mut count = 0u64;

    // Skip the first immediate tick
    interval_30s.tick().await;

    loop {
        interval_30s.tick().await;
        count += 1;

        let advisory_lock = if let Some(pool) = pg_pool.as_deref().or_else(|| engine.pg_pool()) {
            match try_acquire_pg_singleton_lock(pool, POLICY_TICK_ADVISORY_LOCK_ID, "policy-tick")
                .await
            {
                Ok(Some(conn)) => Some(conn),
                Ok(None) => {
                    tracing::debug!("[policy-tick] skipped: advisory lock held elsewhere");
                    continue;
                }
                Err(error) => {
                    tracing::warn!("[policy-tick] advisory lock failed: {error}");
                    continue;
                }
            }
        } else {
            None
        };

        // ── 30s tier: every tick ── (#134: fire by name for dynamic hook binding)
        fire_tick_hook_by_name_with_pg(&engine, pg_pool.as_deref(), "OnTick30s", "30s").await;

        // ── 1min tier: every 2nd tick (60s) ──
        if count % 2 == 0 {
            fire_tick_hook_by_name_with_pg(&engine, pg_pool.as_deref(), "OnTick1min", "1min").await;
        }

        // ── 5min tier: every 10th tick (300s) ──
        if is_five_min_policy_tick(count) {
            fire_tick_hook_by_name_with_pg(&engine, pg_pool.as_deref(), "OnTick5min", "5min").await;
            refresh_memory_health_for_five_min_tick().await;
            if let Some(db) = engine.legacy_db() {
                if let Err(error) =
                    crate::services::api_friction::process_api_friction_patterns(db, None, None)
                        .await
                {
                    tracing::warn!("[policy-tick] api-friction aggregation failed: {error}");
                }
            }
            // #1072 turn-lifecycle SLO aggregation (Epic #905 Phase 1):
            // compute + persist + alert on threshold breach.
            let slo_pool = pg_pool.as_deref().or_else(|| engine.pg_pool());
            let slo_db = engine.legacy_db();
            let now_ms = chrono::Utc::now().timestamp_millis();
            let _ = crate::services::slo::run_aggregation_tick(slo_db, slo_pool, now_ms).await;

            // Also fire legacy OnTick for backward compat
            fire_tick_hook_by_name_with_pg(&engine, pg_pool.as_deref(), "OnTick", "legacy").await;
        }

        if let Some(conn) = advisory_lock {
            release_pg_singleton_lock(conn, POLICY_TICK_ADVISORY_LOCK_ID, "policy-tick").await;
        }
    }
}

/// Fire a single tick hook by name, log timing, record telemetry, and notify any dispatches created by JS.
/// Uses try_fire_hook_by_name for dynamic hook binding (#134).
#[cfg(test)]
async fn fire_tick_hook_by_name(
    engine: &PolicyEngine,
    legacy_db: &crate::db::Db,
    hook_name: &str,
    label: &str,
) {
    let execution =
        fire_tick_hook_by_name_with_timeout(engine, hook_name, label, POLICY_TICK_HOOK_TIMEOUT)
            .await;
    record_tick_hook_execution(legacy_db, label, &execution);
}

async fn fire_tick_hook_by_name_with_pg(
    engine: &PolicyEngine,
    pg_pool: Option<&PgPool>,
    hook_name: &str,
    label: &str,
) {
    let execution =
        fire_tick_hook_by_name_with_timeout(engine, hook_name, label, POLICY_TICK_HOOK_TIMEOUT)
            .await;
    if let Some(pool) = pg_pool.or_else(|| engine.pg_pool()) {
        record_tick_hook_execution_pg(pool, label, &execution).await;
    }
    crate::kanban::drain_hook_side_effects_with_backends(None, engine);
}

async fn fire_tick_hook_by_name_with_timeout(
    engine: &PolicyEngine,
    hook_name: &str,
    label: &str,
    hook_timeout: Duration,
) -> PolicyTickHookExecution {
    let Some(in_flight_guard) = PolicyTickHookGuard::acquire(engine) else {
        tracing::warn!(
            "[policy-tick] {} skipped: previous tick hook is still running",
            label
        );
        return PolicyTickHookExecution {
            outcome: PolicyTickHookOutcome::SkippedInFlight,
            elapsed: Duration::ZERO,
            error: None,
        };
    };

    let start = std::time::Instant::now();
    let engine_for_task = engine.clone();
    let hook_name_owned = hook_name.to_string();
    let label_owned = label.to_string();
    let timed_out = std::sync::Arc::new(AtomicBool::new(false));
    let timed_out_for_task = timed_out.clone();
    let mut handle = tokio::task::spawn_blocking(move || {
        let _guard = in_flight_guard;
        let result = engine_for_task.try_fire_hook_by_name(&hook_name_owned, serde_json::json!({}));
        let elapsed = start.elapsed();
        if timed_out_for_task.load(Ordering::Acquire) {
            POLICY_TICK_POST_TIMEOUT_COMPLETIONS.fetch_add(1, Ordering::AcqRel);
            tracing::warn!(
                engine_label = engine_for_task.actor_label(),
                queue_depth = engine_for_task.actor_queue_depth(),
                "[policy-tick] {} finished after timeout in {}ms (post-timeout completion)",
                label_owned,
                elapsed.as_millis()
            );
        }
        match result {
            Ok(()) => PolicyTickHookExecution {
                outcome: PolicyTickHookOutcome::Ok,
                elapsed,
                error: None,
            },
            Err(error) => PolicyTickHookExecution {
                outcome: PolicyTickHookOutcome::Error,
                elapsed,
                error: Some(error.to_string()),
            },
        }
    });

    tokio::select! {
        joined = &mut handle => match joined {
            Ok(execution) => execution,
            Err(error) => PolicyTickHookExecution {
                outcome: PolicyTickHookOutcome::JoinError,
                elapsed: start.elapsed(),
                error: Some(error.to_string()),
            },
        },
        _ = tokio::time::sleep(hook_timeout) => {
            timed_out.store(true, Ordering::Release);
            POLICY_TICK_TIMEOUT_COUNT.fetch_add(1, Ordering::AcqRel);
            tracing::warn!(
                engine_label = engine.actor_label(),
                queue_depth = engine.actor_queue_depth(),
                timeout_ms = hook_timeout.as_millis() as u64,
                "[policy-tick] {} hook timed out; tick actor continues running in background",
                label
            );
            PolicyTickHookExecution {
                outcome: PolicyTickHookOutcome::Timeout,
                elapsed: start.elapsed(),
                error: None,
            }
        }
    }
}

#[cfg(test)]
fn record_tick_hook_execution(
    legacy_db: &crate::db::Db,
    label: &str,
    execution: &PolicyTickHookExecution,
) {
    let now_ms = chrono::Utc::now().timestamp_millis().to_string();
    let key_ms = format!("last_tick_{}_ms", label);
    let key_status = format!("last_tick_{}_status", label);
    let key_duration = format!("last_tick_{}_duration_ms", label);
    // #747 round-2: `*_skip_ms` tracks the last moment we *attempted* a tick
    // that was rejected by the in-flight guard. It advances for every
    // SkippedInFlight so operators have visibility into a wedged tick, but
    // `last_tick_*_ms` only advances for hooks that actually ran (Ok / Error
    // / JoinError / Timeout — the timed-out body continues in the
    // background and therefore still counts as tick progress).
    let key_skip_ms = format!("last_tick_{}_skip_ms", label);
    let status = execution.outcome.status();

    match execution.outcome {
        PolicyTickHookOutcome::Ok => {
            if execution.elapsed.as_millis() > POLICY_TICK_WARN_MS {
                tracing::warn!(
                    "[policy-tick] {} took {}ms",
                    label,
                    execution.elapsed.as_millis()
                );
            } else {
                tracing::debug!(
                    "[policy-tick] {} took {}ms",
                    label,
                    execution.elapsed.as_millis()
                );
            }
        }
        PolicyTickHookOutcome::Error | PolicyTickHookOutcome::JoinError => {
            tracing::warn!(
                "[policy-tick] {} hook {}: {}",
                label,
                status,
                execution.error.as_deref().unwrap_or("unknown error")
            );
        }
        PolicyTickHookOutcome::Timeout => {
            tracing::warn!(
                "[policy-tick] {} hook timed out after {}ms",
                label,
                execution.elapsed.as_millis()
            );
        }
        PolicyTickHookOutcome::SkippedInFlight => {}
    }

    let skipped_inflight = matches!(execution.outcome, PolicyTickHookOutcome::SkippedInFlight);

    if let Ok(conn) = legacy_db.lock() {
        // Always record the status + skip timestamp so dashboards can see
        // a skipped invocation happened. Duration for SkippedInFlight is
        // always ZERO, which is fine.
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
            libsql_rusqlite::params![key_status, status],
        )
        .ok();
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
            libsql_rusqlite::params![key_skip_ms, now_ms],
        )
        .ok();
        // The global last-skip marker is useful for at-a-glance health.
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('last_tick_skip_ms', ?1)",
            [&now_ms],
        )
        .ok();
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('last_tick_status', ?1)",
            [status],
        )
        .ok();

        if !skipped_inflight {
            // Only real hook executions (including Timeout, whose body
            // continues running in the background) advance the freshness
            // timestamps. This ensures a wedged tick becomes visibly
            // overdue on `/api/cron-jobs` instead of looking "recent".
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                libsql_rusqlite::params![key_ms, now_ms],
            )
            .ok();
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                libsql_rusqlite::params![key_duration, execution.elapsed.as_millis().to_string()],
            )
            .ok();
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('last_tick_ms', ?1)",
                [&now_ms],
            )
            .ok();
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('last_tick_duration_ms', ?1)",
                [execution.elapsed.as_millis().to_string()],
            )
            .ok();
        }
    }
}

async fn record_tick_hook_execution_pg(
    pg_pool: &PgPool,
    label: &str,
    execution: &PolicyTickHookExecution,
) {
    let now_ms = chrono::Utc::now().timestamp_millis().to_string();
    let key_ms = format!("last_tick_{}_ms", label);
    let key_status = format!("last_tick_{}_status", label);
    let key_duration = format!("last_tick_{}_duration_ms", label);
    let key_skip_ms = format!("last_tick_{}_skip_ms", label);
    let status = execution.outcome.status();

    match execution.outcome {
        PolicyTickHookOutcome::Ok => {
            if execution.elapsed.as_millis() > POLICY_TICK_WARN_MS {
                tracing::warn!(
                    "[policy-tick] {} took {}ms",
                    label,
                    execution.elapsed.as_millis()
                );
            } else {
                tracing::debug!(
                    "[policy-tick] {} took {}ms",
                    label,
                    execution.elapsed.as_millis()
                );
            }
        }
        PolicyTickHookOutcome::Error | PolicyTickHookOutcome::JoinError => {
            tracing::warn!(
                "[policy-tick] {} hook {}: {}",
                label,
                status,
                execution.error.as_deref().unwrap_or("unknown error")
            );
        }
        PolicyTickHookOutcome::Timeout => {
            tracing::warn!(
                "[policy-tick] {} hook timed out after {}ms",
                label,
                execution.elapsed.as_millis()
            );
        }
        PolicyTickHookOutcome::SkippedInFlight => {}
    }

    let upsert_sql = "INSERT INTO kv_meta (key, value)
                      VALUES ($1, $2)
                      ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value";
    let skipped_inflight = matches!(execution.outcome, PolicyTickHookOutcome::SkippedInFlight);

    sqlx::query(upsert_sql)
        .bind(&key_status)
        .bind(status)
        .execute(pg_pool)
        .await
        .ok();
    sqlx::query(upsert_sql)
        .bind(&key_skip_ms)
        .bind(&now_ms)
        .execute(pg_pool)
        .await
        .ok();
    sqlx::query(upsert_sql)
        .bind("last_tick_skip_ms")
        .bind(&now_ms)
        .execute(pg_pool)
        .await
        .ok();
    sqlx::query(upsert_sql)
        .bind("last_tick_status")
        .bind(status)
        .execute(pg_pool)
        .await
        .ok();

    if !skipped_inflight {
        let elapsed_ms = execution.elapsed.as_millis().to_string();
        sqlx::query(upsert_sql)
            .bind(&key_ms)
            .bind(&now_ms)
            .execute(pg_pool)
            .await
            .ok();
        sqlx::query(upsert_sql)
            .bind(&key_duration)
            .bind(&elapsed_ms)
            .execute(pg_pool)
            .await
            .ok();
        sqlx::query(upsert_sql)
            .bind("last_tick_ms")
            .bind(&now_ms)
            .execute(pg_pool)
            .await
            .ok();
        sqlx::query(upsert_sql)
            .bind("last_tick_duration_ms")
            .bind(&elapsed_ms)
            .execute(pg_pool)
            .await
            .ok();
    }
}

#[cfg(test)]
pub(crate) async fn fire_tick_hook_by_name_for_test(
    engine: &PolicyEngine,
    legacy_db: &crate::db::Db,
    hook_name: &str,
    label: &str,
    hook_timeout: Duration,
) -> PolicyTickHookOutcome {
    let execution =
        fire_tick_hook_by_name_with_timeout(engine, hook_name, label, hook_timeout).await;
    record_tick_hook_execution(legacy_db, label, &execution);
    execution.outcome
}

async fn upsert_kv_meta_pg_ignore(pg_pool: &PgPool, key: &str, value: &str) {
    sqlx::query(
        "INSERT INTO kv_meta (key, value)
         VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(key)
    .bind(value)
    .execute(pg_pool)
    .await
    .ok();
}

async fn record_periodic_job_execution_pg(
    pg_pool: &PgPool,
    label: &str,
    status: &str,
    elapsed: std::time::Duration,
) {
    let now_ms = chrono::Utc::now().timestamp_millis().to_string();
    let elapsed_ms = elapsed.as_millis().to_string();
    let key_ms = format!("last_tick_{}_ms", label);
    let key_status = format!("last_tick_{}_status", label);
    let key_duration = format!("last_tick_{}_duration_ms", label);

    upsert_kv_meta_pg_ignore(pg_pool, &key_ms, &now_ms).await;
    upsert_kv_meta_pg_ignore(pg_pool, &key_status, status).await;
    upsert_kv_meta_pg_ignore(pg_pool, &key_duration, &elapsed_ms).await;
}

/// Background task that periodically fetches rate-limit data from external providers
/// and caches it in the `rate_limit_cache` table for the dashboard API.
async fn upsert_rate_limit_cache_entry(
    pg_pool: &PgPool,
    provider: &str,
    data: &str,
    fetched_at: i64,
) {
    if let Err(error) = sqlx::query(
        "INSERT INTO rate_limit_cache (provider, data, fetched_at)
         VALUES ($1, $2, $3)
         ON CONFLICT (provider)
         DO UPDATE SET data = EXCLUDED.data, fetched_at = EXCLUDED.fetched_at",
    )
    .bind(provider)
    .bind(data)
    .bind(fetched_at)
    .execute(pg_pool)
    .await
    {
        tracing::warn!(
            "[rate-limit-sync] failed to upsert rate_limit_cache row for {provider}: {error}"
        );
    }
}

async fn rate_limit_sync_loop(pg_pool: Arc<PgPool>) {
    use std::time::Duration;

    let interval = Duration::from_secs(120);
    // Run immediately on startup, then every 2 minutes
    let mut first = true;

    loop {
        if !first {
            tokio::time::sleep(interval).await;
        }
        first = false;

        // --- Claude rate limits ---
        // Priority: 1) OAuth token (Claude Code subscription), 2) ANTHROPIC_API_KEY
        let claude_result = if let Some(token) = get_claude_oauth_token() {
            fetch_claude_oauth_usage(&token).await
        } else if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
            fetch_anthropic_rate_limits(&api_key).await
        } else {
            Err(anyhow::anyhow!("no Claude credentials found"))
        };
        match claude_result {
            Ok(buckets) => {
                let data = serde_json::json!({ "buckets": buckets }).to_string();
                let now = chrono::Utc::now().timestamp();
                upsert_rate_limit_cache_entry(pg_pool.as_ref(), "claude", &data, now).await;
                tracing::info!("[rate-limit-sync] Claude: {} buckets cached", buckets.len());
            }
            Err(e) => {
                tracing::warn!("[rate-limit-sync] Claude rate_limit fetch failed: {e}");
            }
        }

        // --- Codex rate limits ---
        // Priority: 1) ~/.codex/auth.json (Codex CLI subscription), 2) OPENAI_API_KEY
        let codex_result = if let Some(token) = load_codex_access_token() {
            fetch_codex_oauth_usage(&token).await
        } else if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
            fetch_openai_rate_limits(&api_key).await
        } else {
            Err(anyhow::anyhow!("no Codex credentials found"))
        };
        match codex_result {
            Ok(buckets) => {
                let data = serde_json::json!({ "buckets": buckets }).to_string();
                let now = chrono::Utc::now().timestamp();
                upsert_rate_limit_cache_entry(pg_pool.as_ref(), "codex", &data, now).await;
                tracing::info!("[rate-limit-sync] Codex: {} buckets cached", buckets.len());
            }
            Err(e) => {
                tracing::warn!("[rate-limit-sync] Codex rate_limit fetch failed: {e}");
            }
        }

        // --- Gemini rate limits ---
        // Uses OAuth2 creds from ~/.gemini/oauth_creds.json.
        // Returns RPM/RPD buckets with known quota limits; usage fields are -1 (unavailable).
        match fetch_gemini_rate_limits().await {
            Ok(buckets) => {
                let n = buckets.len();
                let data = serde_json::json!({ "buckets": buckets }).to_string();
                let now = chrono::Utc::now().timestamp();
                upsert_rate_limit_cache_entry(pg_pool.as_ref(), "gemini", &data, now).await;
                tracing::info!("[rate-limit-sync] Gemini: {} buckets cached", n);
            }
            Err(e) => {
                tracing::warn!("[rate-limit-sync] Gemini rate_limit fetch failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn test_db() -> crate::db::Db {
        crate::db::test_db()
    }

    fn server_test_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn repo_policies_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies")
    }

    fn test_engine_with_dir(db: &crate::db::Db, dir: &std::path::Path) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = dir.to_path_buf();
        config.policies.hot_reload = false;
        PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap()
    }

    fn kv_value(db: &crate::db::Db, key: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row("SELECT value FROM kv_meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .ok()
    }

    fn repo_ids(db: &crate::db::Db) -> Vec<String> {
        crate::github::list_repos(db)
            .unwrap()
            .into_iter()
            .map(|repo| repo.id)
            .collect()
    }

    fn agent_ids(db: &crate::db::Db) -> Vec<String> {
        let conn = db.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id FROM agents ORDER BY id ASC")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    }

    fn insert_pending_message(db: &crate::db::Db, target: &str, content: &str) -> i64 {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO message_outbox (target, content, bot, source) VALUES (?1, ?2, 'notify', 'system')",
            libsql_rusqlite::params![target, content],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn message_row_status(db: &crate::db::Db, id: i64) -> (String, Option<String>, Option<String>) {
        db.lock()
            .unwrap()
            .query_row(
                "SELECT status, error, sent_at FROM message_outbox WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
    }

    struct TestPostgresDb {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let admin_url = postgres_admin_database_url();
            let database_name = format!("agentdesk_server_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(&admin_url, &database_name, "server tests")
                .await
                .unwrap();

            Self {
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(&self.database_url, "server tests")
                .await
                .unwrap()
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "server tests",
            )
            .await
            .unwrap();
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

    #[test]
    fn extract_gemini_quota_limits_preserves_paid_tier_values() {
        let payload = json!({
            "metrics": [{
                "metric": "generativelanguage.googleapis.com/generate_content_free_tier_requests",
                "consumerQuotaLimits": [
                    {
                        "unit": "1/min/{project}",
                        "quotaBuckets": [
                            {"effectiveLimit": "200"},
                            {"effectiveLimit": 120}
                        ]
                    },
                    {
                        "unit": "1/d/{project}",
                        "quotaBuckets": [
                            {"effectiveLimit": 20000},
                            {"effectiveLimit": 18000}
                        ]
                    }
                ]
            }]
        });

        let (rpm_limit, rpd_limit) = extract_gemini_quota_limits(&payload);
        assert_eq!(rpm_limit, 120);
        assert_eq!(rpd_limit, 18000);
    }

    #[test]
    fn extract_gemini_quota_limits_accepts_string_effective_limits() {
        let payload = json!({
            "metrics": [{
                "metric": "generativelanguage.googleapis.com/generate_content_free_tier_requests",
                "consumerQuotaLimits": [
                    {
                        "unit": "1/min/{project}",
                        "quotaBuckets": [
                            {"effectiveLimit": "200"},
                            {"effectiveLimit": "180"}
                        ]
                    },
                    {
                        "unit": "1/d/{project}",
                        "quotaBuckets": [
                            {"effectiveLimit": "30000"},
                            {"effectiveLimit": "20000"}
                        ]
                    }
                ]
            }]
        });

        let (rpm_limit, rpd_limit) = extract_gemini_quota_limits(&payload);
        assert_eq!(rpm_limit, 180);
        assert_eq!(rpd_limit, 20000);
    }

    #[test]
    fn build_gemini_rate_limit_buckets_uses_non_negative_usage_placeholders() {
        let buckets = build_gemini_rate_limit_buckets(15, 1500);

        assert_eq!(buckets[0]["used"], json!(0));
        assert_eq!(buckets[0]["remaining"], json!(15));
        assert_eq!(buckets[1]["used"], json!(0));
        assert_eq!(buckets[1]["remaining"], json!(1500));
    }

    #[tokio::test]
    async fn startup_memory_health_refresh_uses_startup_reason() {
        let _guard = server_test_lock();
        crate::services::memory::reset_backend_health_for_tests();
        refresh_memory_health_for_startup().await;
        assert_eq!(
            crate::services::memory::last_refresh_reason_for_tests().as_deref(),
            Some(MEMORY_HEALTH_STARTUP_REASON)
        );
    }

    #[test]
    fn five_min_policy_tick_runs_on_every_tenth_iteration() {
        assert!(!is_five_min_policy_tick(1));
        assert!(!is_five_min_policy_tick(9));
        assert!(is_five_min_policy_tick(10));
        assert!(is_five_min_policy_tick(20));
    }

    #[tokio::test]
    async fn five_min_memory_health_refresh_uses_tick_reason() {
        let _guard = server_test_lock();
        crate::services::memory::reset_backend_health_for_tests();
        refresh_memory_health_for_five_min_tick().await;
        assert_eq!(
            crate::services::memory::last_refresh_reason_for_tests().as_deref(),
            Some(MEMORY_HEALTH_FIVE_MIN_REASON)
        );
    }

    #[test]
    fn seed_startup_runtime_state_records_server_port_and_registered_repos() {
        let db = test_db();
        let mut config = crate::config::Config::default();
        config.server.port = 43121;
        config.github.repos = vec!["owner/repo-a".to_string(), "owner/repo-b".to_string()];
        config.agents = vec![crate::config::AgentDef {
            id: "project-agentdesk".to_string(),
            name: "AgentDesk".to_string(),
            name_ko: None,
            provider: "claude".to_string(),
            channels: crate::config::AgentChannels::default(),
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
        }];

        seed_startup_runtime_state(&db, &config);

        assert_eq!(kv_value(&db, "server_port").as_deref(), Some("43121"));
        assert_eq!(
            repo_ids(&db),
            vec!["owner/repo-a".to_string(), "owner/repo-b".to_string()]
        );
        assert_eq!(agent_ids(&db), vec!["project-agentdesk".to_string()]);
    }

    #[test]
    fn seed_startup_runtime_state_deduplicates_and_skips_invalid_repo_entries() {
        let db = test_db();
        let mut config = crate::config::Config::default();
        config.github.repos = vec![
            " owner/repo-a ".to_string(),
            "owner/repo-a".to_string(),
            "".to_string(),
            "noslash".to_string(),
            "owner/repo-b".to_string(),
        ];

        seed_startup_runtime_state(&db, &config);
        seed_startup_runtime_state(&db, &config);

        assert_eq!(
            repo_ids(&db),
            vec!["owner/repo-a".to_string(), "owner/repo-b".to_string()]
        );
    }

    #[test]
    fn sqlite_startup_fallback_can_skip_runtime_kv_seeding() {
        let db = test_db();
        let mut config = crate::config::Config::default();
        config.server.port = 43121;
        config.github.repos = vec!["owner/repo-a".to_string()];
        config.agents = vec![crate::config::AgentDef {
            id: "project-agentdesk".to_string(),
            name: "AgentDesk".to_string(),
            name_ko: None,
            provider: "claude".to_string(),
            channels: crate::config::AgentChannels::default(),
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
        }];

        seed_sqlite_startup_runtime_state(&db, &config, false);

        assert_eq!(kv_value(&db, "server_port"), None);
        assert_eq!(repo_ids(&db), vec!["owner/repo-a".to_string()]);
        assert_eq!(agent_ids(&db), vec!["project-agentdesk".to_string()]);
    }

    #[tokio::test]
    async fn tiered_tick_hooks_record_expected_markers_per_label() {
        crate::services::memory::reset_backend_health_for_tests();
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("tick-tier-probe.js"),
            r#"
            agentdesk.registerPolicy({
                name: "tick-tier-probe",
                priority: 1,
                onTick30s: function() {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('probe_30s', 'hit')"
                    );
                },
                onTick1min: function() {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('probe_1min', 'hit')"
                    );
                },
                onTick5min: function() {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('probe_5min', 'hit')"
                    );
                },
                onTick: function() {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('probe_legacy', 'hit')"
                    );
                }
            });
            "#,
        )
        .unwrap();

        let db = test_db();
        let engine = test_engine_with_dir(&db, dir.path());

        fire_tick_hook_by_name(&engine, &db, "OnTick30s", "30s").await;
        assert_eq!(kv_value(&db, "probe_30s").as_deref(), Some("hit"));
        assert_eq!(kv_value(&db, "last_tick_30s_status").as_deref(), Some("ok"));
        assert!(kv_value(&db, "last_tick_30s_ms").is_some());
        assert_eq!(kv_value(&db, "last_tick_status").as_deref(), Some("ok"));
        assert!(kv_value(&db, "last_tick_ms").is_some());
        assert_eq!(kv_value(&db, "probe_1min"), None);
        assert_eq!(kv_value(&db, "probe_5min"), None);
        assert_eq!(kv_value(&db, "probe_legacy"), None);

        fire_tick_hook_by_name(&engine, &db, "OnTick1min", "1min").await;
        assert_eq!(kv_value(&db, "probe_1min").as_deref(), Some("hit"));
        assert_eq!(
            kv_value(&db, "last_tick_1min_status").as_deref(),
            Some("ok")
        );
        assert!(kv_value(&db, "last_tick_1min_ms").is_some());
        assert_eq!(kv_value(&db, "last_tick_status").as_deref(), Some("ok"));

        fire_tick_hook_by_name(&engine, &db, "OnTick5min", "5min").await;
        assert_eq!(kv_value(&db, "probe_5min").as_deref(), Some("hit"));
        assert_eq!(
            kv_value(&db, "last_tick_5min_status").as_deref(),
            Some("ok")
        );
        assert!(kv_value(&db, "last_tick_5min_ms").is_some());
        assert_eq!(kv_value(&db, "last_tick_status").as_deref(), Some("ok"));

        fire_tick_hook_by_name(&engine, &db, "OnTick", "legacy").await;
        assert_eq!(kv_value(&db, "probe_legacy").as_deref(), Some("hit"));
        assert_eq!(
            kv_value(&db, "last_tick_legacy_status").as_deref(),
            Some("ok")
        );
        assert!(kv_value(&db, "last_tick_legacy_ms").is_some());
        assert!(kv_value(&db, "last_tick_ms").is_some());
        assert!(kv_value(&db, "last_tick_duration_ms").is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn timed_out_tick_marks_status_and_skips_overlapping_runs() {
        let db = test_db();
        let dir = tempfile::TempDir::new().unwrap();
        let engine = test_engine_with_dir(&db, dir.path());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker_engine = engine.clone();
        let blocker = std::thread::spawn(move || {
            blocker_engine
                .block_actor_for_test(entered_tx, release_rx)
                .unwrap();
        });
        entered_rx.recv().unwrap();

        let timeout_outcome = fire_tick_hook_by_name_for_test(
            &engine,
            &db,
            "OnTick1min",
            "1min",
            Duration::from_millis(50),
        )
        .await;
        assert_eq!(timeout_outcome, PolicyTickHookOutcome::Timeout);
        assert_eq!(
            kv_value(&db, "last_tick_1min_status").as_deref(),
            Some("timeout")
        );

        let skipped_outcome = fire_tick_hook_by_name_for_test(
            &engine,
            &db,
            "OnTick1min",
            "1min",
            Duration::from_millis(50),
        )
        .await;
        assert_eq!(skipped_outcome, PolicyTickHookOutcome::SkippedInFlight);
        assert_eq!(
            kv_value(&db, "last_tick_1min_status").as_deref(),
            Some("skipped_inflight")
        );

        release_tx.send(()).unwrap();
        blocker.join().unwrap();

        let ok_outcome = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let outcome = fire_tick_hook_by_name_for_test(
                    &engine,
                    &db,
                    "OnTick1min",
                    "1min",
                    Duration::from_millis(50),
                )
                .await;
                if outcome != PolicyTickHookOutcome::SkippedInFlight {
                    break outcome;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("timed-out tick should release the in-flight guard once background work finishes");
        assert_eq!(ok_outcome, PolicyTickHookOutcome::Ok);
        assert_eq!(
            kv_value(&db, "last_tick_1min_status").as_deref(),
            Some("ok")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tick_in_flight_guard_is_scoped_per_engine_instance() {
        let blocked_db = test_db();
        let blocked_dir = tempfile::TempDir::new().unwrap();
        let blocked_engine = test_engine_with_dir(&blocked_db, blocked_dir.path());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker_engine = blocked_engine.clone();
        let blocker = std::thread::spawn(move || {
            blocker_engine
                .block_actor_for_test(entered_tx, release_rx)
                .unwrap();
        });
        entered_rx.recv().unwrap();

        let timed_out = fire_tick_hook_by_name_for_test(
            &blocked_engine,
            &blocked_db,
            "OnTick1min",
            "1min",
            Duration::from_millis(50),
        )
        .await;
        assert_eq!(timed_out, PolicyTickHookOutcome::Timeout);

        let free_db = test_db();
        let free_dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            free_dir.path().join("tick-probe.js"),
            r#"
            agentdesk.registerPolicy({
                name: "tick-probe",
                priority: 1,
                onTick30s: function() {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('probe_engine_local', 'hit')"
                    );
                }
            });
            "#,
        )
        .unwrap();
        let free_engine = test_engine_with_dir(&free_db, free_dir.path());
        let free_outcome = fire_tick_hook_by_name_for_test(
            &free_engine,
            &free_db,
            "OnTick30s",
            "30s",
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(free_outcome, PolicyTickHookOutcome::Ok);
        assert_eq!(
            kv_value(&free_db, "probe_engine_local").as_deref(),
            Some("hit")
        );

        release_tx.send(()).unwrap();
        blocker.join().unwrap();
    }

    /// Regression for #747: when the tick engine's actor is stuck executing a
    /// long-running hook, firing a regular hook on the *main* engine must not
    /// be queued behind that tick hook. The two engines run on independent
    /// actor threads, so the main engine's fire_hook should return promptly.
    ///
    /// Also verifies that the timeout + post-timeout observability counters
    /// move as expected when the tick hook eventually finishes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stuck_tick_hook_does_not_block_main_engine_hook_calls() {
        use crate::engine::hooks::Hook;

        let db = test_db();

        // Main engine — this is what HTTP/Discord callers use.
        let main_dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            main_dir.path().join("main-probe.js"),
            r#"
            agentdesk.registerPolicy({
                name: "main-probe",
                priority: 1,
                onTick30s: function() {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('main_engine_hit', 'yes')"
                    );
                }
            });
            "#,
        )
        .unwrap();
        let main_engine = test_engine_with_dir(&db, main_dir.path());

        // Tick engine — has its own actor. Completely separate from `main_engine`.
        let tick_dir = tempfile::TempDir::new().unwrap();
        let tick_engine = test_engine_with_dir(&db, tick_dir.path());

        let timeout_before = policy_tick_timeout_count();
        let post_timeout_before = policy_tick_post_timeout_completions();

        // Block the tick engine's actor so any tick hook send() sits in the
        // actor queue until we release the blocker.
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker_engine = tick_engine.clone();
        let blocker = std::thread::spawn(move || {
            blocker_engine
                .block_actor_for_test(entered_tx, release_rx)
                .unwrap();
        });
        entered_rx.recv().unwrap();

        // Tick hook against the stuck tick engine — should time out quickly.
        let tick_outcome = fire_tick_hook_by_name_for_test(
            &tick_engine,
            &db,
            "OnTick30s",
            "30s",
            Duration::from_millis(50),
        )
        .await;
        assert_eq!(tick_outcome, PolicyTickHookOutcome::Timeout);
        // The counter is a global static; other tick tests may bump it in
        // parallel. Assert monotonic growth instead of an exact delta.
        assert!(
            policy_tick_timeout_count() > timeout_before,
            "timed-out tick should bump the timeout counter (before={} after={})",
            timeout_before,
            policy_tick_timeout_count()
        );

        // The tick actor is still holding the BlockActor command. Now fire a
        // regular hook against the *main* engine — this must return promptly
        // because the main engine has its own independent actor thread.
        let main_start = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(2), async {
            // Run the engine call on the current-thread runtime via
            // spawn_blocking so fire_hook doesn't wedge the reactor.
            tokio::task::spawn_blocking({
                let main_engine = main_engine.clone();
                move || {
                    main_engine
                        .fire_hook(Hook::OnTick30s, serde_json::json!({}))
                        .unwrap()
                }
            })
            .await
            .unwrap();
        })
        .await
        .expect("main engine fire_hook must complete while the tick engine is stuck");
        let main_elapsed = main_start.elapsed();
        assert!(
            main_elapsed < Duration::from_millis(500),
            "main engine hook should not wait on the blocked tick engine (elapsed={:?})",
            main_elapsed
        );
        assert_eq!(
            kv_value(&db, "main_engine_hit").as_deref(),
            Some("yes"),
            "main engine hook must have actually fired its side effect"
        );

        // Release the blocked tick actor so it drains the queued BlockActor
        // command. Once the background fire_hook task finishes, it records a
        // post-timeout completion log + counter bump.
        release_tx.send(()).unwrap();
        blocker.join().unwrap();

        // Drain any remaining queued tick hook so the post-timeout counter
        // reflects its completion. We loop until skipped_inflight clears.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let outcome = fire_tick_hook_by_name_for_test(
                    &tick_engine,
                    &db,
                    "OnTick30s",
                    "30s",
                    Duration::from_millis(200),
                )
                .await;
                if outcome == PolicyTickHookOutcome::Ok {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("tick engine eventually drains once unblocked");

        assert!(
            policy_tick_post_timeout_completions() > post_timeout_before,
            "post-timeout completion counter must record the late finish (before={} after={})",
            post_timeout_before,
            policy_tick_post_timeout_completions()
        );
    }

    /// Regression for #747: a timed-out tick hook must NOT leak its in-flight
    /// guard, so a later (non-concurrent) tick call on the same engine can
    /// acquire it again once the stuck work finishes. Together with
    /// `timed_out_tick_marks_status_and_skips_overlapping_runs`, this pins the
    /// `skipped_inflight` contract in place across the engine split.
    #[tokio::test(flavor = "current_thread")]
    async fn tick_skipped_inflight_guard_still_blocks_overlap_on_tick_engine() {
        let db = test_db();
        let dir = tempfile::TempDir::new().unwrap();
        let engine = test_engine_with_dir(&db, dir.path());

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker_engine = engine.clone();
        let blocker = std::thread::spawn(move || {
            blocker_engine
                .block_actor_for_test(entered_tx, release_rx)
                .unwrap();
        });
        entered_rx.recv().unwrap();

        let timed_out = fire_tick_hook_by_name_for_test(
            &engine,
            &db,
            "OnTick30s",
            "30s",
            Duration::from_millis(50),
        )
        .await;
        assert_eq!(timed_out, PolicyTickHookOutcome::Timeout);

        // Second call while the previous hook is still running on the actor
        // must hit the skipped_inflight guard.
        let skipped = fire_tick_hook_by_name_for_test(
            &engine,
            &db,
            "OnTick30s",
            "30s",
            Duration::from_millis(50),
        )
        .await;
        assert_eq!(
            skipped,
            PolicyTickHookOutcome::SkippedInFlight,
            "skipped_inflight contract must hold even with the split engines"
        );

        release_tx.send(()).unwrap();
        blocker.join().unwrap();
    }

    /// Regression for #747 round-2 Finding 2: a `SkippedInFlight` tick must
    /// NOT advance `last_tick_<tier>_ms` / `last_tick_ms`. Those fields are
    /// exposed by `cron_api` as `lastRunAtMs` / `nextRunAtMs`; if they
    /// advance on skip, a wedged tick shows "recent" on the dashboard while
    /// no hook body is actually progressing. A skip must ONLY advance the
    /// new `last_tick_<tier>_skip_ms` / `last_tick_skip_ms` fields.
    #[tokio::test(flavor = "current_thread")]
    async fn skipped_inflight_does_not_advance_last_tick_ms() {
        let db = test_db();
        let dir = tempfile::TempDir::new().unwrap();
        let engine = test_engine_with_dir(&db, dir.path());

        // Wedge the actor so the first hook times out and the second call
        // hits SkippedInFlight.
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker_engine = engine.clone();
        let blocker = std::thread::spawn(move || {
            blocker_engine
                .block_actor_for_test(entered_tx, release_rx)
                .unwrap();
        });
        entered_rx.recv().unwrap();

        // First call: hook body runs past the deadline → Timeout. The body
        // is still running in the background — this counts as "tick
        // progress" for freshness purposes, so it SHOULD advance
        // last_tick_1min_ms.
        let timeout_outcome = fire_tick_hook_by_name_for_test(
            &engine,
            &db,
            "OnTick1min",
            "1min",
            Duration::from_millis(50),
        )
        .await;
        assert_eq!(timeout_outcome, PolicyTickHookOutcome::Timeout);
        let tick_ms_after_timeout = kv_value(&db, "last_tick_1min_ms")
            .and_then(|v| v.parse::<i64>().ok())
            .expect("Timeout must advance last_tick_1min_ms");
        let global_tick_ms_after_timeout = kv_value(&db, "last_tick_ms")
            .and_then(|v| v.parse::<i64>().ok())
            .expect("Timeout must advance last_tick_ms");

        // Need strictly different millisecond reads; sleep just past 1ms.
        tokio::time::sleep(Duration::from_millis(5)).await;

        // Second call while the first is still holding the in-flight guard
        // → SkippedInFlight. This must NOT touch last_tick_1min_ms or
        // last_tick_ms, but MUST advance last_tick_1min_skip_ms /
        // last_tick_skip_ms.
        let skipped_outcome = fire_tick_hook_by_name_for_test(
            &engine,
            &db,
            "OnTick1min",
            "1min",
            Duration::from_millis(50),
        )
        .await;
        assert_eq!(skipped_outcome, PolicyTickHookOutcome::SkippedInFlight);

        let tick_ms_after_skip = kv_value(&db, "last_tick_1min_ms")
            .and_then(|v| v.parse::<i64>().ok())
            .expect("last_tick_1min_ms must still be present after skip");
        let global_tick_ms_after_skip = kv_value(&db, "last_tick_ms")
            .and_then(|v| v.parse::<i64>().ok())
            .expect("last_tick_ms must still be present after skip");
        assert_eq!(
            tick_ms_after_skip, tick_ms_after_timeout,
            "SkippedInFlight must NOT advance last_tick_1min_ms"
        );
        assert_eq!(
            global_tick_ms_after_skip, global_tick_ms_after_timeout,
            "SkippedInFlight must NOT advance last_tick_ms"
        );

        // The new skip freshness markers should be populated.
        let skip_ms = kv_value(&db, "last_tick_1min_skip_ms")
            .and_then(|v| v.parse::<i64>().ok())
            .expect("SkippedInFlight must populate last_tick_1min_skip_ms");
        assert!(
            skip_ms >= tick_ms_after_timeout,
            "skip marker should be >= tick marker (skip={skip_ms}, tick={tick_ms_after_timeout})"
        );
        assert!(
            kv_value(&db, "last_tick_skip_ms").is_some(),
            "SkippedInFlight must populate global last_tick_skip_ms"
        );

        release_tx.send(()).unwrap();
        blocker.join().unwrap();
    }

    /// Regression for #747 round-2 Finding 1: the auto-queue phase-gate
    /// race between `onCardTerminal` (main engine) and `onTick1min`
    /// (tick engine). After the last entry is marked `done`, a tick-side
    /// `finalizeRunWithoutPhaseGate()` must NOT complete a run while its
    /// owning `onCardTerminal` is still creating phase-gate dispatches.
    ///
    /// The grace window column `phase_gate_grace_until` is set by the
    /// main engine's `onCardTerminal` path BEFORE it calls
    /// `_createPhaseGateDispatches`, so the tick's finalize call refuses
    /// to finalize the run until the grace window expires (or
    /// `onCardTerminal` clears it on a non-phase-gate exit).
    #[tokio::test(flavor = "current_thread")]
    async fn phase_gate_grace_window_blocks_tick_finalize_race() {
        let db = test_db();
        // Install a minimal probe policy that exposes just the
        // finalizeRunWithoutPhaseGate behavior we want to exercise. We
        // re-implement the helper here mirroring the production policy so
        // the test stays self-contained.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("grace-window-probe.js"),
            r#"
            var PHASE_GATE_GRACE_WINDOW_MS = 30 * 1000;

            function runWithinPhaseGateGrace(runId) {
                var rows = agentdesk.db.query(
                    "SELECT phase_gate_grace_until FROM auto_queue_runs WHERE id = ?",
                    [runId]
                );
                if (rows.length === 0 || !rows[0].phase_gate_grace_until) return false;
                var until = Date.parse(rows[0].phase_gate_grace_until);
                if (!isFinite(until)) return false;
                return Date.now() < until;
            }

            function finalizeRunWithoutPhaseGate(runId) {
                if (runWithinPhaseGateGrace(runId)) {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('grace_probe_result', 'deferred')"
                    );
                    return false;
                }
                agentdesk.db.execute(
                    "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('grace_probe_result', 'finalized')"
                );
                return true;
            }

            agentdesk.registerPolicy({
                name: "grace-window-probe",
                priority: 1,
                onTick1min: function() {
                    finalizeRunWithoutPhaseGate("run-race-1");
                }
            });
            "#,
        )
        .unwrap();

        // Seed the DB with an auto_queue_runs row and set the grace window
        // into the future (simulating onCardTerminal having just started
        // its continuation work on the main engine).
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status, phase_gate_grace_until)
                 VALUES ('run-race-1', 'test/repo', 'agent-1', 'active',
                         strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '+30 seconds'))",
                [],
            )
            .unwrap();
        }

        let engine = test_engine_with_dir(&db, dir.path());
        let outcome = fire_tick_hook_by_name_for_test(
            &engine,
            &db,
            "OnTick1min",
            "1min",
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(outcome, PolicyTickHookOutcome::Ok);
        assert_eq!(
            kv_value(&db, "grace_probe_result").as_deref(),
            Some("deferred"),
            "tick-side finalize must defer while the grace window is active"
        );

        // Now clear the grace window (simulating onCardTerminal finishing
        // without a phase-gate path) and re-fire: the tick should now be
        // allowed to finalize.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE auto_queue_runs SET phase_gate_grace_until = NULL WHERE id = 'run-race-1'",
                [],
            )
            .unwrap();
        }

        let outcome2 = fire_tick_hook_by_name_for_test(
            &engine,
            &db,
            "OnTick1min",
            "1min",
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(outcome2, PolicyTickHookOutcome::Ok);
        assert_eq!(
            kv_value(&db, "grace_probe_result").as_deref(),
            Some("finalized"),
            "once the grace window is cleared, finalize may proceed"
        );
    }

    #[tokio::test]
    #[ignore = "manual profiling baseline for #735 docs"]
    async fn profile_real_policy_tick_hooks_empty_db_baseline() {
        let db = test_db();
        let config = crate::config::Config::default();
        seed_startup_runtime_state(&db, &config);
        let engine = test_engine_with_dir(&db, &repo_policies_dir());

        let started_1min = std::time::Instant::now();
        let on_tick_1min = fire_tick_hook_by_name_for_test(
            &engine,
            &db,
            "OnTick1min",
            "1min",
            Duration::from_secs(30),
        )
        .await;
        let elapsed_1min = started_1min.elapsed();

        let started_5min = std::time::Instant::now();
        let on_tick_5min = fire_tick_hook_by_name_for_test(
            &engine,
            &db,
            "OnTick5min",
            "5min",
            Duration::from_secs(30),
        )
        .await;
        let elapsed_5min = started_5min.elapsed();

        println!(
            "profile_real_policy_tick_hooks_empty_db_baseline onTick1min outcome={on_tick_1min:?} elapsed_ms={}",
            elapsed_1min.as_millis()
        );
        println!(
            "profile_real_policy_tick_hooks_empty_db_baseline onTick5min outcome={on_tick_5min:?} elapsed_ms={}",
            elapsed_5min.as_millis()
        );

        assert_eq!(on_tick_1min, PolicyTickHookOutcome::Ok);
        assert_eq!(on_tick_5min, PolicyTickHookOutcome::Ok);
    }

    #[tokio::test]
    async fn drain_message_outbox_batch_marks_successful_rows_sent() {
        let db = test_db();
        let message_id = insert_pending_message(&db, "channel:1492506767085801535", "hello");
        let delivered = Arc::new(Mutex::new(Vec::new()));

        let processed = drain_message_outbox_batch_once_sqlite(&db, {
            let delivered = delivered.clone();
            move |target, content, source, bot| {
                let delivered = delivered.clone();
                async move {
                    delivered.lock().unwrap().push(json!({
                        "target": target,
                        "content": content,
                        "source": source,
                        "bot": bot,
                    }));
                    ("200 OK".to_string(), json!({"ok": true}).to_string())
                }
            }
        })
        .await;

        let captured = delivered.lock().unwrap().clone();
        let (status, error, sent_at) = message_row_status(&db, message_id);

        assert_eq!(processed, 1, "one pending outbox row should be drained");
        assert_eq!(status, "sent");
        assert_eq!(error, None);
        assert!(sent_at.is_some(), "successful delivery must stamp sent_at");
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0]["target"], "channel:1492506767085801535");
        assert_eq!(captured[0]["content"], "hello");
        assert_eq!(captured[0]["source"], "system");
        assert_eq!(captured[0]["bot"], "notify");
    }

    #[tokio::test]
    async fn drain_message_outbox_batch_marks_http_failures_failed() {
        let db = test_db();
        let message_id = insert_pending_message(&db, "channel:1492506767085801535", "boom");

        let processed =
            drain_message_outbox_batch_once_sqlite(&db, |_target, _content, _source, _bot| async {
                (
                    "500 Internal Server Error".to_string(),
                    json!({"error": "mock failure"}).to_string(),
                )
            })
            .await;

        let (status, error, sent_at) = message_row_status(&db, message_id);

        assert_eq!(
            processed, 1,
            "failed deliveries still consume the pending outbox row"
        );
        assert_eq!(status, "failed");
        assert_eq!(sent_at, None);
        let error = error.expect("failed rows must persist error details");
        assert!(error.contains("500 Internal Server Error"));
        assert!(error.contains("mock failure"));
    }

    #[tokio::test]
    async fn claim_pending_message_outbox_batch_pg_reclaims_stale_processing_rows() {
        let _guard = server_test_lock();
        let pg_db = TestPostgresDb::create().await;
        let pg_pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO message_outbox (
                target, content, bot, source, status, claimed_at, claim_owner
             ) VALUES ($1, $2, 'notify', 'system', 'processing', NOW() - INTERVAL '10 minutes', 'old-owner')",
        )
        .bind("channel:1492506767085801535")
        .bind("stale")
        .execute(&pg_pool)
        .await
        .unwrap();

        let claimed = claim_pending_message_outbox_batch_pg(&pg_pool, "test-owner").await;
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].2, "stale");

        let row = sqlx::query(
            "SELECT status, claim_owner
             FROM message_outbox
             WHERE id = $1",
        )
        .bind(claimed[0].0)
        .fetch_one(&pg_pool)
        .await
        .unwrap();
        let status: String = row.get("status");
        let claim_owner: Option<String> = row.get("claim_owner");
        assert_eq!(status, "processing");
        assert_eq!(claim_owner.as_deref(), Some("test-owner"));

        pg_pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn drain_message_outbox_batch_pg_marks_successful_rows_sent() {
        let _guard = server_test_lock();
        let db = test_db();
        let pg_db = TestPostgresDb::create().await;
        let pg_pool = pg_db.connect_and_migrate().await;
        let delivered = Arc::new(Mutex::new(Vec::new()));

        let message_id: i64 = sqlx::query_scalar(
            "INSERT INTO message_outbox (target, content, bot, source)
             VALUES ($1, $2, 'notify', 'system')
             RETURNING id",
        )
        .bind("channel:1492506767085801535")
        .bind("hello-pg")
        .fetch_one(&pg_pool)
        .await
        .unwrap();

        let processed = drain_message_outbox_batch_once(&pg_pool, Some("test-owner"), {
            let delivered = delivered.clone();
            move |target, content, source, bot| {
                let delivered = delivered.clone();
                async move {
                    delivered.lock().unwrap().push(json!({
                        "target": target,
                        "content": content,
                        "source": source,
                        "bot": bot,
                    }));
                    ("200 OK".to_string(), json!({"ok": true}).to_string())
                }
            }
        })
        .await;

        assert_eq!(processed, 1);
        let captured = delivered.lock().unwrap().clone();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0]["content"], "hello-pg");

        let row = sqlx::query(
            "SELECT status,
                    error,
                    sent_at IS NOT NULL AS has_sent_at,
                    claimed_at IS NULL AS claim_cleared,
                    claim_owner IS NULL AS owner_cleared
             FROM message_outbox
             WHERE id = $1",
        )
        .bind(message_id)
        .fetch_one(&pg_pool)
        .await
        .unwrap();
        let status: String = row.get("status");
        let error: Option<String> = row.get("error");
        let has_sent_at: bool = row.get("has_sent_at");
        let claim_cleared: bool = row.get("claim_cleared");
        let owner_cleared: bool = row.get("owner_cleared");

        assert_eq!(status, "sent");
        assert_eq!(error, None);
        assert!(has_sent_at);
        assert!(claim_cleared);
        assert!(owner_cleared);

        pg_pool.close().await;
        pg_db.drop().await;
    }

    /// #835 split-brain regression: when two machines share the same Postgres
    /// `agentdesk` DB, the policy-tick singleton lock (`POLICY_TICK_ADVISORY_LOCK_ID`
    /// = 7,801,001) must keep at most one tick loop active. The standby's
    /// `pg_try_advisory_lock` call must observe `false` while the active holder
    /// owns the lease, and must succeed once the holder releases (handover).
    ///
    /// Real two-machine verification is documented in
    /// `docs/multimachine-failover.md`; this test exercises the same code path
    /// against a single Postgres instance using two independent `PgPool`s, which
    /// is what each machine would use in the multimachine deployment.
    #[tokio::test]
    async fn policy_tick_advisory_lock_blocks_split_brain_across_pools() {
        let _guard = server_test_lock();
        let pg_db = TestPostgresDb::create().await;

        // Two independent pools simulate two machines (Mac Mini + Mac Book)
        // pointing at the same `agentdesk` Postgres instance.
        let active_pool = pg_db.connect_and_migrate().await;
        let standby_pool = pg_db.connect_and_migrate().await;

        let active_holder = try_acquire_pg_singleton_lock(
            &active_pool,
            POLICY_TICK_ADVISORY_LOCK_ID,
            "policy-tick",
        )
        .await
        .expect("active machine must be able to attempt the lock")
        .expect("active machine must acquire the policy-tick singleton lease");

        let standby_attempt = try_acquire_pg_singleton_lock(
            &standby_pool,
            POLICY_TICK_ADVISORY_LOCK_ID,
            "policy-tick",
        )
        .await
        .expect("standby probe must not error");
        assert!(
            standby_attempt.is_none(),
            "standby must be fenced while active holds the policy-tick lease"
        );

        // Active machine releases (e.g. dcserver shutdown / failover).
        release_pg_singleton_lock(active_holder, POLICY_TICK_ADVISORY_LOCK_ID, "policy-tick").await;

        // Standby should now be able to acquire — handover within one probe.
        let standby_holder = try_acquire_pg_singleton_lock(
            &standby_pool,
            POLICY_TICK_ADVISORY_LOCK_ID,
            "policy-tick",
        )
        .await
        .expect("standby probe after failover must not error")
        .expect("standby must acquire the policy-tick lease after handover");
        release_pg_singleton_lock(standby_holder, POLICY_TICK_ADVISORY_LOCK_ID, "policy-tick")
            .await;

        active_pool.close().await;
        standby_pool.close().await;
        pg_db.drop().await;
    }

    /// #835 split-brain regression for the github-sync singleton (lock_id
    /// `GITHUB_SYNC_ADVISORY_LOCK_ID` = 7,801,002). The github-sync lock is
    /// distinct from `POLICY_TICK_ADVISORY_LOCK_ID` (7,801,001); each must
    /// singleton independently across machines that share the `agentdesk` PG.
    #[tokio::test]
    async fn github_sync_advisory_lock_blocks_split_brain_across_pools() {
        let _guard = server_test_lock();
        let pg_db = TestPostgresDb::create().await;

        let active_pool = pg_db.connect_and_migrate().await;
        let standby_pool = pg_db.connect_and_migrate().await;

        let active_holder = try_acquire_pg_singleton_lock(
            &active_pool,
            GITHUB_SYNC_ADVISORY_LOCK_ID,
            "github-sync",
        )
        .await
        .expect("active probe for github-sync lock must not error")
        .expect("active machine must acquire github-sync lease");

        let standby_attempt = try_acquire_pg_singleton_lock(
            &standby_pool,
            GITHUB_SYNC_ADVISORY_LOCK_ID,
            "github-sync",
        )
        .await
        .expect("standby github-sync probe must not error");
        assert!(
            standby_attempt.is_none(),
            "standby must be fenced from github-sync while active holds it"
        );

        // The policy-tick singleton (7,801,001) must remain orthogonal to the
        // github-sync singleton (7,801,002) — verify standby can still acquire
        // policy-tick while github-sync is held by the active machine.
        let standby_policy = try_acquire_pg_singleton_lock(
            &standby_pool,
            POLICY_TICK_ADVISORY_LOCK_ID,
            "policy-tick",
        )
        .await
        .expect("standby policy-tick probe must not error")
        .expect("policy-tick must be acquirable while github-sync is held elsewhere");
        release_pg_singleton_lock(standby_policy, POLICY_TICK_ADVISORY_LOCK_ID, "policy-tick")
            .await;

        // Active machine releases github-sync — handover to standby.
        release_pg_singleton_lock(active_holder, GITHUB_SYNC_ADVISORY_LOCK_ID, "github-sync").await;

        let standby_holder = try_acquire_pg_singleton_lock(
            &standby_pool,
            GITHUB_SYNC_ADVISORY_LOCK_ID,
            "github-sync",
        )
        .await
        .expect("standby github-sync probe after release must not error")
        .expect("standby must acquire github-sync after active release");
        release_pg_singleton_lock(standby_holder, GITHUB_SYNC_ADVISORY_LOCK_ID, "github-sync")
            .await;

        active_pool.close().await;
        standby_pool.close().await;
        pg_db.drop().await;
    }
}

/// Fetch rate limits from the Anthropic API via the count_tokens endpoint (free, no token cost).
/// Parses `anthropic-ratelimit-*` response headers into bucket format.
async fn fetch_anthropic_rate_limits(
    api_key: &str,
) -> Result<Vec<serde_json::Value>, anyhow::Error> {
    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.anthropic.com/v1/messages/count_tokens")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&serde_json::json!({
            "model": "claude-haiku-4-5-20251001",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await?;

    let headers = resp.headers().clone();
    let mut buckets = Vec::new();

    // Parse requests bucket
    if let Some(limit) = parse_header_i64(&headers, "anthropic-ratelimit-requests-limit") {
        let remaining =
            parse_header_i64(&headers, "anthropic-ratelimit-requests-remaining").unwrap_or(limit);
        let reset = parse_header_reset(&headers, "anthropic-ratelimit-requests-reset");
        buckets.push(serde_json::json!({
            "name": "requests",
            "limit": limit,
            "used": limit - remaining,
            "remaining": remaining,
            "reset": reset,
        }));
    }

    // Parse tokens bucket
    if let Some(limit) = parse_header_i64(&headers, "anthropic-ratelimit-tokens-limit") {
        let remaining =
            parse_header_i64(&headers, "anthropic-ratelimit-tokens-remaining").unwrap_or(limit);
        let reset = parse_header_reset(&headers, "anthropic-ratelimit-tokens-reset");
        buckets.push(serde_json::json!({
            "name": "tokens",
            "limit": limit,
            "used": limit - remaining,
            "remaining": remaining,
            "reset": reset,
        }));
    }

    Ok(buckets)
}

/// Fetch rate limits from the OpenAI API via the models endpoint (free, read-only).
/// Parses `x-ratelimit-*` response headers into bucket format.
async fn fetch_openai_rate_limits(api_key: &str) -> Result<Vec<serde_json::Value>, anyhow::Error> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.openai.com/v1/models")
        .header("authorization", format!("Bearer {api_key}"))
        .send()
        .await?;

    let headers = resp.headers().clone();
    let mut buckets = Vec::new();

    // OpenAI rate limit headers: x-ratelimit-limit-requests, x-ratelimit-remaining-requests, etc.
    if let Some(limit) = parse_header_i64(&headers, "x-ratelimit-limit-requests") {
        let remaining =
            parse_header_i64(&headers, "x-ratelimit-remaining-requests").unwrap_or(limit);
        let reset = parse_header_reset(&headers, "x-ratelimit-reset-requests");
        buckets.push(serde_json::json!({
            "name": "requests",
            "limit": limit,
            "used": limit - remaining,
            "remaining": remaining,
            "reset": reset,
        }));
    }

    if let Some(limit) = parse_header_i64(&headers, "x-ratelimit-limit-tokens") {
        let remaining = parse_header_i64(&headers, "x-ratelimit-remaining-tokens").unwrap_or(limit);
        let reset = parse_header_reset(&headers, "x-ratelimit-reset-tokens");
        buckets.push(serde_json::json!({
            "name": "tokens",
            "limit": limit,
            "used": limit - remaining,
            "remaining": remaining,
            "reset": reset,
        }));
    }

    Ok(buckets)
}

fn parse_header_i64(headers: &reqwest::header::HeaderMap, name: &str) -> Option<i64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

/// Parse ISO 8601 reset timestamp from header into unix epoch seconds.
fn parse_header_reset(headers: &reqwest::header::HeaderMap, name: &str) -> i64 {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|dt| dt.timestamp())
        })
        .unwrap_or(0)
}

/// Read Claude Code OAuth token from macOS Keychain, falling back to ~/.claude/.credentials.json.
fn get_claude_oauth_token() -> Option<String> {
    // Try macOS Keychain first
    if let Ok(output) = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
    {
        if output.status.success() {
            if let Ok(raw) = String::from_utf8(output.stdout) {
                let raw = raw.trim();
                if let Ok(creds) = serde_json::from_str::<serde_json::Value>(raw) {
                    if let Some(token) = creds
                        .get("claudeAiOauth")
                        .and_then(|o| o.get("accessToken"))
                        .and_then(|v| v.as_str())
                    {
                        return Some(token.to_string());
                    }
                }
            }
        }
    }
    // Fallback: credentials file
    let home = dirs::home_dir()?;
    let cred_path = home.join(".claude").join(".credentials.json");
    let raw = std::fs::read_to_string(cred_path).ok()?;
    let creds: serde_json::Value = serde_json::from_str(&raw).ok()?;
    creds
        .get("claudeAiOauth")
        .and_then(|o| o.get("accessToken"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Fetch Claude usage via OAuth API (subscription-based, no API key needed).
/// Returns utilization-based buckets (5h, 7d).
async fn fetch_claude_oauth_usage(token: &str) -> Result<Vec<serde_json::Value>, anyhow::Error> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("user-agent", "agentdesk/1.0.0")
        .send()
        .await?;

    if resp.status() == 429 {
        return Err(anyhow::anyhow!("Claude OAuth usage API rate limited (429)"));
    }
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "Claude OAuth usage API returned {}",
            resp.status()
        ));
    }

    let data: serde_json::Value = resp.json().await?;
    let mut buckets = Vec::new();

    for key in &["five_hour", "seven_day", "seven_day_sonnet"] {
        if let Some(bucket) = data.get(key) {
            let utilization = bucket
                .get("utilization")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let resets_at = bucket
                .get("resets_at")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let label = match *key {
                "five_hour" => "5h",
                "seven_day" => "7d",
                "seven_day_sonnet" => "7d Sonnet",
                _ => key,
            };
            // Convert utilization (0-100 float) to used/limit format for consistency
            let limit = 100i64;
            let used = utilization.round() as i64;
            let reset_ts = chrono::DateTime::parse_from_rfc3339(resets_at)
                .map(|dt| dt.timestamp())
                .unwrap_or(0);

            buckets.push(serde_json::json!({
                "name": label,
                "limit": limit,
                "used": used,
                "remaining": limit - used,
                "reset": reset_ts,
            }));
        }
    }

    Ok(buckets)
}

/// Read Codex CLI access token from ~/.codex/auth.json.
fn load_codex_access_token() -> Option<String> {
    let home = dirs::home_dir()?;
    let auth_path = home.join(".codex").join("auth.json");
    let raw = std::fs::read_to_string(auth_path).ok()?;
    let auth: serde_json::Value = serde_json::from_str(&raw).ok()?;
    auth.get("tokens")
        .and_then(|t| t.get("access_token"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Fetch Codex usage via chatgpt.com backend API (subscription-based, no API key needed).
async fn fetch_codex_oauth_usage(token: &str) -> Result<Vec<serde_json::Value>, anyhow::Error> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://chatgpt.com/backend-api/codex/usage")
        .header("authorization", format!("Bearer {token}"))
        .header("user-agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .header("accept", "application/json")
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "Codex usage API returned {}",
            resp.status()
        ));
    }

    let data: serde_json::Value = resp.json().await?;
    let mut buckets = Vec::new();

    if let Some(rl) = data.get("rate_limit") {
        for window_key in &["primary_window", "secondary_window"] {
            if let Some(window) = rl.get(window_key) {
                let used_percent = window
                    .get("used_percent")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let window_seconds = window
                    .get("limit_window_seconds")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let reset_at = window.get("reset_at").and_then(|v| v.as_i64()).unwrap_or(0);

                let label = if window_seconds <= 18000 {
                    "5h"
                } else if window_seconds <= 86400 {
                    "1d"
                } else {
                    "7d"
                };

                let limit = 100i64;
                let used = used_percent.round() as i64;

                buckets.push(serde_json::json!({
                    "name": label,
                    "limit": limit,
                    "used": used,
                    "remaining": limit - used,
                    "reset": reset_at,
                }));
            }
        }
    }

    Ok(buckets)
}

// ── Gemini rate-limit helpers ─────────────────────────────────────────────────

/// Extract OAuth2 app credentials (client_id, client_secret) for the Gemini CLI
/// "installed app" flow.  These are public client credentials distributed inside
/// the Gemini CLI npm bundle — not server secrets.  The refresh_token in
/// ~/.gemini/oauth_creds.json is the actual per-user secret.
///
/// Resolution order:
///   1. env vars GEMINI_CLIENT_ID / GEMINI_CLIENT_SECRET
///   2. parse from the installed Gemini CLI bundle (e.g. Homebrew path)
fn load_gemini_oauth_app_creds() -> Result<(String, String), anyhow::Error> {
    // 1. Environment variables take precedence (CI / custom installs)
    if let (Ok(id), Ok(secret)) = (
        std::env::var("GEMINI_CLIENT_ID"),
        std::env::var("GEMINI_CLIENT_SECRET"),
    ) {
        return Ok((id, secret));
    }

    // 2. Parse from the Gemini CLI bundle on disk.
    //    Support both Homebrew Cellar installs and npm-global installs.
    let candidate_globs = [
        "/opt/homebrew/Cellar/gemini-cli/*/libexec/lib/node_modules/@google/gemini-cli/bundle/chunk-*.js",
        "/usr/local/Cellar/gemini-cli/*/libexec/lib/node_modules/@google/gemini-cli/bundle/chunk-*.js",
        "/opt/homebrew/lib/node_modules/@google/gemini-cli/bundle/chunk-*.js",
        "/usr/local/lib/node_modules/@google/gemini-cli/bundle/chunk-*.js",
        "/usr/lib/node_modules/@google/gemini-cli/bundle/chunk-*.js",
    ];

    for pattern in &candidate_globs {
        let Ok(paths) = glob::glob(pattern) else {
            continue;
        };
        for entry in paths.flatten() {
            let Ok(content) = std::fs::read_to_string(&entry) else {
                continue;
            };
            // Gemini CLI 0.38.x bundles export OAuth constants like:
            //   var OAUTH_CLIENT_ID = "<id>";
            //   var OAUTH_CLIENT_SECRET = "<secret>";
            // Older bundles also inline:
            //   clientId:"<id>",clientSecret:"<secret>"
            let id = extract_assigned_string(&content, "OAUTH_CLIENT_ID")
                .or_else(|| extract_quoted_value(&content, "clientId"));
            let secret = extract_assigned_string(&content, "OAUTH_CLIENT_SECRET")
                .or_else(|| extract_quoted_value(&content, "clientSecret"));
            if let (Some(id), Some(secret)) = (id, secret) {
                return Ok((id, secret));
            }
        }
    }

    Err(anyhow::anyhow!(
        "Gemini OAuth app credentials not found. \
         Set GEMINI_CLIENT_ID / GEMINI_CLIENT_SECRET env vars or install gemini-cli."
    ))
}

/// Extract the value of a key from a JS bundle snippet like `key:"value"`.
fn extract_quoted_value(src: &str, key: &str) -> Option<String> {
    // Match:  clientId:"<value>"  or  clientId:'<value>'
    let needle = format!("{key}:\"");
    if let Some(start) = src.find(&needle) {
        let rest = &src[start + needle.len()..];
        if let Some(end) = rest.find('"') {
            return Some(rest[..end].to_string());
        }
    }
    let needle_sq = format!("{key}:'");
    if let Some(start) = src.find(&needle_sq) {
        let rest = &src[start + needle_sq.len()..];
        if let Some(end) = rest.find('\'') {
            return Some(rest[..end].to_string());
        }
    }
    None
}

/// Extract the value of a JS assignment like `var KEY = "value";`.
fn extract_assigned_string(src: &str, key: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let needle = format!("{key} = {quote}");
        if let Some(start) = src.find(&needle) {
            let rest = &src[start + needle.len()..];
            if let Some(end) = rest.find(quote) {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

/// Read (and refresh if expired) the Gemini OAuth2 access token from
/// `~/.gemini/oauth_creds.json`.  Writes back the new token on refresh.
async fn load_gemini_access_token() -> Result<String, anyhow::Error> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let creds_path = home.join(".gemini").join("oauth_creds.json");
    let raw = std::fs::read_to_string(&creds_path)
        .map_err(|e| anyhow::anyhow!("cannot read ~/.gemini/oauth_creds.json: {e}"))?;
    let mut creds: serde_json::Value = serde_json::from_str(&raw)?;

    let now_ms = chrono::Utc::now().timestamp_millis();
    let expiry_ms = creds
        .get("expiry_date")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    // 30-second buffer to avoid using a token that expires mid-request
    if expiry_ms > now_ms + 30_000 {
        return creds
            .get("access_token")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("no access_token in oauth_creds.json"));
    }

    // Token expired — refresh via Google token endpoint
    let refresh_token = creds
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("no refresh_token in oauth_creds.json"))?
        .to_string();

    let (client_id, client_secret) = load_gemini_oauth_app_creds()?;

    let client = reqwest::Client::new();
    let resp = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("refresh_token", refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "Gemini token refresh returned {}",
            resp.status()
        ));
    }

    let new_data: serde_json::Value = resp.json().await?;
    let new_access_token = new_data
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("no access_token in refresh response"))?
        .to_string();
    let expires_in = new_data
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(3600);

    // Persist refreshed token so the next call doesn't need to refresh again
    creds["access_token"] = serde_json::json!(new_access_token.clone());
    creds["expiry_date"] = serde_json::json!(now_ms + expires_in * 1000);
    if let Ok(updated) = serde_json::to_string_pretty(&creds) {
        let _ = std::fs::write(&creds_path, updated);
    }

    Ok(new_access_token)
}

/// Discover the "Default Gemini Project" ID via the Cloud Resource Manager API.
/// The Gemini CLI creates this project automatically during OAuth setup.
async fn discover_gemini_project_id(token: &str) -> Result<String, anyhow::Error> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://cloudresourcemanager.googleapis.com/v1/projects?filter=name:Default%20Gemini%20Project")
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "Cloud Resource Manager returned {}",
            resp.status()
        ));
    }

    let data: serde_json::Value = resp.json().await?;
    data.get("projects")
        .and_then(|p| p.as_array())
        .and_then(|arr| arr.first())
        .and_then(|proj| proj.get("projectId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            anyhow::anyhow!("Default Gemini Project not found via Cloud Resource Manager")
        })
}

fn extract_gemini_quota_limits(data: &serde_json::Value) -> (i64, i64) {
    let mut rpm_limit: Option<i64> = None;
    let mut rpd_limit: Option<i64> = None;

    if let Some(metrics) = data.get("metrics").and_then(|m| m.as_array()) {
        for metric in metrics {
            let metric_name = metric.get("metric").and_then(|m| m.as_str()).unwrap_or("");
            if metric_name
                != "generativelanguage.googleapis.com/generate_content_free_tier_requests"
            {
                continue;
            }

            if let Some(limits) = metric.get("consumerQuotaLimits").and_then(|l| l.as_array()) {
                for limit in limits {
                    let unit = limit.get("unit").and_then(|u| u.as_str()).unwrap_or("");
                    let is_per_minute = unit.contains("/min/");
                    let is_per_day = unit.starts_with("1/d/");

                    if let Some(buckets) = limit.get("quotaBuckets").and_then(|b| b.as_array()) {
                        // Take the minimum positive limit across all model buckets —
                        // this reflects the tightest constraint a user is likely to hit.
                        let min_positive = buckets
                            .iter()
                            .filter_map(|b| {
                                b.get("effectiveLimit")
                                    .and_then(parse_gemini_effective_limit_value)
                            })
                            .filter(|&v| v > 0)
                            .min();

                        if let Some(min_val) = min_positive {
                            if is_per_minute {
                                rpm_limit =
                                    Some(rpm_limit.map_or(min_val, |current| current.min(min_val)));
                            } else if is_per_day {
                                rpd_limit =
                                    Some(rpd_limit.map_or(min_val, |current| current.min(min_val)));
                            }
                        }
                    }
                }
            }
        }
    }

    (rpm_limit.unwrap_or(15), rpd_limit.unwrap_or(1500))
}

fn parse_gemini_effective_limit_value(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse::<i64>().ok()))
}

fn build_gemini_rate_limit_buckets(rpm_limit: i64, rpd_limit: i64) -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "name": "rpm",
            "limit": rpm_limit,
            "used": 0_i64,
            "remaining": rpm_limit,
            "reset": 0_i64,
        }),
        serde_json::json!({
            "name": "rpd",
            "limit": rpd_limit,
            "used": 0_i64,
            "remaining": rpd_limit,
            "reset": 0_i64,
        }),
    ]
}

/// Fetch Gemini quota limits via the Google Cloud ServiceUsage API.
///
/// Returns RPM and RPD buckets sourced from `generate_content_free_tier_requests`
/// quota metrics. The API does not expose real-time usage counters, so the
/// returned buckets use non-negative placeholder usage (`used = 0`,
/// `remaining = limit`) to keep downstream UI math stable.
async fn fetch_gemini_rate_limits() -> Result<Vec<serde_json::Value>, anyhow::Error> {
    let token = load_gemini_access_token().await?;
    let project_id = discover_gemini_project_id(&token).await?;

    let client = reqwest::Client::new();
    let url = format!(
        "https://serviceusage.googleapis.com/v1beta1/projects/{}/services/\
         generativelanguage.googleapis.com/consumerQuotaMetrics\
         ?fields=metrics.metric,metrics.consumerQuotaLimits",
        project_id
    );
    let resp = client
        .get(&url)
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "Gemini ServiceUsage API returned {}",
            resp.status()
        ));
    }

    let data: serde_json::Value = resp.json().await?;
    let (rpm_limit, rpd_limit) = extract_gemini_quota_limits(&data);
    Ok(build_gemini_rate_limit_buckets(rpm_limit, rpd_limit))
}

/// Background task that periodically syncs GitHub issues for all registered repos.
async fn github_sync_loop(pg_pool: Arc<PgPool>, interval_minutes: u64) {
    use std::time::Duration;

    if !crate::github::gh_available() {
        tracing::warn!("[github-sync] gh CLI not available — periodic sync disabled");
        return;
    }

    tracing::info!(
        "[github-sync] Periodic sync enabled (every {} minutes)",
        interval_minutes
    );

    let interval = Duration::from_secs(interval_minutes * 60);

    loop {
        tokio::time::sleep(interval).await;
        let sync_start = std::time::Instant::now();

        let mut advisory_lock = match try_acquire_pg_singleton_lock(
            &pg_pool,
            GITHUB_SYNC_ADVISORY_LOCK_ID,
            "github-sync",
        )
        .await
        {
            Ok(Some(conn)) => Some(conn),
            Ok(None) => {
                tracing::debug!("[github-sync] skipped: advisory lock held elsewhere");
                continue;
            }
            Err(error) => {
                tracing::warn!("[github-sync] advisory lock failed: {error}");
                continue;
            }
        };

        tracing::debug!("[github-sync] Running periodic sync...");

        let repos = match crate::github::list_repos_pg(&pg_pool).await {
            Ok(repos) => repos,
            Err(error) => {
                tracing::error!("[github-sync] Failed to list repos from PG: {error}");
                record_periodic_job_execution_pg(
                    &pg_pool,
                    "github_sync",
                    "error",
                    sync_start.elapsed(),
                )
                .await;
                if let Some(conn) = advisory_lock.take() {
                    release_pg_singleton_lock(conn, GITHUB_SYNC_ADVISORY_LOCK_ID, "github-sync")
                        .await;
                }
                continue;
            }
        };

        let mut had_errors = false;
        for repo in &repos {
            if !repo.sync_enabled {
                continue;
            }

            let issues = match crate::github::sync::fetch_issues(&repo.id) {
                Ok(i) => i,
                Err(e) => {
                    tracing::warn!("[github-sync] Fetch failed for {}: {e}", repo.id);
                    had_errors = true;
                    continue;
                }
            };

            match crate::github::triage::triage_new_issues_pg(&pg_pool, &repo.id, &issues).await {
                Ok(count) if count > 0 => {
                    tracing::info!("[github-sync] Triaged {count} new issues for {}", repo.id);
                }
                Err(error) => {
                    tracing::warn!("[github-sync] Triage failed for {}: {error}", repo.id);
                    had_errors = true;
                }
                _ => {}
            }

            match crate::github::sync::sync_github_issues_for_repo_pg(&pg_pool, &repo.id, &issues)
                .await
            {
                Ok(result) => {
                    if result.closed_count > 0 || result.inconsistency_count > 0 {
                        tracing::info!(
                            "[github-sync] {}: closed={}, inconsistencies={}",
                            repo.id,
                            result.closed_count,
                            result.inconsistency_count
                        );
                    }
                }
                Err(error) => {
                    tracing::error!("[github-sync] Sync failed for {}: {error}", repo.id);
                    had_errors = true;
                }
            }
        }

        record_periodic_job_execution_pg(
            &pg_pool,
            "github_sync",
            if had_errors { "error" } else { "ok" },
            sync_start.elapsed(),
        )
        .await;

        if let Some(conn) = advisory_lock.take() {
            release_pg_singleton_lock(conn, GITHUB_SYNC_ADVISORY_LOCK_ID, "github-sync").await;
        }
    }
}

/// Recursively copy a directory tree. Returns the number of files copied.
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<usize> {
    std::fs::create_dir_all(dst)?;
    let mut count = 0;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if ty.is_dir() {
            count += copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)?;
            count += 1;
        }
    }
    Ok(count)
}

/// Async worker that drains the message_outbox table via the in-process Discord delivery path (#120).
/// Runs every 2 seconds, processes up to 10 messages per tick.
#[cfg(test)]
fn load_pending_message_outbox_batch_sqlite(
    legacy_db: &crate::db::Db,
) -> Vec<(i64, String, String, String, String)> {
    let conn = match legacy_db.lock() {
        Ok(conn) => conn,
        Err(_) => return Vec::new(),
    };
    let mut stmt = match conn.prepare(
        "SELECT id, target, content, bot, source FROM message_outbox \
         WHERE status = 'pending' ORDER BY id ASC LIMIT 10",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    stmt.query_map([], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
        ))
    })
    .ok()
    .map(|rows| rows.filter_map(|row| row.ok()).collect())
    .unwrap_or_default()
}

async fn claim_pending_message_outbox_batch_pg(
    pool: &PgPool,
    claim_owner: &str,
) -> Vec<(i64, String, String, String, String)> {
    let rows = match sqlx::query(
        "WITH claimed AS (
            SELECT id
              FROM message_outbox
             WHERE status = 'pending'
                OR (
                    status = 'processing'
                    AND (
                        claimed_at IS NULL
                        OR claimed_at <= NOW() - ($1::bigint * INTERVAL '1 second')
                    )
                )
             ORDER BY id ASC
             FOR UPDATE SKIP LOCKED
             LIMIT 10
        )
        UPDATE message_outbox mo
           SET status = 'processing',
               claimed_at = NOW(),
               claim_owner = $2,
               error = NULL
          FROM claimed
         WHERE mo.id = claimed.id
        RETURNING mo.id, mo.target, mo.content, mo.bot, mo.source",
    )
    .bind(MESSAGE_OUTBOX_CLAIM_STALE_SECS)
    .bind(claim_owner)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!("[outbox-pg] failed to claim message_outbox rows: {error}");
            return Vec::new();
        }
    };

    let mut claimed = rows
        .into_iter()
        .filter_map(|row| {
            Some((
                row.try_get::<i64, _>("id").ok()?,
                row.try_get::<String, _>("target").ok()?,
                row.try_get::<String, _>("content").ok()?,
                row.try_get::<String, _>("bot").ok()?,
                row.try_get::<String, _>("source").ok()?,
            ))
        })
        .collect::<Vec<_>>();
    claimed.sort_by_key(|row| row.0);
    claimed
}

async fn drain_message_outbox_batch_once<F, Fut>(
    pg_pool: &PgPool,
    claim_owner: Option<&str>,
    mut deliver: F,
) -> usize
where
    F: FnMut(String, String, String, String) -> Fut,
    Fut: std::future::Future<Output = (String, String)>,
{
    let pending =
        claim_pending_message_outbox_batch_pg(pg_pool, claim_owner.unwrap_or("message-outbox"))
            .await;
    if pending.is_empty() {
        return 0;
    }

    for (id, target, content, bot, source) in &pending {
        let (status, err_text) =
            deliver(target.clone(), content.clone(), source.clone(), bot.clone()).await;
        if status == "200 OK" {
            sqlx::query(
                "UPDATE message_outbox
                    SET status = 'sent',
                        sent_at = NOW(),
                        error = NULL,
                        claimed_at = NULL,
                        claim_owner = NULL
                  WHERE id = $1",
            )
            .bind(*id)
            .execute(pg_pool)
            .await
            .ok();
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::debug!("[{ts}] [outbox] ✅ delivered msg {id} → {target}");
        } else {
            let error_text = format!("{status}: {err_text}");
            sqlx::query(
                "UPDATE message_outbox
                    SET status = 'failed',
                        error = $1,
                        claimed_at = NULL,
                        claim_owner = NULL
                  WHERE id = $2",
            )
            .bind(error_text)
            .bind(*id)
            .execute(pg_pool)
            .await
            .ok();
            tracing::warn!("[outbox] ❌ msg {id} → {target} failed: {status}");
        }
    }

    pending.len()
}

#[cfg(test)]
async fn drain_message_outbox_batch_once_sqlite<F, Fut>(
    sqlite_db: &crate::db::Db,
    mut deliver: F,
) -> usize
where
    F: FnMut(String, String, String, String) -> Fut,
    Fut: std::future::Future<Output = (String, String)>,
{
    let pending = load_pending_message_outbox_batch_sqlite(sqlite_db);
    if pending.is_empty() {
        return 0;
    }

    for (id, target, content, bot, source) in &pending {
        let (status, err_text) =
            deliver(target.clone(), content.clone(), source.clone(), bot.clone()).await;
        if status == "200 OK" {
            if let Ok(conn) = sqlite_db.lock() {
                conn.execute(
                    "UPDATE message_outbox
                        SET status = 'sent',
                            sent_at = datetime('now'),
                            error = NULL,
                            claimed_at = NULL,
                            claim_owner = NULL
                      WHERE id = ?1",
                    [id],
                )
                .ok();
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::debug!("[{ts}] [outbox] ✅ delivered msg {id} → {target}");
        } else {
            let error_text = format!("{status}: {err_text}");
            if let Ok(conn) = sqlite_db.lock() {
                conn.execute(
                    "UPDATE message_outbox
                        SET status = 'failed',
                            error = ?1,
                            claimed_at = NULL,
                            claim_owner = NULL
                      WHERE id = ?2",
                    libsql_rusqlite::params![error_text, id],
                )
                .ok();
            }
            tracing::warn!("[outbox] ❌ msg {id} → {target} failed: {status}");
        }
    }

    pending.len()
}

async fn message_outbox_loop(pg_pool: Arc<PgPool>, health_registry: Option<Arc<HealthRegistry>>) {
    use std::time::Duration;

    let Some(health_registry) = health_registry else {
        tracing::error!("[outbox] Health registry unavailable; message outbox worker stopped");
        return;
    };

    // Give Discord runtime bootstrap a brief head start before polling.
    tokio::time::sleep(Duration::from_secs(3)).await;
    tracing::info!("[outbox] Message outbox worker started (adaptive backoff 500ms-5s)");
    let claim_owner = format!(
        "message-outbox:{}:{}",
        std::env::var("HOSTNAME").unwrap_or_else(|_| "local".to_string()),
        std::process::id()
    );

    let mut poll_interval = Duration::from_millis(500);
    let max_interval = Duration::from_secs(5);

    loop {
        tokio::time::sleep(poll_interval).await;
        if drain_message_outbox_batch_once(pg_pool.as_ref(), Some(&claim_owner), {
            let health_registry = health_registry.clone();
            let pg_pool = pg_pool.clone();
            move |target, content, source, bot| {
                let health_registry = health_registry.clone();
                let pg_pool = pg_pool.clone();
                async move {
                    let (status, err_text) =
                        crate::services::discord::health::send_message_with_backends(
                            &health_registry,
                            None,
                            Some(pg_pool.as_ref()),
                            &target,
                            &content,
                            &source,
                            &bot,
                            None,
                        )
                        .await;
                    (status.to_string(), err_text)
                }
            }
        })
        .await
            == 0
        {
            // No work: increase interval (up to max)
            poll_interval = (poll_interval.mul_f64(1.5)).min(max_interval);
            continue;
        }
        // Work found: reset to fast polling
        poll_interval = Duration::from_millis(500);
    }
}

async fn dm_reply_retry_loop(pg_pool: Arc<PgPool>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
    interval.tick().await; // skip immediate first tick
    loop {
        interval.tick().await;
        crate::services::discord::retry_failed_dm_notifications(None, Some(pg_pool.as_ref())).await;
    }
}
