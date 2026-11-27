use super::{
    Arc, AtomicBool, Duration, Ordering, POLICY_TICK_ADVISORY_LOCK_ID, PgPool, PolicyEngine,
    policy_tick_logging, record_tick_hook_execution_pg, release_pg_singleton_lock,
    try_acquire_pg_singleton_lock,
};

const MEMORY_HEALTH_FIVE_MIN_REASON: &str = "OnTick5min";
const FIVE_MIN_POLICY_TICK_INTERVAL: u64 = 10;
const POLICY_TICK_HOOK_TIMEOUT: Duration = Duration::from_secs(5);

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
    pub(super) fn status(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Timeout => "timeout",
            Self::SkippedInFlight => "skipped_inflight",
            Self::JoinError => "join_error",
        }
    }
}

pub(super) struct PolicyTickHookExecution {
    pub(super) outcome: PolicyTickHookOutcome,
    pub(super) elapsed: Duration,
    pub(super) error: Option<String>,
}

pub(super) async fn refresh_memory_health_for_five_min_tick() {
    crate::services::memory::refresh_backend_health(MEMORY_HEALTH_FIVE_MIN_REASON).await;
}

pub(super) async fn cleanup_stale_pending_queue_tmp_files_for_five_min_tick() {
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

pub(super) fn is_five_min_policy_tick(count: u64) -> bool {
    count != 0 && count % FIVE_MIN_POLICY_TICK_INTERVAL == 0
}

pub(super) fn should_run_slo_api_friction_aggregation(
    count: u64,
    leader_epoch_pending: bool,
) -> bool {
    leader_epoch_pending || is_five_min_policy_tick(count)
}

pub(super) async fn run_slo_api_friction_aggregation_tick(
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

    // Turn-lifecycle SLO aggregation: compute, persist, and alert on threshold breach.
    let now_ms = chrono::Utc::now().timestamp_millis();
    let aggregates = crate::services::slo::run_aggregation_tick(pool, now_ms).await;
    tracing::debug!(
        reason,
        aggregate_count = aggregates.len(),
        "[policy-tick] SLO aggregation completed"
    );
}

pub(super) async fn run_leader_epoch_slo_api_friction_kickstart(
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

#[cfg(test)]
pub(crate) async fn run_five_min_policy_tiers_for_test(
    pool: &PgPool,
    engine: &PolicyEngine,
) -> anyhow::Result<()> {
    let mut logging = policy_tick_logging::TickLogging::default();
    for (hook, label) in [("OnTick1min", "1min"), ("OnTick5min", "5min")] {
        fire_tick_hook_by_name_with_pg(engine, Some(pool), &mut logging, hook, label).await;
    }
    crate::reconcile::reconcile_completed_queue_review_drift_pg(pool, engine).await?;
    Ok(())
}

pub(super) async fn fire_tick_hook_by_name_with_pg(
    engine: &PolicyEngine,
    pg_pool: Option<&PgPool>,
    tick_logging: &mut policy_tick_logging::TickLogging,
    hook_name: &str,
    label: &str,
) {
    // The orphan review timeout runs on the one-minute tier, including five-minute cycles.
    if hook_name == "OnTick1min"
        && let Some(pool) = pg_pool.or_else(|| engine.pg_pool())
        && let Err(error) =
            crate::reconcile::reconcile_missing_queue_review_dispatches_pg(pool, engine).await
    {
        tracing::warn!("[policy-tick] missing queue review dispatch reconcile failed: {error}");
    }
    let execution =
        fire_tick_hook_by_name_with_timeout(engine, hook_name, label, POLICY_TICK_HOOK_TIMEOUT)
            .await;
    if let Some(pool) = pg_pool.or_else(|| engine.pg_pool()) {
        record_tick_hook_execution_pg(pool, tick_logging, label, &execution).await;
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
