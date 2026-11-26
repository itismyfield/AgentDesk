mod bind_security_diagnostics;
pub(crate) mod cluster;
pub(crate) mod cluster_session_routing;
pub(crate) mod cron_catalog;
mod dashboard_auth;
mod dashboard_provision;
#[cfg(test)]
mod database_fixture_invariant_tests;
pub mod dto;
pub(crate) mod issue_specs;
pub(crate) mod maintenance;
pub(crate) mod multinode_regression;
mod outbox_actionable_delivery;
mod outbox_delivery_alert;
pub(crate) mod outbox_worker;
mod policy_tick_logging;
use outbox_worker::message_outbox_loop;
mod rate_limit_profiles;
mod rate_limit_sync;
use rate_limit_profiles::{sync_named_profile_rate_limits, upsert_rate_limit_cache_entry};
pub(crate) mod resource_locks;
pub mod routes;
mod routine_script_audit;
mod startup_preflight;
pub(crate) mod task_dispatch_claims;
pub(crate) mod test_phase_runs;
mod web_surface;
mod worker_recovery;
mod worker_registry;
pub mod ws;

mod github_sync;
mod message_outbox_drain;
mod policy_tick;
mod rate_limit_fetch;
mod routine_runtime;

use std::sync::Arc;

use anyhow::Result;
use sqlx::{PgPool, Row};

use github_sync::github_sync_loop;
use message_outbox_drain::{
    MessageOutboxFailureAction, PendingMessageOutboxRow, dm_reply_retry_loop,
    drain_message_outbox_batch_once, message_outbox_failure_action,
};
pub(crate) use policy_tick::PolicyTickHookOutcome;
#[cfg(test)]
pub(crate) use policy_tick::run_five_min_policy_tiers_for_test;
use policy_tick::{
    PolicyTickHookExecution, cleanup_stale_pending_queue_tmp_files_for_five_min_tick,
    fire_tick_hook_by_name_with_pg, is_five_min_policy_tick,
    refresh_memory_health_for_five_min_tick, run_leader_epoch_slo_api_friction_kickstart,
    run_slo_api_friction_aggregation_tick, should_run_slo_api_friction_aggregation,
};
use rate_limit_fetch::{
    CLAUDE_RATE_LIMIT_FORCED_REFRESH_TIMEOUT, claude_rate_limit_refresh_lock,
    fetch_codex_oauth_usage, fetch_openai_rate_limits, parse_claude_oauth_usage_buckets,
    parse_header_i64, parse_header_reset, refresh_dispatch_gate_snapshots,
};
pub(crate) use rate_limit_fetch::{
    ClaudeRateLimitRefreshOutcome, spawn_claude_rate_limit_refresh_if_leader,
    trigger_claude_rate_limit_refresh_if_leader,
};
use routine_runtime::routine_runtime_loop;

use crate::db::postgres::AdvisoryLockLease;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::config::Config;
use crate::engine::PolicyEngine;
use crate::services::discord::health::HealthRegistry;
use crate::services::engine_runtime::health_port::stale_sweep_port;

const MEMORY_HEALTH_STARTUP_REASON: &str = "startup";
const MESSAGE_OUTBOX_CLAIM_STALE_SECS: i64 = 300;
const POLICY_TICK_ADVISORY_LOCK_ID: i64 = 7_801_001;

async fn try_acquire_pg_singleton_lock(
    pool: &PgPool,
    lock_id: i64,
    job_name: &str,
) -> std::result::Result<Option<AdvisoryLockLease>, String> {
    AdvisoryLockLease::try_acquire(pool, lock_id, job_name).await
}

async fn release_pg_singleton_lock(lease: AdvisoryLockLease, lock_id: i64, job_name: &str) {
    if let Err(error) = lease.unlock().await {
        tracing::warn!("[{job_name}] failed to release advisory lock {lock_id}: {error}");
    }
}

async fn refresh_memory_health_for_startup() {
    crate::services::memory::refresh_backend_health(MEMORY_HEALTH_STARTUP_REASON).await;
}

pub(crate) async fn run(
    config: Config,
    engine: PolicyEngine,
    health_registry: Option<Arc<HealthRegistry>>,
    pg_pool: Option<PgPool>,
) -> Result<()> {
    crate::config::validate_config(&config)?;
    crate::services::running_binary::initialize();
    let modules = config.cluster.runtime_profile.modules();
    crate::services::dispatches::wait_queue::set_runtime_cluster_config(config.cluster.clone());
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
                    crate::services::settings::startup::startup_reseed_with_warmup_pool(
                        pool_ref, &config,
                    )
                    .await
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
        // Lets cancel-tombstone helpers with no pool argument still persist tombstones
        // across dcserver restarts.
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
    let cluster_runtime = cluster::bootstrap(
        &config,
        pg_pool.clone(),
        crate::services::dispatches::cluster_hooks::HOOKS,
    )
    .await;
    let cluster_instance_id = cluster_runtime.instance_id().to_string();
    if modules.hub_services
        && let Some(pool) = pg_pool.clone()
    {
        crate::services::dispatch_watchdog::spawn(pool);
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
        stale_sweep_port(health_registry.as_deref()),
        crate::services::discord::zombie_foreground_release::inflight_episode_lookup_for_tmux_name,
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
    if modules.dashboard {
        routes::receipt::spawn_token_analytics_cache_prewarm();
    }

    let dashboard_dir = dashboard_provision::prepare_dashboard(modules.dashboard).await;

    let broadcast_tx = ws::new_broadcast();
    let batch_buffer = worker_registry.start_after_websocket_broadcast(broadcast_tx.clone())?;
    let control_plane_auth_state = routes::AppState {
        pg_pool: pg_pool.clone(),
        engine: engine.clone(),
        config: Arc::new(config.clone()),
        broadcast_tx: broadcast_tx.clone(),
        batch_buffer: batch_buffer.clone(),
        health_registry: health_registry.clone(),
        cluster_instance_id: Some(cluster_instance_id.clone()),
    };

    let app = web_surface::router(
        control_plane_auth_state,
        &dashboard_dir,
        _claude_tui_hook_endpoint.is_some(),
    );

    // Auth is fail-open without `server.auth_token`, so a non-loopback bind would hand the
    // mutating control plane to any LAN peer. Force loopback rather than refuse to boot.
    let (bind_host, bind_decision) = routes::resolve_secure_bind_host(&config);
    bind_security_diagnostics::log_decision(&config, &bind_host, &bind_decision);
    let addr = format!("{}:{}", bind_host, config.server.port);
    let listener = crate::services::platform::network::bind_tcp_listener(&addr).await?;
    crate::config::test_instance::lifecycle::bound_listener(listener.local_addr()?)
        .map_err(anyhow::Error::msg)?;
    let app = crate::config::test_instance::lifecycle::with_control_routes(app)
        .map_err(anyhow::Error::msg)?;
    tracing::info!("HTTP server listening on {addr}");
    routes::audit_explicit_auth_routes_on_boot(&config);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// Fires OnTick30s each tick, OnTick1min every 2nd and OnTick5min every 10th, but a tick that
/// misses the advisory lock skips all three. Tiers run in turn: a slow one delays the next tick.
async fn policy_tick_loop(
    engine: PolicyEngine,
    pg_pool: Option<Arc<PgPool>>,
    cluster_runtime: Option<cluster::ClusterRuntime>,
    shutdown: Option<Arc<AtomicBool>>,
    // `None` only without Discord providers; the cleanup replay then converges its
    // PostgreSQL side and skips the in-memory teardown, as there is no runtime to tear down.
    health_registry: Option<Arc<HealthRegistry>>,
) {
    tracing::info!("[policy-tick] 3-tier tick started: 30s / 1min / 5min");

    let mut count = 0u64;
    let mut tick_logging = policy_tick_logging::TickLogging::default();
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

    // Skip the immediate first tick.
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
        fire_tick_hook_by_name_with_pg(
            &engine,
            pg_pool.as_deref(),
            &mut tick_logging,
            "OnTick30s",
            "30s",
        )
        .await;

        // ── 1min tier: every 2nd tick (60s) ──
        if count % 2 == 0 {
            // Before OnTick1min, so the auto-queue tick sees what was just handed off.
            if let Some(pool) = pg_pool.as_deref().or_else(|| engine.pg_pool())
                && let Err(error) =
                    crate::services::auto_queue::route::hand_off_auto_campaigns_pg(pool, &engine)
                        .await
            {
                tracing::warn!("[policy-tick] campaign handoff failed: {error}");
            }
            fire_tick_hook_by_name_with_pg(
                &engine,
                pg_pool.as_deref(),
                &mut tick_logging,
                "OnTick1min",
                "1min",
            )
            .await;
            if let Some(pool) = pg_pool.as_deref().or_else(|| engine.pg_pool()) {
                match crate::services::stale_turn_reconciler::reconcile_stale_turns_pg(
                    pool,
                    stale_sweep_port(health_registry.as_deref()),
                    crate::services::discord::relay_recovery::AxisBSite::PolicyTickStaleSweep,
                    crate::services::discord::zombie_foreground_release::inflight_episode_lookup_for_tmux_name,
                )
                .await
                {
                    Ok(reconciled) if reconciled > 0 => {
                        tracing::warn!(
                            reconciled,
                            "[policy-tick] reconciled stale busy sessions blocking mailbox injection"
                        );
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(
                            "[policy-tick] stale busy session reconcile failed: {error}"
                        );
                    }
                }

                // Resume post-commit cleanup a previous process left owed; this is how a
                // restarted process converges residual provider sessions and slot tokens.
                match crate::services::auto_queue::cleanup_tasks::replay_pending_run_cleanup_tasks_pg(
                    crate::services::auto_queue::runtime::slot_runtime_health(health_registry.clone()),
                    pool,
                )
                .await
                {
                    Ok(stats) if stats.touched() => {
                        tracing::warn!(
                            drained = stats.drained,
                            completed = stats.completed,
                            dead_lettered = stats.dead_lettered,
                            unrecorded_failures = stats.unrecorded_failures,
                            "[policy-tick] replayed auto-queue run cleanup tasks left by an earlier process"
                        );
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(
                            "[policy-tick] auto-queue run cleanup replay failed: {error}"
                        );
                    }
                }

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
            fire_tick_hook_by_name_with_pg(
                &engine,
                pg_pool.as_deref(),
                &mut tick_logging,
                "OnTick5min",
                "5min",
            )
            .await;
            refresh_memory_health_for_five_min_tick().await;
            cleanup_stale_pending_queue_tmp_files_for_five_min_tick().await;
            if let Some(pool) = pg_pool.as_deref().or_else(|| engine.pg_pool()) {
                crate::dispatch::replay_marked_dispatch_completions_pg(&engine, pool).await;
                // Keep idempotency_keys bounded (24h TTL).
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
            fire_tick_hook_by_name_with_pg(
                &engine,
                pg_pool.as_deref(),
                &mut tick_logging,
                "OnTick",
                "legacy",
            )
            .await;
        }

        if let Some(conn) = advisory_lock {
            release_pg_singleton_lock(conn, POLICY_TICK_ADVISORY_LOCK_ID, "policy-tick").await;
        }
    }
}

async fn record_tick_hook_execution_pg(
    pg_pool: &PgPool,
    tick_logging: &mut policy_tick_logging::TickLogging,
    label: &str,
    execution: &PolicyTickHookExecution,
) {
    let now_ms = chrono::Utc::now().timestamp_millis().to_string();
    let key_ms = format!("last_tick_{}_ms", label);
    let key_status = format!("last_tick_{}_status", label);
    let key_duration = format!("last_tick_{}_duration_ms", label);
    let key_skip_ms = format!("last_tick_{}_skip_ms", label);
    let status = execution.outcome.status();

    tick_logging.record(label, execution);

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

#[cfg(test)]
mod policy_tick_schedule_tests;

#[cfg(test)]
mod rate_limit_header_tests;

#[cfg(test)]
mod claude_oauth_usage_tests;

// reason: callers branch on the variant only; the fields are kept for `Debug` diagnostics.
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

#[cfg(test)]
mod message_outbox_retry_tests;

#[cfg(test)]
use crate::dispatch::test_support::DispatchPostgresTestDb;

/// Via the hourly `relay_signal_alerter` entry point, a breached signal is reported (WARN and
/// event, see the `relay_signal_alert` tests) and never becomes an outbox row.
#[cfg(test)]
async fn relay_signal_threshold_report_enqueues_nothing_pg(pg_db: DispatchPostgresTestDb) {
    let pool = pg_db.connect_and_migrate().await;
    // `relay_uncommitted_inflight_cleared` trips at a single occurrence.
    sqlx::query(
        "INSERT INTO observability_events (event_type, status)
             VALUES ('relay_root_cause_counter', 'relay_uncommitted_inflight_cleared')",
    )
    .execute(&pool)
    .await
    .expect("seed breached relay signal");

    let crossed = crate::services::observability::report_relay_signal_threshold_crossings_pg(&pool)
        .await
        .expect("report relay signals");

    assert_eq!(crossed, 1);
    let outbox_rows: i64 = sqlx::query_scalar("SELECT COUNT(*)::bigint FROM message_outbox")
        .fetch_one(&pool)
        .await
        .expect("count outbox rows");
    assert_eq!(
        outbox_rows, 0,
        "the report never enqueues a Discord message"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[cfg(test)]
async fn old_owner_completion_after_stale_reclaim_is_noop_pg(pg_db: DispatchPostgresTestDb) {
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

    let result = mark_message_outbox_sent_pg(&pool, outbox_id, "old-owner", old_claimed_at).await;
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

/// The real claim/drain path keeps the stall alert's provider-owned DM identity.
#[cfg(test)]
async fn stall_alert_dm_row_drains_with_provider_bot_pg(pg_db: DispatchPostgresTestDb) {
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

/// Mutation sentinel: removing the drain loop's fence call turns this into a delivery.
#[cfg(test)]
async fn drain_fences_superseded_circuit_row_before_delivery_pg(pg_db: DispatchPostgresTestDb) {
    let pool = pg_db.connect_and_migrate().await;
    // Stage a genuine circuit-stamped row through the validated producers.
    use crate::services::message_outbox_circuit_authority as circuit;
    sqlx::query(
            "INSERT INTO intake_session_owners(provider,raw_channel_id,owner_instance_id,generation,status)
             VALUES('discord','777','node-a',7,'active')",
        )
        .execute(&pool)
        .await
        .expect("seed active intake owner");
    let coord = match circuit::reserve_next_authority(
        &pool, "discord", "777", "node-a", 7, "e1", 10, 1, None,
    )
    .await
    .expect("reserve authority")
    {
        circuit::AuthorityReservation::Reserved(coordinate) => coordinate,
        other => panic!("unexpected reservation: {other:?}"),
    };
    let id = match circuit::stage_held(
        &pool,
        crate::services::message_outbox::OutboxMessage {
            target: "channel:777",
            content: "fenced body",
            bot: "notify",
            source: "system",
            reason_code: Some("fence"),
            session_key: Some("channel:777"),
        },
        &coord,
        300,
    )
    .await
    .expect("stage held circuit row")
    {
        circuit::StageHeldOutcome::Staged { id } => id,
        other => panic!("unexpected stage outcome: {other:?}"),
    };
    assert_eq!(
        circuit::activate_fenced(&pool, id, &coord)
            .await
            .expect("activate"),
        circuit::CircuitActivation::Activated
    );
    // A newer episode reserves epoch 2, superseding the row's stamped epoch 1.
    assert!(matches!(
        circuit::reserve_next_authority(&pool, "discord", "777", "node-a", 7, "e2", 10, 1, Some(1))
            .await
            .expect("supersede authority"),
        circuit::AuthorityReservation::Reserved(_)
    ));

    let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<i64>::new()));
    let observed_delivery = observed.clone();
    let drained = drain_message_outbox_batch_once(&pool, Some("fence-drain-test"), {
        move |row| {
            let observed_delivery = observed_delivery.clone();
            async move {
                observed_delivery
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .push(row.id);
                ("200 OK".to_string(), String::new())
            }
        }
    })
    .await;

    assert_eq!(drained, 1, "the row is claimed and accounted for");
    assert!(
        observed
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .is_empty(),
        "a superseded circuit row must never reach deliver()"
    );
    let row =
        sqlx::query("SELECT status,delivery_fence_checked_at FROM message_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("load fenced row");
    assert_eq!(row.get::<String, _>("status"), "cancelled");
    assert!(
        row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("delivery_fence_checked_at")
            .is_some(),
        "the fenced row must stamp delivery_fence_checked_at"
    );

    pool.close().await;
    pg_db.drop().await;
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
                error = NULL, attachment_filename = NULL, attachment_content_type = NULL, attachment_data = NULL,
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

use outbox_worker::claim::claim_pending_message_outbox_batch_pg;

/// Auth-boundary tests for the root-mounted control-plane routers (`/tui/*`, `/hooks/*`).
/// They live here because composing a router with `auth_middleware` is a server-layer job.
#[cfg(test)]
mod control_plane_auth_tests;
