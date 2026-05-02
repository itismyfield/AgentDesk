//! Stuck-dispatch watchdog (#1546).
//!
//! Scans `task_dispatches` periodically for rows that have been in the
//! `dispatched` state for too long without `updated_at` movement. Emits a
//! structured `dispatch_stuck` observability event so operators see the
//! drift immediately, without relying on a person tail-grepping tmux panes.
//!
//! Detection only — auto-cancel/requeue is intentionally deferred to a
//! follow-up so this first iteration cannot make a bad situation worse.
//! The signal is what was missing during the Phase 3 #1489 incident
//! (90+ min silently dispatched, dispatch row never updated).

use std::time::Duration;

use serde_json::json;
use sqlx::{PgPool, Row};

const SCAN_INTERVAL: Duration = Duration::from_secs(300);
const STUCK_THRESHOLD_MINUTES: i64 = 60;

/// Spawn the watchdog as a background task. Cheap query (single indexed
/// SELECT every 5 minutes), so always-on is fine.
pub fn spawn(pool: PgPool) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(SCAN_INTERVAL);
        // Skip the immediate first tick so boot reconcile finishes first.
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = scan_once(&pool).await {
                tracing::warn!("[dispatch_watchdog] scan failed: {error}");
            }
        }
    });
}

async fn scan_once(pool: &PgPool) -> Result<(), sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id,
                kanban_card_id,
                dispatch_type,
                to_agent_id,
                EXTRACT(EPOCH FROM (NOW() - updated_at))::bigint AS idle_seconds
         FROM task_dispatches
         WHERE status = 'dispatched'
           AND completed_at IS NULL
           AND updated_at < NOW() - make_interval(mins => $1)",
    )
    .bind(STUCK_THRESHOLD_MINUTES)
    .fetch_all(pool)
    .await?;

    for row in rows {
        let id: String = row.try_get("id").unwrap_or_default();
        let card_id: Option<String> = row.try_get("kanban_card_id").ok();
        let dispatch_type: Option<String> = row.try_get("dispatch_type").ok();
        let agent_id: Option<String> = row.try_get("to_agent_id").ok();
        let idle_seconds: i64 = row.try_get("idle_seconds").unwrap_or(0);
        let idle_minutes = idle_seconds / 60;

        tracing::warn!(
            dispatch_id = %id,
            kanban_card_id = ?card_id,
            dispatch_type = ?dispatch_type,
            agent_id = ?agent_id,
            idle_minutes,
            "[dispatch_watchdog] stuck dispatch detected"
        );

        crate::services::observability::events::record_simple(
            "dispatch_stuck",
            None,
            None,
            json!({
                "dispatch_id": id,
                "kanban_card_id": card_id,
                "dispatch_type": dispatch_type,
                "agent_id": agent_id,
                "idle_minutes": idle_minutes,
                "threshold_minutes": STUCK_THRESHOLD_MINUTES,
            }),
        );
    }

    Ok(())
}
