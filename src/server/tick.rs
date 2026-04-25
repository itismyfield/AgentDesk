use crate::db::Db;
use crate::engine::PolicyEngine;

const MEMORY_HEALTH_STARTUP_REASON: &str = "startup";
const MEMORY_HEALTH_FIVE_MIN_REASON: &str = "OnTick5min";
const FIVE_MIN_POLICY_TICK_INTERVAL: u64 = 10;

pub(super) async fn refresh_memory_health_for_startup() {
    crate::services::memory::refresh_backend_health(MEMORY_HEALTH_STARTUP_REASON).await;
}

async fn refresh_memory_health_for_five_min_tick() {
    crate::services::memory::refresh_backend_health(MEMORY_HEALTH_FIVE_MIN_REASON).await;
}

fn is_five_min_policy_tick(count: u64) -> bool {
    count != 0 && count % FIVE_MIN_POLICY_TICK_INTERVAL == 0
}

/// Background task that fires tiered OnTick hooks at different intervals (#127).
///
/// 3 tiers to prevent slow sections from blocking time-critical recovery:
/// - OnTick30s (30s): retry, unsent notification recovery, deadlock detection [I], orphan recovery [K]
/// - OnTick1min (1m): non-critical timeouts [A][C][D][E][L], stale detection
/// - OnTick5min (5m): non-critical reconciliation [R][B][F][G][H][M][O], idle session cleanup
/// - OnTick (legacy, 5m): backward compat for policies that only register onTick
pub(super) async fn policy_tick_loop(engine: PolicyEngine, db: Db) {
    use std::time::Duration;

    tracing::info!("[policy-tick] 3-tier tick started: 30s / 1min / 5min");

    let mut interval_30s = tokio::time::interval(Duration::from_secs(30));
    let mut count = 0u64;

    interval_30s.tick().await;

    loop {
        interval_30s.tick().await;
        count += 1;

        fire_tick_hook_by_name(&engine, &db, "OnTick30s", "30s");
        drain_transitions(&engine, &db);

        if count % 2 == 0 {
            fire_tick_hook_by_name(&engine, &db, "OnTick1min", "1min");
            drain_transitions(&engine, &db);
        }

        if is_five_min_policy_tick(count) {
            fire_tick_hook_by_name(&engine, &db, "OnTick5min", "5min");
            drain_transitions(&engine, &db);
            refresh_memory_health_for_five_min_tick().await;
            if let Err(error) =
                crate::services::api_friction::process_api_friction_patterns(&db, None, None).await
            {
                tracing::warn!("[policy-tick] api-friction aggregation failed: {error}");
            }
            // #1072 turn-lifecycle SLO aggregation (Epic #905 Phase 1).
            let now_ms = chrono::Utc::now().timestamp_millis();
            let _ = crate::services::slo::run_aggregation_tick(Some(&db), None, now_ms).await;
            fire_tick_hook_by_name(&engine, &db, "OnTick", "legacy");
            drain_transitions(&engine, &db);
        }
    }
}

fn fire_tick_hook_by_name(engine: &PolicyEngine, db: &Db, hook_name: &str, label: &str) {
    let start = std::time::Instant::now();
    let now_ms = chrono::Utc::now().timestamp_millis().to_string();

    let key_ms = format!("last_tick_{}_ms", label);
    let key_status = format!("last_tick_{}_status", label);

    if let Err(error) = engine.try_fire_hook_by_name(hook_name, serde_json::json!({})) {
        tracing::warn!("[policy-tick] {} hook error: {error}", label);
        if let Ok(conn) = db.lock() {
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, 'error')",
                [&key_status],
            )
            .ok();
        }
    } else {
        let elapsed = start.elapsed();
        if elapsed.as_millis() > 500 {
            tracing::warn!("[policy-tick] {} took {}ms", label, elapsed.as_millis());
        } else {
            tracing::debug!("[policy-tick] {} took {}ms", label, elapsed.as_millis());
        }
        if let Ok(conn) = db.lock() {
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                libsql_rusqlite::params![key_ms, now_ms],
            )
            .ok();
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, 'ok')",
                [&key_status],
            )
            .ok();
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('last_tick_ms', ?1)",
                [&now_ms],
            )
            .ok();
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('last_tick_status', 'ok')",
                [],
            )
            .ok();
        }
    }

    crate::kanban::drain_hook_side_effects(db, engine);
}

fn drain_transitions(engine: &PolicyEngine, db: &Db) {
    crate::kanban::drain_hook_side_effects(db, engine);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn startup_memory_health_refresh_uses_startup_reason() {
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
        crate::services::memory::reset_backend_health_for_tests();
        refresh_memory_health_for_five_min_tick().await;
        assert_eq!(
            crate::services::memory::last_refresh_reason_for_tests().as_deref(),
            Some(MEMORY_HEALTH_FIVE_MIN_REASON)
        );
    }
}
