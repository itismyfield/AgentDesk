pub(crate) mod cluster;
pub(crate) mod cluster_session_routing;
pub(crate) mod cron_catalog;
pub mod dto;
pub(crate) mod issue_specs;
pub(crate) mod maintenance;
pub(crate) mod multinode_regression;
mod outbox_actionable_delivery;
mod outbox_delivery_alert;
pub(crate) mod resource_locks;
pub mod routes;
mod startup_preflight;
pub(crate) mod task_dispatch_claims;
pub(crate) mod test_phase_runs;
mod worker_registry;
pub mod ws;

use std::sync::{Arc, OnceLock};

use anyhow::Result;
use axum::Router;
use axum::routing::get;
use serde::Serialize;
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
const MESSAGE_OUTBOX_MAX_RETRY_COUNT: i64 = 4;
const MESSAGE_OUTBOX_RETRY_BACKOFF_SECS: [i64; 4] = [60, 300, 900, 3600];
const POLICY_TICK_ADVISORY_LOCK_ID: i64 = 7_801_001;
const GITHUB_SYNC_ADVISORY_LOCK_ID: i64 = 7_801_002;
const POLICY_TICK_WARN_MS: u128 = 500;
const POLICY_TICK_HOOK_TIMEOUT: Duration = Duration::from_secs(5);
const CLAUDE_RATE_LIMIT_FORCED_REFRESH_TIMEOUT: Duration = Duration::from_secs(8);

/// Set once the rate-limit sync loop has emitted its first WARN about absent
/// Gemini OAuth credentials. When Gemini is simply not configured, the loop runs
/// every 2 minutes and would otherwise spam an identical WARN forever (#3566).
/// First miss logs at WARN; subsequent misses drop to DEBUG. Transient errors
/// (network/API) bypass this flag and keep WARNing every cycle.
static GEMINI_CREDS_MISSING_WARNED: AtomicBool = AtomicBool::new(false);

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

fn claude_rate_limit_refresh_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
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

async fn cleanup_stale_pending_queue_tmp_files_for_five_min_tick() {
    let result = tokio::task::spawn_blocking(
        crate::services::turn_orchestrator::cleanup_stale_pending_queue_tmp_files_all_tokens,
    )
    .await;
    match result {
        Ok(audits) => {
            let removed_stale = audits
                .iter()
                .filter(|audit| audit.action == "removed_stale")
                .count();
            let remove_failed = audits
                .iter()
                .filter(|audit| audit.action == "remove_failed")
                .count();
            let preserved_active = audits
                .iter()
                .filter(|audit| audit.action == "preserved_active")
                .count();
            if removed_stale > 0 || remove_failed > 0 {
                tracing::warn!(
                    scanned_tmp = audits.len(),
                    removed_stale,
                    remove_failed,
                    preserved_active,
                    "[policy-tick] pending_queue tmp cleanup completed"
                );
            } else if preserved_active > 0 {
                tracing::debug!(
                    scanned_tmp = audits.len(),
                    preserved_active,
                    "[policy-tick] pending_queue tmp cleanup found only active tmp writes"
                );
            }
        }
        Err(error) => {
            tracing::warn!("[policy-tick] pending_queue tmp cleanup task failed: {error}");
        }
    }
}

fn is_five_min_policy_tick(count: u64) -> bool {
    count != 0 && count % FIVE_MIN_POLICY_TICK_INTERVAL == 0
}

fn should_run_slo_api_friction_aggregation(count: u64, leader_epoch_pending: bool) -> bool {
    leader_epoch_pending || is_five_min_policy_tick(count)
}

async fn run_slo_api_friction_aggregation_tick(
    engine: &PolicyEngine,
    pg_pool: Option<&PgPool>,
    reason: &str,
) {
    let pool = pg_pool.or_else(|| engine.pg_pool());
    tracing::debug!(reason, "[policy-tick] running SLO/api-friction aggregation");

    if let Err(error) =
        crate::services::api_friction::process_api_friction_patterns(pool, None, None).await
    {
        tracing::warn!(
            reason,
            "[policy-tick] api-friction aggregation failed: {error}"
        );
    }

    // #1072 turn-lifecycle SLO aggregation (Epic #905 Phase 1):
    // compute + persist + alert on threshold breach.
    let now_ms = chrono::Utc::now().timestamp_millis();
    let aggregates = crate::services::slo::run_aggregation_tick(pool, now_ms).await;
    tracing::debug!(
        reason,
        aggregate_count = aggregates.len(),
        "[policy-tick] SLO aggregation completed"
    );
}

async fn run_leader_epoch_slo_api_friction_kickstart(
    engine: &PolicyEngine,
    pg_pool: Option<&PgPool>,
) -> bool {
    let pool = pg_pool.or_else(|| engine.pg_pool());
    let advisory_lock = if let Some(pool) = pool {
        match try_acquire_pg_singleton_lock(
            pool,
            POLICY_TICK_ADVISORY_LOCK_ID,
            "policy-tick-leader-epoch",
        )
        .await
        {
            Ok(Some(conn)) => Some(conn),
            Ok(None) => {
                tracing::debug!(
                    "[policy-tick] leader-epoch SLO/API-friction kickstart skipped: advisory lock held elsewhere"
                );
                return false;
            }
            Err(error) => {
                tracing::warn!("[policy-tick] leader-epoch advisory lock failed: {error}");
                return false;
            }
        }
    } else {
        None
    };

    run_slo_api_friction_aggregation_tick(engine, pool, "leader_epoch").await;

    if let Some(conn) = advisory_lock {
        release_pg_singleton_lock(
            conn,
            POLICY_TICK_ADVISORY_LOCK_ID,
            "policy-tick-leader-epoch",
        )
        .await;
    }

    true
}

pub(crate) async fn run(
    config: Config,
    engine: PolicyEngine,
    health_registry: Option<Arc<HealthRegistry>>,
    pg_pool: Option<PgPool>,
) -> Result<()> {
    crate::services::dispatches::wait_queue::set_runtime_cluster_config(config.cluster.clone());
    // Publish the boot config as the shared live snapshot and (when enabled)
    // start the config-file watcher so hot-swappable settings reload without a
    // restart, mirroring the policies watcher. The guard is held for the
    // lifetime of `run`; dropping it on shutdown joins the watcher thread.
    crate::config_live_reload::install(config.clone());
    let _config_hot_reload_guard = crate::config_live_reload::start(
        crate::config::resolved_config_path(),
        config.config_hot_reload,
    );
    let pg_pool = match pg_pool {
        // Callers that provide a runtime pool have already completed the
        // startup migrate/config-audit/reseed sequence under the startup lock.
        Some(pool) => Some(pool),
        None => {
            let pool = crate::db::postgres::connect(&config)
                .await
                .map_err(anyhow::Error::msg)?;
            if let Some(pool_ref) = pool.as_ref() {
                crate::db::postgres::with_startup_advisory_lock(pool_ref, || async {
                    crate::db::postgres::migrate(pool_ref).await?;
                    crate::db::postgres::startup_reseed_with_warmup_pool(pool_ref, &config).await
                })
                .await
                .map_err(anyhow::Error::msg)?;
            }
            pool
        }
    };
    if pg_pool.is_none() {
        anyhow::bail!("PostgreSQL is required for AgentDesk server runtime");
    }
    let startup_pg_pool = if pg_pool.is_some() {
        match crate::db::postgres::connect_for_startup(&config).await {
            Ok(pool) => pool,
            Err(error) => {
                tracing::warn!(
                    "[startup] postgres warmup pool unavailable for boot reconcile; falling back to runtime pool: {error}"
                );
                None
            }
        }
    } else {
        None
    };
    if let Some(pool) = pg_pool.as_ref() {
        // #1309: publish the runtime PG pool so cancel-tombstone helpers
        // called from contexts without a SharedData / PgPool argument
        // (e.g. `turn_lifecycle::stop_turn_with_policy`) can still mirror
        // cancel tombstones to the durable store across dcserver restarts.
        crate::db::cancel_tombstones::set_global_pool(pool.clone());
    }
    crate::services::observability::init_observability(pg_pool.clone());
    let _claude_tui_hook_endpoint =
        if crate::services::provider_hosting::any_requested_tui_hosting_driver_available(&config) {
            let endpoint = config.server.local_base_url();
            tracing::info!(
                endpoint,
                "claude_tui hook receiver published on dcserver HTTP port"
            );
            Some(crate::services::claude_tui::hook_server::publish_hook_endpoint(endpoint))
        } else {
            None
        };

    startup_preflight::run();
    let cluster_runtime = cluster::bootstrap(&config, pg_pool.clone()).await;
    let cluster_instance_id = cluster_runtime.instance_id().to_string();
    if let Some(pool) = pg_pool.clone() {
        crate::services::dispatch_watchdog::spawn(pool);
    }
    // #3557 (A): long-turn cluster probe — pages out when a burst of >10m turns
    // finishes inside one window (the stall watchdog's blind spot for
    // delegated_to_watcher turns, which stay desynced=false).
    if let Some(pool) = pg_pool.clone() {
        crate::services::long_turn_watchdog::spawn(pool);
    }
    crate::pipeline::refresh_override_health_report(pg_pool.as_ref()).await;
    let boot_reconcile_engine = match startup_pg_pool.as_ref() {
        Some(pool) => Some(crate::engine::PolicyEngine::new_with_pg(
            &config,
            Some(pool.clone()),
        )?),
        None => None,
    };
    let startup_pool = startup_pg_pool.as_ref().or(pg_pool.as_ref());
    crate::reconcile::reconcile_boot_runtime(
        boot_reconcile_engine.as_ref().unwrap_or(&engine),
        startup_pool,
        &cluster_instance_id,
    )
    .await?;
    drop(boot_reconcile_engine);
    drop(startup_pg_pool);

    let mut worker_registry = worker_registry::SupervisedWorkerRegistry::new(
        config.clone(),
        engine.clone(),
        health_registry.clone(),
        pg_pool.clone().map(Arc::new),
        cluster_runtime,
    );
    worker_registry.run_boot_only_steps().await?;
    worker_registry.start_after_boot_reconcile()?;
    routes::receipt::spawn_token_analytics_cache_prewarm();

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
    let control_plane_auth_state = routes::AppState {
        pg_pool: pg_pool.clone(),
        engine: engine.clone(),
        config: Arc::new(config.clone()),
        broadcast_tx: broadcast_tx.clone(),
        batch_buffer: batch_buffer.clone(),
        health_registry: health_registry.clone(),
        cluster_instance_id: Some(cluster_instance_id.clone()),
    };

    let mut app = Router::new()
        .route("/ws", get(ws::ws_handler).with_state(broadcast_tx.clone()))
        .nest(
            "/api",
            routes::api_router_with_pg_and_cluster(
                engine.clone(),
                config.clone(),
                broadcast_tx.clone(),
                batch_buffer,
                health_registry,
                pg_pool,
                Some(cluster_instance_id),
            ),
        );
    if _claude_tui_hook_endpoint.is_some() {
        app = app.merge(
            crate::services::claude_tui::hook_server::hook_receiver_router().layer(
                axum::middleware::from_fn_with_state(
                    control_plane_auth_state.clone(),
                    routes::auth::auth_middleware,
                ),
            ),
        );
    }
    // `tui_relay` exposes the `claude_tui_send` / `claude_tui_wait` MCP
    // primitives (see audit issue #2652). It is always mounted because the
    // event-driven wait path is useful even when the hook receiver
    // endpoint has not been published yet (e.g. early-boot Codex calls).
    app = app.merge(crate::services::claude_tui::tui_relay::router().layer(
        axum::middleware::from_fn_with_state(
            control_plane_auth_state,
            routes::auth::auth_middleware,
        ),
    ));
    let app = app.fallback_service(dashboard_service);

    // #3870 — fail closed on the dangerous combination of a non-loopback bind
    // host with no `server.auth_token`. The control-plane auth middleware is
    // fail-open when no token is set, so exposing it on the LAN would hand the
    // entire mutating control-plane (deploy gate, agent CRUD, dispatch create)
    // to any LAN peer. Force the bind to loopback instead of refusing to boot,
    // so the server still serves locally — graceful degradation, not a brick.
    let (bind_host, bind_decision) = routes::resolve_secure_bind_host(&config);
    if let routes::BindSecurityDecision::ForcedLoopback { requested_host } = &bind_decision {
        tracing::error!(
            requested_host = %requested_host,
            forced_host = %bind_host,
            port = config.server.port,
            "SECURITY (#3870): server.host={requested_host} is non-loopback and \
             server.auth_token is unset — the control-plane auth middleware is fail-open, so \
             this would expose deploy-gate / agent-CRUD / dispatch endpoints to the LAN with no \
             auth. Force-binding to loopback ({bind_host}) instead. The server still serves \
             locally. To expose on the LAN intentionally, set server.auth_token (recommended) \
             or server.allow_insecure_nonloopback_bind=true."
        );
    }
    let addr = format!("{}:{}", bind_host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("HTTP server listening on {addr}");
    routes::audit_explicit_auth_routes_on_boot(&config);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// Background task that fires tiered OnTick hooks at different intervals (#127).
///
/// 3 tiers to prevent slow sections from blocking time-critical recovery:
/// - OnTick30s (30s): retry, unsent notification recovery, deadlock detection [I], orphan recovery [K]
/// - OnTick1min (1m): non-critical timeouts [A][C][D][E][L], stale detection
/// - OnTick5min (5m): non-critical reconciliation [R][B][F][G][H][M][O], idle session cleanup
/// - OnTick (legacy, 5m): backward compat for policies that only register onTick
async fn policy_tick_loop(
    engine: PolicyEngine,
    pg_pool: Option<Arc<PgPool>>,
    cluster_runtime: Option<cluster::ClusterRuntime>,
    shutdown: Option<Arc<AtomicBool>>,
) {
    tracing::info!("[policy-tick] 3-tier tick started: 30s / 1min / 5min");

    let mut count = 0u64;
    let mut leader_epoch_slo_api_friction_pending = true;

    if shutdown
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::Acquire))
    {
        tracing::info!("[policy-tick] shutdown requested before leader-epoch kickstart");
        return;
    }

    if let Some(runtime) = cluster_runtime.as_ref()
        && !runtime.is_leader()
    {
        tracing::warn!(
            instance_id = runtime.instance_id(),
            "[policy-tick] self-fenced before leader-epoch kickstart after cluster leadership was lost"
        );
        return;
    }

    if should_run_slo_api_friction_aggregation(count, leader_epoch_slo_api_friction_pending)
        && run_leader_epoch_slo_api_friction_kickstart(&engine, pg_pool.as_deref()).await
    {
        leader_epoch_slo_api_friction_pending = false;
    }

    let mut interval_30s = tokio::time::interval(Duration::from_secs(30));

    // Skip the first immediate tick
    interval_30s.tick().await;

    loop {
        interval_30s.tick().await;
        count += 1;

        if shutdown
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            tracing::info!("[policy-tick] shutdown requested");
            break;
        }

        if let Some(runtime) = cluster_runtime.as_ref()
            && !runtime.is_leader()
        {
            tracing::warn!(
                instance_id = runtime.instance_id(),
                "[policy-tick] self-fenced after cluster leadership was lost"
            );
            break;
        }

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

        let ran_slo_api_friction_this_tick = if should_run_slo_api_friction_aggregation(
            count,
            leader_epoch_slo_api_friction_pending,
        ) {
            let reason = if leader_epoch_slo_api_friction_pending {
                "leader_epoch_retry"
            } else {
                "five_min_tick"
            };
            run_slo_api_friction_aggregation_tick(&engine, pg_pool.as_deref(), reason).await;
            leader_epoch_slo_api_friction_pending = false;
            true
        } else {
            false
        };

        // ── 30s tier: every tick ── (#134: fire by name for dynamic hook binding)
        fire_tick_hook_by_name_with_pg(&engine, pg_pool.as_deref(), "OnTick30s", "30s").await;

        // ── 1min tier: every 2nd tick (60s) ──
        if count % 2 == 0 {
            fire_tick_hook_by_name_with_pg(&engine, pg_pool.as_deref(), "OnTick1min", "1min").await;
            if let Some(pool) = pg_pool.as_deref().or_else(|| engine.pg_pool()) {
                match crate::reconcile::reconcile_auto_queue_pending_delivery_orphans_pg(pool).await
                {
                    Ok(stats) if stats.touched() => {
                        tracing::info!(
                            candidates = stats.candidates,
                            requeued_notify = stats.requeued_notify,
                            skipped = stats.skipped,
                            "[policy-tick] auto-queue pending delivery orphan reconcile repaired notify rows"
                        );
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(
                            "[policy-tick] auto-queue pending delivery orphan reconcile failed: {error}"
                        );
                    }
                }
            }
        }

        // ── 5min tier: every 10th tick (300s) ──
        if is_five_min_policy_tick(count) {
            fire_tick_hook_by_name_with_pg(&engine, pg_pool.as_deref(), "OnTick5min", "5min").await;
            refresh_memory_health_for_five_min_tick().await;
            cleanup_stale_pending_queue_tmp_files_for_five_min_tick().await;
            // #2257 concern 5: sweep expired idempotency_keys rows so the
            // table stays bounded. The endpoint defaults are 24h TTL; one
            // 5-min sweep is plenty even under heavy use.
            if let Some(pool) = pg_pool.as_deref().or_else(|| engine.pg_pool()) {
                match crate::db::idempotency::gc_expired(pool).await {
                    Ok(0) => {}
                    Ok(deleted) => {
                        tracing::info!(
                            deleted,
                            "[policy-tick] idempotency_keys GC swept expired rows"
                        );
                    }
                    Err(error) => {
                        tracing::warn!(
                            error = %error,
                            "[policy-tick] idempotency_keys GC failed"
                        );
                    }
                }
            }
            if !ran_slo_api_friction_this_tick {
                run_slo_api_friction_aggregation_tick(&engine, pg_pool.as_deref(), "five_min_tick")
                    .await;
            }
            let slo_pool = pg_pool.as_deref().or_else(|| engine.pg_pool());
            if let Some(pool) = slo_pool {
                match crate::reconcile::reconcile_completed_queue_review_drift_pg(pool, &engine)
                    .await
                {
                    Ok(recovered) if recovered > 0 => {
                        tracing::info!(
                            recovered,
                            "[policy-tick] completed queue review drift recovered cards"
                        );
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(
                            "[policy-tick] completed queue review drift reconcile failed: {error}"
                        );
                    }
                }

                match crate::reconcile::reconcile_dispatch_delivery_events_pg(pool).await {
                    Ok(stats) if stats.touched() => {
                        tracing::warn!(
                            mismatch_count = stats.mismatch_count,
                            missing_typed = stats.missing_typed,
                            notified_status_mismatch = stats.notified_status_mismatch,
                            missing_kv_meta = stats.missing_kv_meta,
                            "[policy-tick] dispatch delivery event reconcile found mismatches"
                        );
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(
                            "[policy-tick] dispatch delivery event reconcile failed: {error}"
                        );
                    }
                }
            }

            // Also fire legacy OnTick for backward compat
            fire_tick_hook_by_name_with_pg(&engine, pg_pool.as_deref(), "OnTick", "legacy").await;
        }

        if let Some(conn) = advisory_lock {
            release_pg_singleton_lock(conn, POLICY_TICK_ADVISORY_LOCK_ID, "policy-tick").await;
        }
    }
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
    crate::kanban::drain_hook_side_effects_with_backends(engine);
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

        let _ = sync_claude_rate_limit_cache_once_serialized(pg_pool.as_ref()).await;

        // --- Codex rate limits ---
        // Priority: 1) ~/.codex/auth.json (Codex CLI subscription), 2) OPENAI_API_KEY
        let codex_result = if let Some(token) = crate::services::provider_auth::codex_access_token()
        {
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
                let msg = e.to_string();
                // Only suppress the genuine "not configured / file missing" case,
                // classified at the source (provider_auth) by `io::ErrorKind`:
                //   - "no home dir"            (no $HOME)
                //   - NotFound                 (oauth_creds.json does not exist)
                // PermissionDenied / IsADirectory / transient I/O are tagged
                // differently and corrupt/partial creds ("no access_token" /
                // "no refresh_token") are separate problems — all keep WARNing,
                // so we deliberately do NOT match on "oauth_creds.json" broadly
                // here (#3566 over-suppress fix, codex r2).
                let creds_missing =
                    crate::services::provider_auth::is_gemini_unconfigured_error(&e);
                if creds_missing {
                    // Gemini simply isn't configured — log once, then drop to DEBUG
                    // so the 2-minute sync loop doesn't spam an identical WARN (#3566).
                    if !GEMINI_CREDS_MISSING_WARNED.swap(true, Ordering::AcqRel) {
                        tracing::warn!(
                            "[rate-limit-sync] Gemini credentials not configured ({msg}); suppressing further repeats"
                        );
                    } else {
                        tracing::debug!(
                            "[rate-limit-sync] Gemini credentials absent; skipping (suppressed)"
                        );
                    }
                } else {
                    // Transient errors (network/API/token refresh) and corrupt/partial
                    // credentials keep WARNing.
                    tracing::warn!("[rate-limit-sync] Gemini rate_limit fetch failed: {e}");
                }
            }
        }

        // feature: rate-limit-aware-dispatch-gate — refresh the process-wide
        // in-memory pressure + agent→provider snapshots that the auto-queue
        // dispatch gate reads O(1) off the hot path (no DB on dispatch).
        refresh_dispatch_gate_snapshots_serialized(pg_pool.as_ref()).await;
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaudeRateLimitRefreshOutcome {
    pub triggered: bool,
    pub dispatch_gate_refreshed: bool,
    pub refreshed_at: Option<i64>,
    pub reason: Option<&'static str>,
    pub error: Option<String>,
}

impl ClaudeRateLimitRefreshOutcome {
    fn scheduled() -> Self {
        Self {
            triggered: true,
            dispatch_gate_refreshed: false,
            refreshed_at: None,
            reason: Some("refresh_scheduled"),
            error: None,
        }
    }

    fn skipped(reason: &'static str) -> Self {
        Self {
            triggered: false,
            dispatch_gate_refreshed: false,
            refreshed_at: None,
            reason: Some(reason),
            error: None,
        }
    }

    fn failed(error: anyhow::Error) -> Self {
        Self {
            triggered: false,
            dispatch_gate_refreshed: false,
            refreshed_at: None,
            reason: Some("sync_failed"),
            error: Some(error.to_string()),
        }
    }
}

pub(crate) fn spawn_claude_rate_limit_refresh_if_leader(
    pg_pool: PgPool,
) -> ClaudeRateLimitRefreshOutcome {
    if !worker_registry::rate_limit_sync_active() {
        return ClaudeRateLimitRefreshOutcome::skipped("rate_limit_sync_not_active_on_this_node");
    }

    tokio::spawn(async move {
        match tokio::time::timeout(
            CLAUDE_RATE_LIMIT_FORCED_REFRESH_TIMEOUT,
            trigger_claude_rate_limit_refresh_if_leader(&pg_pool),
        )
        .await
        {
            Ok(outcome) if outcome.error.is_none() => {
                tracing::info!(
                    triggered = outcome.triggered,
                    dispatch_gate_refreshed = outcome.dispatch_gate_refreshed,
                    refreshed_at = ?outcome.refreshed_at,
                    reason = ?outcome.reason,
                    "[rate-limit-sync] dashboard-triggered Claude refresh completed"
                );
            }
            Ok(outcome) => {
                tracing::warn!(
                    reason = ?outcome.reason,
                    error = ?outcome.error,
                    "[rate-limit-sync] dashboard-triggered Claude refresh failed"
                );
            }
            Err(_) => {
                tracing::warn!(
                    timeout_secs = CLAUDE_RATE_LIMIT_FORCED_REFRESH_TIMEOUT.as_secs(),
                    "[rate-limit-sync] dashboard-triggered Claude refresh timed out"
                );
            }
        }
    });

    ClaudeRateLimitRefreshOutcome::scheduled()
}

pub(crate) async fn trigger_claude_rate_limit_refresh_if_leader(
    pg_pool: &PgPool,
) -> ClaudeRateLimitRefreshOutcome {
    if !worker_registry::rate_limit_sync_active() {
        return ClaudeRateLimitRefreshOutcome::skipped("rate_limit_sync_not_active_on_this_node");
    }

    match sync_claude_rate_limit_cache_once_and_refresh_dispatch_gate_serialized(pg_pool).await {
        Ok(_) => ClaudeRateLimitRefreshOutcome {
            triggered: true,
            dispatch_gate_refreshed: true,
            refreshed_at: Some(chrono::Utc::now().timestamp()),
            reason: None,
            error: None,
        },
        Err(error) => ClaudeRateLimitRefreshOutcome::failed(error),
    }
}

async fn sync_claude_rate_limit_cache_once_serialized(
    pg_pool: &PgPool,
) -> Result<usize, anyhow::Error> {
    let _guard = claude_rate_limit_refresh_lock().lock().await;
    sync_claude_rate_limit_cache_once(pg_pool).await
}

async fn sync_claude_rate_limit_cache_once_and_refresh_dispatch_gate_serialized(
    pg_pool: &PgPool,
) -> Result<usize, anyhow::Error> {
    let _guard = claude_rate_limit_refresh_lock().lock().await;
    let bucket_count = sync_claude_rate_limit_cache_once(pg_pool).await?;
    refresh_dispatch_gate_snapshots(pg_pool).await;
    Ok(bucket_count)
}

async fn refresh_dispatch_gate_snapshots_serialized(pg_pool: &PgPool) {
    let _guard = claude_rate_limit_refresh_lock().lock().await;
    refresh_dispatch_gate_snapshots(pg_pool).await;
}

async fn sync_claude_rate_limit_cache_once(pg_pool: &PgPool) -> Result<usize, anyhow::Error> {
    // Priority: 1) OAuth token (Claude Code subscription), 2) ANTHROPIC_API_KEY.
    let claude_result =
        if let Some(token) = crate::services::provider_auth::claude_oauth_token_blocking().await {
            fetch_claude_oauth_usage(&token).await
        } else if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
            fetch_anthropic_rate_limits(&api_key).await
        } else {
            Err(anyhow::anyhow!("no Claude credentials found"))
        };

    match claude_result {
        Ok(buckets) => {
            let bucket_count = buckets.len();
            let data = serde_json::json!({ "buckets": buckets }).to_string();
            let now = chrono::Utc::now().timestamp();
            upsert_rate_limit_cache_entry(pg_pool, "claude", &data, now).await;
            tracing::info!("[rate-limit-sync] Claude: {} buckets cached", bucket_count);
            Ok(bucket_count)
        }
        Err(e) => {
            tracing::warn!("[rate-limit-sync] Claude rate_limit fetch failed: {e}");
            Err(e)
        }
    }
}

/// Rebuild the in-memory snapshots consumed by
/// `crate::services::dispatch_gate` from the freshly-synced `rate_limit_cache`
/// and the current agent/channel bindings. Runs off the hot dispatch path (once
/// per ~120s rate-limit tick), so any DB cost here never touches activation.
///
/// Note: `RateLimitSync` is leader-only, so this leader-side refresh only keeps
/// the leader's snapshots warm. Non-leader serving nodes refresh their own
/// process-local snapshots lazily from the shared DB cache on the activation
/// path (`dispatch_gate::refresh_snapshots_if_stale`), so the gate is populated
/// on every node — not silently a no-op on followers.
async fn refresh_dispatch_gate_snapshots(pg_pool: &PgPool) {
    let now = chrono::Utc::now().timestamp();
    crate::services::dispatch_gate::refresh_snapshots_from_db(pg_pool, now).await;
    // Record the refresh so the activation-path throttle treats the leader's
    // snapshots as fresh and does not redundantly re-read the cache here.
    crate::services::dispatch_gate::mark_snapshots_refreshed(now);
}

#[cfg(test)]
mod policy_tick_schedule_tests {
    use super::{is_five_min_policy_tick, should_run_slo_api_friction_aggregation};

    #[test]
    fn slo_api_friction_aggregation_runs_while_leader_epoch_is_pending() {
        assert!(should_run_slo_api_friction_aggregation(0, true));
        assert!(should_run_slo_api_friction_aggregation(1, true));
        assert!(should_run_slo_api_friction_aggregation(9, true));
        assert!(should_run_slo_api_friction_aggregation(10, true));
    }

    #[test]
    fn slo_api_friction_aggregation_falls_back_to_five_min_after_kickstart() {
        assert!(!should_run_slo_api_friction_aggregation(0, false));
        assert!(!should_run_slo_api_friction_aggregation(1, false));
        assert!(!should_run_slo_api_friction_aggregation(9, false));
        assert!(should_run_slo_api_friction_aggregation(10, false));
        assert!(should_run_slo_api_friction_aggregation(20, false));
    }

    #[test]
    fn five_min_policy_tick_runs_on_every_tenth_nonzero_iteration() {
        assert!(!is_five_min_policy_tick(0));
        assert!(!is_five_min_policy_tick(1));
        assert!(!is_five_min_policy_tick(9));
        assert!(is_five_min_policy_tick(10));
        assert!(is_five_min_policy_tick(20));
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

/// Parse reset timestamp from a rate-limit header into unix epoch seconds.
///
/// Anthropic returns RFC3339 timestamps. OpenAI commonly returns relative
/// durations such as `1s` or `6m0s`, so accept both forms.
fn parse_header_reset(headers: &reqwest::header::HeaderMap, name: &str) -> i64 {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| parse_rate_limit_reset_value(s, chrono::Utc::now().timestamp()))
        .unwrap_or(0)
}

fn parse_rate_limit_reset_value(value: &str, now: i64) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.timestamp())
        .or_else(|| parse_rate_limit_duration_seconds(value).map(|seconds| now + seconds))
}

fn parse_rate_limit_duration_seconds(value: &str) -> Option<i64> {
    let input = value.trim();
    if input.is_empty() {
        return None;
    }
    if let Ok(seconds) = input.parse::<i64>() {
        return Some(seconds.max(0));
    }

    let mut total = 0.0_f64;
    let mut number = String::new();
    let mut consumed = false;
    let chars = input.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if ch.is_ascii_digit() || ch == '.' {
            number.push(ch);
            index += 1;
            continue;
        }
        if number.is_empty() {
            return None;
        }
        let amount = number.parse::<f64>().ok()?;
        number.clear();
        let multiplier = match ch {
            'd' => 86_400.0,
            'h' => 3_600.0,
            'm' => {
                if chars.get(index + 1) == Some(&'s') {
                    index += 1;
                    0.001
                } else {
                    60.0
                }
            }
            's' => 1.0,
            _ => return None,
        };
        total += amount * multiplier;
        consumed = true;
        index += 1;
    }

    if !number.is_empty() || !consumed {
        return None;
    }
    Some(total.ceil().max(0.0) as i64)
}

#[cfg(test)]
mod rate_limit_header_tests {
    use super::{parse_rate_limit_duration_seconds, parse_rate_limit_reset_value};

    #[test]
    fn parses_openai_relative_reset_duration() {
        assert_eq!(parse_rate_limit_duration_seconds("6m0s"), Some(360));
        assert_eq!(parse_rate_limit_duration_seconds("1s"), Some(1));
        assert_eq!(parse_rate_limit_reset_value("500ms", 1_000), Some(1_001));
    }

    #[test]
    fn parses_rfc3339_reset_timestamp() {
        assert_eq!(
            parse_rate_limit_reset_value("2026-06-17T00:00:00Z", 1_000),
            Some(1_781_654_400)
        );
    }
}

/// Fetch Claude usage via OAuth API (subscription-based, no API key needed).
/// Returns utilization-based buckets (5h, 7d).
async fn fetch_claude_oauth_usage(token: &str) -> Result<Vec<serde_json::Value>, anyhow::Error> {
    let client = reqwest::Client::builder()
        .timeout(CLAUDE_RATE_LIMIT_FORCED_REFRESH_TIMEOUT)
        .build()?;
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
            // Convert utilization (0-100 float) to used/limit format for
            // consistency, but keep the precise value so the dispatch gate does
            // not round 99.5% into a false 100% saturation.
            let limit = 100i64;
            let used = utilization.floor().clamp(0.0, 100.0) as i64;
            let reset_ts = chrono::DateTime::parse_from_rfc3339(resets_at)
                .map(|dt| dt.timestamp())
                .unwrap_or(0);

            buckets.push(serde_json::json!({
                "name": label,
                "limit": limit,
                "used": used,
                "remaining": limit - used,
                "utilization": utilization,
                "reset": reset_ts,
            }));
        }
    }

    Ok(buckets)
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
    let (creds_path, mut creds) = crate::services::provider_auth::read_gemini_oauth_creds()?;

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
                    if result.closed_count > 0
                        || result.inconsistency_count > 0
                        || result.stale_card_issue_check_count > 0
                        || result.stale_card_issue_error_count > 0
                    {
                        tracing::info!(
                            "[github-sync] {}: closed={}, inconsistencies={}, stale_issue_checks={}, stale_issue_batches={}, stale_issue_errors={}",
                            repo.id,
                            result.closed_count,
                            result.inconsistency_count,
                            result.stale_card_issue_check_count,
                            result.stale_card_issue_batch_count,
                            result.stale_card_issue_error_count
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
#[derive(Clone, Debug)]
struct PendingMessageOutboxRow {
    id: i64,
    target: String,
    content: String,
    bot: String,
    source: String,
    reason_code: Option<String>,
    session_key: Option<String>,
    retry_count: i64,
    claim_owner: String,
    claimed_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MessageOutboxFailureAction {
    Retry { retry_count: i64, backoff_secs: i64 },
    Fail { retry_count: i64 },
}

// reason: the `Db` payload and `StaleLeaseLost` fields are retained for
// `Debug` diagnostics and future structured logging; callers currently branch
// on the variant (`is_ok`/`matches!`) without reading the fields, so the lib
// build sees them as dead. See #3312.
#[allow(dead_code)]
#[derive(Debug)]
enum MessageOutboxLeaseUpdateError {
    Db(sqlx::Error),
    StaleLeaseLost {
        outbox_id: i64,
        claim_owner: String,
        claimed_at: chrono::DateTime<chrono::Utc>,
    },
}

fn is_terminal_turn_delivery_outbox_source(source: &str) -> bool {
    matches!(source, "headless_turn" | "turn_bridge_tmux_handoff")
}

#[cfg(test)]
fn session_can_be_released_for_terminal_outbox_failure(
    session_key: Option<&str>,
    failed_outbox_session_key: Option<&str>,
    active_turn_delivery_outbox_id: Option<i64>,
    failed_outbox_id: i64,
) -> bool {
    let session_key_matches = match failed_outbox_session_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(failed_session_key) => session_key == Some(failed_session_key),
        None => true,
    };

    session_key_matches && active_turn_delivery_outbox_id == Some(failed_outbox_id)
}

async fn release_session_for_terminal_outbox_failure(
    pg_pool: &PgPool,
    channel_id_str: &str,
    outbox_id: i64,
) -> Result<u64, sqlx::Error> {
    // Keep these predicates in sync with
    // `session_can_be_released_for_terminal_outbox_failure`.
    let result = sqlx::query(
        "WITH lock_guard AS (
            SELECT pg_advisory_xact_lock(1752, hashtext($1))
         ),
         failed_outbox AS (
            SELECT id, session_key
              FROM message_outbox
             WHERE id = $2
         )
         UPDATE sessions
            SET status = 'idle',
                active_turn_delivery_outbox_id = NULL
           FROM failed_outbox, lock_guard
          WHERE sessions.thread_channel_id = $1
            AND sessions.status IN ('turn_active', 'working')
            AND sessions.active_turn_delivery_outbox_id = failed_outbox.id
            AND (
                failed_outbox.session_key IS NULL
                OR failed_outbox.session_key = ''
                OR sessions.session_key = failed_outbox.session_key
            )",
    )
    .bind(channel_id_str)
    .bind(outbox_id)
    .execute(pg_pool)
    .await?;

    Ok(result.rows_affected())
}

fn message_outbox_failure_action(retry_count: i64) -> MessageOutboxFailureAction {
    let next_retry_count = retry_count.saturating_add(1);
    if next_retry_count > MESSAGE_OUTBOX_MAX_RETRY_COUNT {
        return MessageOutboxFailureAction::Fail {
            retry_count: next_retry_count,
        };
    }

    let backoff_idx = (next_retry_count - 1) as usize;
    let backoff_secs = MESSAGE_OUTBOX_RETRY_BACKOFF_SECS
        .get(backoff_idx)
        .copied()
        .unwrap_or(3600);
    MessageOutboxFailureAction::Retry {
        retry_count: next_retry_count,
        backoff_secs,
    }
}

#[cfg(test)]
mod message_outbox_retry_tests {
    use super::outbox_delivery_alert::{
        OUTBOX_DELIVERY_ALERT_SOURCE, note_terminal_outbox_delivery_failure, outbox_alert_snippet,
        outbox_alert_target_pg,
    };
    use super::{
        MessageOutboxFailureAction, MessageOutboxLeaseUpdateError, PendingMessageOutboxRow,
        drain_message_outbox_batch_once, is_terminal_turn_delivery_outbox_source,
        mark_message_outbox_sent_pg, message_outbox_failure_action,
        session_can_be_released_for_terminal_outbox_failure,
    };
    use sqlx::Row as _;

    #[test]
    fn outbox_alert_snippet_truncates_and_handles_empty() {
        assert_eq!(outbox_alert_snippet("  hello  "), "hello");
        assert_eq!(outbox_alert_snippet("   "), "(빈 내용)");
        let long: String = "a".repeat(200);
        let snippet = outbox_alert_snippet(&long);
        assert!(
            snippet.chars().count() <= 121,
            "120 content chars + one ellipsis"
        );
        assert!(snippet.ends_with('…'));
    }

    /// #4260 vector 3: a terminal outbox failure enqueues exactly one operator
    /// card to the `kanban_human_alert_channel_id`, never targets the failing
    /// destination channel, and the alert card's OWN failure does not recurse.
    #[tokio::test]
    async fn terminal_outbox_failure_enqueues_ops_card_and_guards_recursion_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_outbox_delivery_alert",
            "outbox terminal failure ops alert",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;

        // Unconfigured target ⇒ resolver returns None ⇒ no ops card is enqueued.
        assert!(outbox_alert_target_pg(&pool).await.is_none());

        sqlx::query(
            "INSERT INTO kv_meta (key, value) VALUES ('kanban_human_alert_channel_id', '555')",
        )
        .execute(&pool)
        .await
        .expect("seed alert channel");
        assert_eq!(
            outbox_alert_target_pg(&pool).await.as_deref(),
            Some("channel:555"),
            "bare channel id must be normalized to a channel: target"
        );

        let row = |id: i64, source: &str| PendingMessageOutboxRow {
            id,
            target: "channel:123".to_string(),
            content: "undeliverable body".to_string(),
            bot: "notify".to_string(),
            source: source.to_string(),
            reason_code: None,
            session_key: Some("sess-1".to_string()),
            retry_count: 5,
            claim_owner: "owner".to_string(),
            claimed_at: chrono::Utc::now(),
        };

        // A normal (non-alert) source enqueues exactly one ops card (the DB
        // work rides a detached spawn — await the returned handle so the
        // assertion is deterministic).
        note_terminal_outbox_delivery_failure(&pool, &row(1, "headless_turn"), "500: boom")
            .expect("non-alert source must spawn the ops-card task")
            .await
            .expect("ops-card task must not panic");
        let alert_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM message_outbox
               WHERE source = $1 AND target = 'channel:555'",
        )
        .bind(OUTBOX_DELIVERY_ALERT_SOURCE)
        .fetch_one(&pool)
        .await
        .expect("count alert rows");
        assert_eq!(alert_count, 1);
        let (alert_bot, alert_reason): (String, Option<String>) =
            sqlx::query_as("SELECT bot, reason_code FROM message_outbox WHERE source = $1")
                .bind(OUTBOX_DELIVERY_ALERT_SOURCE)
                .fetch_one(&pool)
                .await
                .expect("load actionable ops alert routing");
        assert_eq!(alert_bot, "announce");
        assert_eq!(alert_reason.as_deref(), Some("outbox_delivery_failed"));

        // The alert card's OWN terminal failure must NOT even spawn a card task
        // (recursion guard short-circuits before the detached DB work).
        assert!(
            note_terminal_outbox_delivery_failure(
                &pool,
                &row(2, OUTBOX_DELIVERY_ALERT_SOURCE),
                "500: boom",
            )
            .is_none(),
            "alert-source failures must not spawn a card-for-a-card"
        );
        let alert_count_after: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::bigint FROM message_outbox WHERE source = $1")
                .bind(OUTBOX_DELIVERY_ALERT_SOURCE)
                .fetch_one(&pool)
                .await
                .expect("count alert rows after recursion guard");
        assert_eq!(
            alert_count_after, 1,
            "the alert source must never recurse into a new card"
        );

        // The ops card is never sent to the failing destination channel.
        let to_dest: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM message_outbox
               WHERE source = $1 AND target = 'channel:123'",
        )
        .bind(OUTBOX_DELIVERY_ALERT_SOURCE)
        .fetch_one(&pool)
        .await
        .expect("count destination-directed alert rows");
        assert_eq!(
            to_dest, 0,
            "must not notify the failing destination channel"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn message_outbox_failure_action_retries_then_fails() {
        assert_eq!(
            message_outbox_failure_action(0),
            MessageOutboxFailureAction::Retry {
                retry_count: 1,
                backoff_secs: 60,
            }
        );
        assert_eq!(
            message_outbox_failure_action(3),
            MessageOutboxFailureAction::Retry {
                retry_count: 4,
                backoff_secs: 3600,
            }
        );
        assert_eq!(
            message_outbox_failure_action(4),
            MessageOutboxFailureAction::Fail { retry_count: 5 }
        );
    }

    #[test]
    fn session_release_is_limited_to_terminal_turn_delivery_sources() {
        assert!(is_terminal_turn_delivery_outbox_source("headless_turn"));
        assert!(is_terminal_turn_delivery_outbox_source(
            "turn_bridge_tmux_handoff"
        ));

        assert!(!is_terminal_turn_delivery_outbox_source(
            crate::services::message_outbox::LIFECYCLE_NOTIFIER_SOURCE
        ));
        assert!(!is_terminal_turn_delivery_outbox_source("routine-runtime"));
        assert!(!is_terminal_turn_delivery_outbox_source(
            "quality_regression_alerter"
        ));
        assert!(!is_terminal_turn_delivery_outbox_source("system"));
    }

    #[test]
    fn session_release_requires_same_failed_outbox_session_when_present() {
        assert!(session_can_be_released_for_terminal_outbox_failure(
            Some("session-current"),
            Some("session-current"),
            Some(42),
            42,
        ));
        assert!(!session_can_be_released_for_terminal_outbox_failure(
            Some("session-current"),
            Some("session-old"),
            Some(42),
            42,
        ));
        assert!(session_can_be_released_for_terminal_outbox_failure(
            Some("session-current"),
            Some("  "),
            Some(42),
            42,
        ));
    }

    #[test]
    fn session_release_requires_matching_terminal_delivery_outbox_marker() {
        assert!(!session_can_be_released_for_terminal_outbox_failure(
            Some("session-current"),
            Some("session-current"),
            None,
            42,
        ));
        assert!(!session_can_be_released_for_terminal_outbox_failure(
            Some("session-current"),
            Some("session-current"),
            Some(41),
            42,
        ));
        assert!(session_can_be_released_for_terminal_outbox_failure(
            Some("session-current"),
            Some("session-current"),
            Some(42),
            42,
        ));
    }

    #[tokio::test]
    async fn old_owner_completion_after_stale_reclaim_is_noop_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_message_outbox_lease",
            "message outbox lease fencing tests",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;

        let row = sqlx::query(
            "INSERT INTO message_outbox (
                target, content, bot, source, status, retry_count, claimed_at, claim_owner
             ) VALUES (
                'channel:123', 'hello', 'notify', 'system', 'processing', 0,
                NOW() - INTERVAL '10 minutes', 'old-owner'
             )
             RETURNING id, claimed_at",
        )
        .fetch_one(&pool)
        .await
        .expect("seed claimed message outbox row");
        let outbox_id: i64 = row.try_get("id").unwrap();
        let old_claimed_at: chrono::DateTime<chrono::Utc> = row.try_get("claimed_at").unwrap();

        sqlx::query(
            "UPDATE message_outbox
                SET claim_owner = 'new-owner',
                    claimed_at = NOW()
              WHERE id = $1",
        )
        .bind(outbox_id)
        .execute(&pool)
        .await
        .expect("reclaim message outbox row");

        let result =
            mark_message_outbox_sent_pg(&pool, outbox_id, "old-owner", old_claimed_at).await;
        assert!(matches!(
            result,
            Err(MessageOutboxLeaseUpdateError::StaleLeaseLost { .. })
        ));

        let state = sqlx::query(
            "SELECT status, claim_owner, sent_at
               FROM message_outbox
              WHERE id = $1",
        )
        .bind(outbox_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(state.try_get::<String, _>("status").unwrap(), "processing");
        assert_eq!(
            state.try_get::<String, _>("claim_owner").unwrap(),
            "new-owner"
        );
        assert!(
            state
                .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("sent_at")
                .unwrap()
                .is_none()
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// #4460/#4446 integration: the real message-outbox claim/drain path must
    /// retain the stall alert's provider-owned DM identity through delivery.
    #[tokio::test]
    async fn stall_alert_dm_row_drains_with_provider_bot_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_stall_alert_outbox_drain",
            "stall alert message-outbox drain routing tests",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        let target = "channel:1479662682909966499";
        let session_key = "codex/test/host:AgentDesk-codex-dm-343742347365974026";
        let inserted = crate::services::message_outbox::enqueue_outbox_pg_with_ttl(
            &pool,
            crate::services::message_outbox::OutboxMessage {
                target,
                content: "stall alert drain fixture",
                bot: "notify",
                source: "stall_watchdog",
                reason_code: Some("stall_watchdog_suspected_stall"),
                session_key: Some(session_key),
            },
            1800,
        )
        .await
        .expect("enqueue stall alert row");
        assert!(inserted);

        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed_delivery = observed.clone();
        let drained = drain_message_outbox_batch_once(&pool, Some("stall-alert-drain-test"), {
            move |row| {
                let observed_delivery = observed_delivery.clone();
                async move {
                    let bot = crate::services::message_outbox::delivery_bot_for_target_session(
                        &row.target,
                        &row.bot,
                        row.session_key.as_deref(),
                    )
                    .into_owned();
                    observed_delivery
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .push((row.target.clone(), row.source.clone(), bot));
                    ("200 OK".to_string(), String::new())
                }
            }
        })
        .await;

        assert_eq!(drained, 1);
        assert_eq!(
            *observed.lock().unwrap_or_else(|poison| poison.into_inner()),
            vec![(
                target.to_string(),
                "stall_watchdog".to_string(),
                "codex".to_string(),
            )]
        );
        let status: String =
            sqlx::query_scalar("SELECT status FROM message_outbox WHERE source='stall_watchdog'")
                .fetch_one(&pool)
                .await
                .expect("load drained stall alert status");
        assert_eq!(status, "sent");

        pool.close().await;
        pg_db.drop().await;
    }
}

impl PendingMessageOutboxRow {
    fn delivery_ids(&self) -> (String, String) {
        let reason = self
            .reason_code
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("message");
        let session = self
            .session_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(self.target.as_str());
        (
            format!("message_outbox:{}:{reason}:{session}", self.source),
            format!("message_outbox:{}:deliver", self.id),
        )
    }
}

async fn mark_message_outbox_sent_pg(
    pool: &PgPool,
    outbox_id: i64,
    claim_owner: &str,
    claimed_at: chrono::DateTime<chrono::Utc>,
) -> std::result::Result<(), MessageOutboxLeaseUpdateError> {
    let result = sqlx::query(
        "UPDATE message_outbox
            SET status = 'sent',
                sent_at = NOW(),
                error = NULL,
                retry_count = 0,
                next_attempt_at = NULL,
                claimed_at = NULL,
                claim_owner = NULL
          WHERE id = $1
            AND claim_owner = $2
            AND claimed_at = $3",
    )
    .bind(outbox_id)
    .bind(claim_owner)
    .bind(claimed_at)
    .execute(pool)
    .await
    .map_err(|error| {
        tracing::warn!(
            outbox_id,
            claim_owner,
            %claimed_at,
            %error,
            "[outbox] db failure while marking message_outbox row sent"
        );
        MessageOutboxLeaseUpdateError::Db(error)
    })?;
    if result.rows_affected() == 0 {
        tracing::warn!(
            outbox_id,
            claim_owner,
            %claimed_at,
            "[outbox] stale lease no-op while marking message_outbox row sent"
        );
        return Err(MessageOutboxLeaseUpdateError::StaleLeaseLost {
            outbox_id,
            claim_owner: claim_owner.to_string(),
            claimed_at,
        });
    }
    Ok(())
}

async fn mark_message_outbox_failed_pg(
    pool: &PgPool,
    outbox_id: i64,
    error_text: &str,
    retry_count: i64,
    claim_owner: &str,
    claimed_at: chrono::DateTime<chrono::Utc>,
) -> std::result::Result<(), MessageOutboxLeaseUpdateError> {
    let result = sqlx::query(
        "UPDATE message_outbox
            SET status = 'failed',
                error = $1,
                retry_count = $2,
                next_attempt_at = NULL,
                claimed_at = NULL,
                claim_owner = NULL
          WHERE id = $3
            AND claim_owner = $4
            AND claimed_at = $5",
    )
    .bind(error_text)
    .bind(retry_count)
    .bind(outbox_id)
    .bind(claim_owner)
    .bind(claimed_at)
    .execute(pool)
    .await
    .map_err(|error| {
        tracing::warn!(
            outbox_id,
            claim_owner,
            %claimed_at,
            %error,
            "[outbox] db failure while marking message_outbox row failed"
        );
        MessageOutboxLeaseUpdateError::Db(error)
    })?;
    if result.rows_affected() == 0 {
        tracing::warn!(
            outbox_id,
            claim_owner,
            %claimed_at,
            "[outbox] stale lease no-op while marking message_outbox row failed"
        );
        return Err(MessageOutboxLeaseUpdateError::StaleLeaseLost {
            outbox_id,
            claim_owner: claim_owner.to_string(),
            claimed_at,
        });
    }
    Ok(())
}

async fn schedule_message_outbox_retry_pg(
    pool: &PgPool,
    outbox_id: i64,
    error_text: &str,
    retry_count: i64,
    backoff_secs: i64,
    claim_owner: &str,
    claimed_at: chrono::DateTime<chrono::Utc>,
) -> std::result::Result<(), MessageOutboxLeaseUpdateError> {
    let result = sqlx::query(
        "UPDATE message_outbox
            SET status = 'pending',
                error = $1,
                retry_count = $2,
                next_attempt_at = NOW() + ($3::bigint * INTERVAL '1 second'),
                claimed_at = NULL,
                claim_owner = NULL
          WHERE id = $4
            AND claim_owner = $5
            AND claimed_at = $6",
    )
    .bind(error_text)
    .bind(retry_count)
    .bind(backoff_secs)
    .bind(outbox_id)
    .bind(claim_owner)
    .bind(claimed_at)
    .execute(pool)
    .await
    .map_err(|error| {
        tracing::warn!(
            outbox_id,
            claim_owner,
            %claimed_at,
            %error,
            "[outbox] db failure while scheduling message_outbox row retry"
        );
        MessageOutboxLeaseUpdateError::Db(error)
    })?;
    if result.rows_affected() == 0 {
        tracing::warn!(
            outbox_id,
            claim_owner,
            %claimed_at,
            "[outbox] stale lease no-op while scheduling message_outbox row retry"
        );
        return Err(MessageOutboxLeaseUpdateError::StaleLeaseLost {
            outbox_id,
            claim_owner: claim_owner.to_string(),
            claimed_at,
        });
    }
    Ok(())
}

async fn claim_pending_message_outbox_batch_pg(
    pool: &PgPool,
    claim_owner: &str,
) -> Vec<PendingMessageOutboxRow> {
    let rows = match sqlx::query(
        "WITH claimed AS (
            SELECT id
              FROM message_outbox
             WHERE (
                    status = 'pending'
                    AND (
                        next_attempt_at IS NULL
                        OR next_attempt_at <= NOW()
                    )
                 )
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
        RETURNING mo.id, mo.target, mo.content, mo.bot, mo.source, mo.reason_code, mo.session_key, mo.retry_count, mo.claim_owner, mo.claimed_at",
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
            Some(PendingMessageOutboxRow {
                id: row.try_get::<i64, _>("id").ok()?,
                target: row.try_get::<String, _>("target").ok()?,
                content: row.try_get::<String, _>("content").ok()?,
                bot: row.try_get::<String, _>("bot").ok()?,
                source: row.try_get::<String, _>("source").ok()?,
                reason_code: row.try_get::<Option<String>, _>("reason_code").ok()?,
                session_key: row.try_get::<Option<String>, _>("session_key").ok()?,
                retry_count: row.try_get::<i64, _>("retry_count").unwrap_or(0),
                claim_owner: row.try_get::<String, _>("claim_owner").ok()?,
                claimed_at: row
                    .try_get::<chrono::DateTime<chrono::Utc>, _>("claimed_at")
                    .ok()?,
            })
        })
        .collect::<Vec<_>>();
    claimed.sort_by_key(|row| row.id);
    claimed
}

async fn drain_message_outbox_batch_once<F, Fut>(
    pg_pool: &PgPool,
    claim_owner: Option<&str>,
    mut deliver: F,
) -> usize
where
    F: FnMut(PendingMessageOutboxRow) -> Fut,
    Fut: std::future::Future<Output = (String, String)>,
{
    let pending =
        claim_pending_message_outbox_batch_pg(pg_pool, claim_owner.unwrap_or("message-outbox"))
            .await;
    if pending.is_empty() {
        return 0;
    }

    for row in &pending {
        let (status, err_text) = deliver(row.clone()).await;
        if status == "200 OK" {
            if mark_message_outbox_sent_pg(pg_pool, row.id, &row.claim_owner, row.claimed_at)
                .await
                .is_ok()
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::debug!(
                    "[{ts}] [outbox] ✅ delivered msg {} → {}",
                    row.id,
                    row.target
                );
            }
        } else {
            let error_text = format!("{status}: {err_text}");
            let action = message_outbox_failure_action(row.retry_count);
            match action {
                MessageOutboxFailureAction::Fail { retry_count } => {
                    let failed_update = mark_message_outbox_failed_pg(
                        pg_pool,
                        row.id,
                        &error_text,
                        retry_count,
                        &row.claim_owner,
                        row.claimed_at,
                    )
                    .await;
                    // Release only terminal turn-delivery rows, and only if no newer
                    // session heartbeat proves another turn has since taken the channel.
                    if failed_update.is_ok() && is_terminal_turn_delivery_outbox_source(&row.source)
                    {
                        if let Some(channel_id_str) = row.target.strip_prefix("channel:") {
                            let released_sessions =
                                match release_session_for_terminal_outbox_failure(
                                    pg_pool,
                                    channel_id_str,
                                    row.id,
                                )
                                .await
                                {
                                    Ok(count) => count,
                                    Err(error) => {
                                        tracing::warn!(
                                            "[outbox] failed to release session for channel {} after msg {} failure: {}",
                                            channel_id_str,
                                            row.id,
                                            error,
                                        );
                                        0
                                    }
                                };
                            tracing::warn!(
                                "[outbox] ❌ permanent delivery failure for channel {} (msg {}, released_sessions={}): {}",
                                channel_id_str,
                                row.id,
                                released_sessions,
                                error_text,
                            );
                        }
                    }
                    // #4260 vector 3: every terminal failure surfaces — warn +
                    // quality event inline, ops card on a detached spawn. Sited
                    // AFTER the session release (dual r1 codex#1) so alerting
                    // never delays freeing the channel; the destination channel
                    // is never notified (it may be the failing target itself).
                    if failed_update.is_ok() {
                        drop(
                            outbox_delivery_alert::note_terminal_outbox_delivery_failure(
                                pg_pool,
                                row,
                                &error_text,
                            ),
                        );
                    }
                }
                MessageOutboxFailureAction::Retry {
                    retry_count,
                    backoff_secs,
                } => {
                    let _ = schedule_message_outbox_retry_pg(
                        pg_pool,
                        row.id,
                        &error_text,
                        retry_count,
                        backoff_secs,
                        &row.claim_owner,
                        row.claimed_at,
                    )
                    .await;
                }
            }
            tracing::warn!(
                "[outbox] ❌ msg {} → {} failed: {status} ({action:?})",
                row.id,
                row.target
            );
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
    // Periodic stale-row GC: prune old terminal rows so config rejections do not accumulate.
    let mut next_gc_at = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        tokio::time::sleep(poll_interval).await;

        // #3651: the message outbox drain delivers headless terminal responses
        // — foreground turns enqueue here and synchronously block on the row
        // becoming `sent` — so this loop is NOT backpressured. Yielding it under
        // pool pressure would delay (and risk later duplicate-recovery of)
        // user-visible delivery. Only genuinely low-priority chore loops gate on
        // `background_should_yield`.

        if std::time::Instant::now() >= next_gc_at {
            match crate::services::message_outbox::gc_stale_outbox_rows(pg_pool.as_ref()).await {
                Ok((held, failed, sent)) if held + failed + sent > 0 => {
                    tracing::info!(
                        held_pruned = held,
                        failed_pruned = failed,
                        sent_pruned = sent,
                        "[outbox] gc swept stale message_outbox rows"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!("[outbox] gc failed: {error}");
                }
            }
            next_gc_at = std::time::Instant::now() + Duration::from_secs(3600);
        }
        if drain_message_outbox_batch_once(pg_pool.as_ref(), Some(&claim_owner), {
            let health_registry = health_registry.clone();
            let pg_pool = pg_pool.clone();
            move |row| {
                let health_registry = health_registry.clone();
                let pg_pool = pg_pool.clone();
                async move {
                    outbox_actionable_delivery::deliver(&health_registry, pg_pool.as_ref(), &row)
                        .await
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
        crate::services::discord::retry_failed_dm_notifications(Some(pg_pool.as_ref())).await;
    }
}

async fn routine_runtime_loop(
    pg_pool: Arc<PgPool>,
    health_registry: Option<Arc<HealthRegistry>>,
    routines_config: crate::config::RoutinesConfig,
    routine_health_target: Option<String>,
    tick_interval_secs: u64,
) {
    use crate::services::routines::{
        RoutineAction, RoutineAgentExecutor, RoutineDiscordLogger, RoutineScriptLoader,
        RoutineStore, poll_agent_turns, run_due_tick,
    };
    let Some(tick_interval_secs) = std::num::NonZeroU64::new(tick_interval_secs) else {
        tracing::warn!("routine runtime not started: tick_interval_secs must be greater than zero");
        return;
    };

    let _routine_action_validator: fn(serde_json::Value) -> anyhow::Result<RoutineAction> =
        crate::services::routines::validate_routine_action;

    let script_loader = match RoutineScriptLoader::new() {
        Ok(loader) => loader,
        Err(e) => {
            tracing::warn!(error = %e, "routine runtime not started: script loader init failed");
            return;
        }
    };
    let routine_script_dirs = routines_config.script_dirs();
    match script_loader.load_dirs(&routine_script_dirs) {
        Ok(count) => tracing::info!(count, "routine script registry initialized"),
        Err(e) => tracing::warn!(error = %e, "routine script registry initialization failed"),
    }

    let store = RoutineStore::new_with_timezone_and_checkpoint_limit(
        pg_pool.clone(),
        routines_config.default_timezone.clone(),
        routines_config.max_checkpoint_bytes,
    );
    let discord_logger = RoutineDiscordLogger::new_with_health_registry(
        pg_pool.clone(),
        health_registry.clone(),
        routine_health_target,
    );
    let agent_executor =
        RoutineAgentExecutor::new(pg_pool, health_registry, routines_config.agent_timeout_secs);
    match store.recover_stale_running_runs().await {
        Ok(recovered) if !recovered.is_empty() => {
            for run in &recovered {
                // #3022: reap the orphaned fresh session the interrupted run
                // owned (positive ownership proof required) before logging, so a
                // dcserver restart no longer leaves a stranded fresh session to
                // be later misreported as an abrupt "session ended".
                agent_executor
                    .teardown_recovered_fresh_session(&store, run)
                    .await;
                discord_logger.log_recovery(&store, run).await;
            }
            tracing::info!(
                recovered = recovered.len(),
                "routine boot recovery: stale runs marked interrupted"
            )
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "routine boot recovery failed"),
    }
    let mut interval =
        tokio::time::interval(std::time::Duration::from_secs(tick_interval_secs.get()));
    loop {
        interval.tick().await;
        // Per-tick tunables (hot_reload toggle, poll/due-per-tick caps) are read
        // from the live config snapshot so a config-file edit takes effect on the
        // next tick without a restart. Boot-bound values (script dirs, store
        // timezone/checkpoint limits, agent timeout) keep their startup values
        // because they are already wired into long-lived objects above.
        let routines_config = crate::config_live_reload::current()
            .map(|cfg| cfg.routines.clone())
            .unwrap_or_else(|| routines_config.clone());
        match store.recover_stale_running_runs().await {
            Ok(recovered) if !recovered.is_empty() => {
                for run in &recovered {
                    // #3022: deliberately NO fresh-session reap here. Periodic
                    // recovery runs concurrently with claims/run-now, so the
                    // routine can be re-claimed and a replacement fresh run can
                    // create a new session under the same deterministic tmux
                    // name before any reap completes — racing the reap against a
                    // live turn. The reap is therefore confined to boot recovery
                    // (above), which runs before the tick loop with no concurrent
                    // claimer. An expired-lease orphan that slips through here is
                    // still collected by the idle-kill backstop.
                    discord_logger.log_recovery(&store, run).await;
                }
                tracing::info!(
                    recovered = recovered.len(),
                    "routine periodic recovery: expired-lease runs marked interrupted"
                )
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "routine periodic recovery failed"),
        }
        if routines_config.hot_reload {
            match script_loader.load_dirs(&routine_script_dirs) {
                Ok(count) if count > 0 => {
                    tracing::debug!(count, "routine script registry hot-reload pass complete")
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "routine script registry hot-reload failed"),
            }
        }
        // #3564: surface routines that have been stuck in `paused` past the
        // configured threshold. A failed/timed-out routine can otherwise stay
        // paused forever because paused routines are excluded from claims, so we
        // alert the operator instead of letting it silently never run again. The
        // knob defaults to 0 (disabled), so this is a no-op for deployments that
        // have not opted in.
        let stale_paused_alert_secs = routines_config.stale_paused_alert_secs;
        if stale_paused_alert_secs > 0 {
            let now = chrono::Utc::now();
            let cutoff = now - chrono::Duration::seconds(stale_paused_alert_secs as i64);
            match store.list_stale_paused_routines(cutoff).await {
                Ok(stale) => {
                    for routine in &stale {
                        let paused_for_secs = (now - routine.updated_at).num_seconds().max(0);
                        if paused_for_secs >= stale_paused_alert_secs as i64 {
                            discord_logger
                                .log_stale_paused(
                                    &store,
                                    routine,
                                    paused_for_secs,
                                    routines_config.stale_paused_alert_ttl_secs as i64,
                                )
                                .await;
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "routine stale-paused scan failed")
                }
            }
        }
        // #3573/#3628: opt-in auto-resume for failure-paused routines. Only
        // routines with `pause_reason = 'failure'` are eligible; manual/
        // migration_invalid/NULL rows are never touched. The
        // `ResumeRequiresNextDueAt` guard is applied inside
        // `auto_resume_failure_paused_routine`. The knob defaults to 0
        // (disabled); set to e.g. 3600 to enable with a 1-hour backoff.
        let auto_resume_secs = routines_config.failure_pause_auto_resume_secs;
        let pause_on_terminal_failure = routines_config.failure_pause_auto_resume_secs > 0;
        if pause_on_terminal_failure {
            let now = chrono::Utc::now();
            let cutoff = now - chrono::Duration::seconds(auto_resume_secs as i64);
            match store.list_failure_paused_routines(cutoff).await {
                Ok(candidates) => {
                    for routine in &candidates {
                        match store
                            .auto_resume_failure_paused_routine(&routine.id, cutoff)
                            .await
                        {
                            Ok(true) => {
                                tracing::info!(
                                    routine_id = %routine.id,
                                    routine_name = %routine.name,
                                    "auto-resumed failure-paused routine"
                                );
                            }
                            Ok(false) => {
                                tracing::debug!(
                                    routine_id = %routine.id,
                                    "auto-resume: routine no longer eligible (already resumed or re-paused)"
                                );
                            }
                            Err(error) => {
                                tracing::warn!(
                                    routine_id = %routine.id,
                                    error = %error,
                                    "auto-resume failure-paused routine failed"
                                );
                            }
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "routine auto-resume scan failed")
                }
            }
        }
        match poll_agent_turns(
            &store,
            &agent_executor,
            routines_config.max_agent_polls_per_tick,
            pause_on_terminal_failure,
        )
        .await
        {
            Ok(outcomes) if !outcomes.is_empty() => {
                for outcome in &outcomes {
                    discord_logger.log_run_outcome(&store, outcome).await;
                }
                tracing::info!(count = outcomes.len(), "routine agent turns completed")
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "routine agent turn polling failed"),
        }
        match run_due_tick(
            &store,
            &script_loader,
            &routine_script_dirs,
            Some(&agent_executor),
            Some(&discord_logger),
            routines_config.max_due_per_tick,
            pause_on_terminal_failure,
        )
        .await
        {
            Ok(outcomes) if !outcomes.is_empty() => {
                tracing::info!(count = outcomes.len(), "routine due tick executed")
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "routine due tick failed"),
        }
    }
}

/// Auth-boundary integration tests for the root-mounted control-plane routers
/// (`/tui/*`, `/hooks/*`). These live in the server layer because composing a
/// router with `auth::auth_middleware` is a server-layer responsibility — the
/// service modules only own the handler/validation behavior (#3311). The
/// service-layer tests (control-character rejection, handler success) stay
/// next to their handlers in `services::claude_tui`.
#[cfg(test)]
mod control_plane_auth_tests {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::extract::ConnectInfo;
    use axum::http::{Method, Request, StatusCode, header::AUTHORIZATION};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::server::routes::AppState;
    use crate::server::routes::auth::auth_middleware;
    use crate::services::claude_tui::hook_server::{
        HookServerState, hook_receiver_router_with_state,
    };
    use crate::services::claude_tui::tui_relay::{FakeSendBackend, router_with_send_backend};

    fn test_app_state(auth_token: Option<&str>) -> AppState {
        let mut config = crate::config::Config::default();
        // Non-loopback host so the middleware cannot fall back to a host-based
        // shortcut; the boundary must be proven by peer addr / Bearer alone.
        config.server.host = "0.0.0.0".to_string();
        config.server.auth_token = auth_token.map(str::to_string);
        let tx = crate::eventbus::new_broadcast();
        let buf = crate::eventbus::spawn_batch_flusher(tx.clone());
        AppState {
            pg_pool: None,
            engine: crate::engine::PolicyEngine::new(&config).expect("test policy engine"),
            config: Arc::new(config),
            broadcast_tx: tx,
            batch_buffer: buf,
            health_registry: None,
            cluster_instance_id: None,
        }
    }

    fn protected_tui_router(auth_token: Option<&str>) -> Router {
        router_with_send_backend(Arc::new(FakeSendBackend)).layer(
            axum::middleware::from_fn_with_state(test_app_state(auth_token), auth_middleware),
        )
    }

    fn protected_hook_router(auth_token: Option<&str>) -> Router {
        hook_receiver_router_with_state(HookServerState::new()).layer(
            axum::middleware::from_fn_with_state(test_app_state(auth_token), auth_middleware),
        )
    }

    fn tui_send_request(peer: &str, token: Option<&str>, text: &str) -> Request<Body> {
        let body = json!({
            "session_name": "test-session",
            "text": text,
            "submit": true,
        });
        let mut request = Request::builder()
            .method(Method::POST)
            .uri("/tui/send")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("build request");
        request.extensions_mut().insert(ConnectInfo(
            peer.parse::<SocketAddr>().expect("valid socket addr"),
        ));
        if let Some(token) = token {
            request.headers_mut().insert(
                AUTHORIZATION,
                format!("Bearer {token}").parse().expect("valid bearer"),
            );
        }
        request
    }

    fn hook_request(peer: &str, token: Option<&str>) -> Request<Body> {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri("/hooks/claude/Stop?session_id=sess-auth")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"hook_event_name":"Stop"}"#))
            .expect("build request");
        request.extensions_mut().insert(ConnectInfo(
            peer.parse::<SocketAddr>().expect("valid socket addr"),
        ));
        if let Some(token) = token {
            request.headers_mut().insert(
                AUTHORIZATION,
                format!("Bearer {token}").parse().expect("valid bearer"),
            );
        }
        request
    }

    async fn response_json(response: axum::response::Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        serde_json::from_slice(&bytes).expect("json response")
    }

    #[tokio::test]
    async fn tui_send_rejects_unauthenticated_non_loopback_when_protected() {
        let app = protected_tui_router(Some("secret"));

        let response = app
            .oneshot(tui_send_request("10.0.0.5:8791", None, "hello"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn tui_send_rejects_unauthenticated_non_loopback_when_unconfigured() {
        // Defense in depth: even with no auth_token configured ("local-only
        // mode"), a non-loopback caller must NOT reach the control plane.
        let app = protected_tui_router(None);

        let response = app
            .oneshot(tui_send_request("10.0.0.5:8791", None, "hello"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn tui_send_valid_input_with_valid_auth_returns_success() {
        let app = protected_tui_router(Some("secret"));

        let response = app
            .oneshot(tui_send_request(
                "10.0.0.5:8791",
                Some("secret"),
                "hello\nworld",
            ))
            .await
            .expect("response");
        let status = response.status();
        let body = response_json(response).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["session_name"], "test-session");
        assert_eq!(body["bytes"].as_u64(), Some("hello\nworld".len() as u64));
        assert_eq!(body["submitted"], true);
    }

    #[tokio::test]
    async fn tui_send_allows_loopback_without_bearer_when_protected() {
        let app = protected_tui_router(Some("secret"));

        let response = app
            .oneshot(tui_send_request("127.0.0.1:8791", None, "hello"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn hook_receiver_rejects_unauthenticated_non_loopback_when_protected() {
        let app = protected_hook_router(Some("secret"));

        let response = app
            .oneshot(hook_request("10.0.0.5:8791", None))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn hook_receiver_rejects_unauthenticated_non_loopback_when_unconfigured() {
        let app = protected_hook_router(None);

        let response = app
            .oneshot(hook_request("10.0.0.5:8791", None))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn hook_receiver_allows_loopback_without_bearer_when_protected() {
        let app = protected_hook_router(Some("secret"));

        let response = app
            .oneshot(hook_request("127.0.0.1:8791", None))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }
}
