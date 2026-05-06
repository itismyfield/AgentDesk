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
//!
//! Discord page-out replaces the previous `[SLO] avg turn latency` /
//! `[SLO] turn success rate` alerts which were demoted to record-only after
//! they were shown to be dominated by noise (sample=1..2 windows on a
//! platform whose normal turn length spans 5–30 min).

use std::time::Duration;

use serde_json::json;
use sqlx::{PgPool, Row};

use crate::services::slo;

const SCAN_INTERVAL: Duration = Duration::from_secs(300);

/// Stuck threshold for `implementation` dispatches.
///
/// 30-day production data (n=374) shows implementation runs p50=17m, p95=68m,
/// p99=115m. The previous flat 60-minute threshold mis-classified roughly 5%
/// of healthy runs as stuck. 180m sits ~1.5× above p99 and still catches the
/// real failure mode (#1489: 90+ min silently dispatched, but in practice
/// stuck dispatches sit for hours).
const STUCK_THRESHOLD_IMPLEMENTATION: i64 = 180;

/// Stuck threshold for every other dispatch type (review, rework,
/// review-decision, phase-gate, e2e-test, create-pr, NULL/unknown).
///
/// Their 30-day p99 sits at 38–72m, so 90m clears every legitimate run with
/// margin. phase-gate's small sample (n=17, max=74m) does not justify a
/// dedicated tier yet — revisit if its volume grows.
const STUCK_THRESHOLD_OTHER: i64 = 90;

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
    // `last_stuck_alert_at IS NULL` keeps the page-out one-shot per stuck
    // incident — the column is cleared when the dispatch leaves `dispatched`
    // (see services that flip status to completed/failed/cancelled).
    //
    // The CASE clause encodes the 2-tier threshold: implementation runs are
    // legitimately long (p99 ≈ 115 min in 30-day production data) while every
    // other type completes well under an hour. A flat 60-minute threshold
    // mis-classified ~5% of healthy implementation runs.
    let rows = sqlx::query(
        "SELECT id,
                kanban_card_id,
                dispatch_type,
                to_agent_id,
                EXTRACT(EPOCH FROM (NOW() - updated_at))::bigint AS idle_seconds,
                CASE WHEN dispatch_type = 'implementation' THEN $1 ELSE $2 END AS threshold_minutes
         FROM task_dispatches
         WHERE status = 'dispatched'
           AND completed_at IS NULL
           AND last_stuck_alert_at IS NULL
           AND updated_at < NOW() - make_interval(mins =>
               CASE WHEN dispatch_type = 'implementation' THEN $1 ELSE $2 END)",
    )
    .bind(STUCK_THRESHOLD_IMPLEMENTATION)
    .bind(STUCK_THRESHOLD_OTHER)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(());
    }

    let alert_channel = slo::resolve_alert_channel();

    for row in rows {
        let id: String = row.try_get("id").unwrap_or_default();
        let card_id: Option<String> = row.try_get("kanban_card_id").ok();
        let dispatch_type: Option<String> = row.try_get("dispatch_type").ok();
        let agent_id: Option<String> = row.try_get("to_agent_id").ok();
        let idle_seconds: i64 = row.try_get("idle_seconds").unwrap_or(0);
        let idle_minutes = idle_seconds / 60;
        let threshold_minutes: i64 = row
            .try_get("threshold_minutes")
            .unwrap_or(STUCK_THRESHOLD_OTHER);

        tracing::warn!(
            dispatch_id = %id,
            kanban_card_id = ?card_id,
            dispatch_type = ?dispatch_type,
            agent_id = ?agent_id,
            idle_minutes,
            threshold_minutes,
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
                "threshold_minutes": threshold_minutes,
            }),
        );

        let message = format_stuck_alert(
            &id,
            agent_id.as_deref(),
            dispatch_type.as_deref(),
            card_id.as_deref(),
            idle_minutes,
            threshold_minutes,
        );

        if let Err(error) = enqueue_stuck_alert(pool, &alert_channel, &message).await {
            tracing::warn!(
                dispatch_id = %id,
                "[dispatch_watchdog] enqueue stuck alert failed: {error}"
            );
            // Skip the cooldown-marker update so a transient enqueue failure
            // does not silently swallow the alert forever.
            continue;
        }

        if let Err(error) = mark_alert_sent(pool, &id).await {
            tracing::warn!(
                dispatch_id = %id,
                "[dispatch_watchdog] mark_alert_sent failed: {error}"
            );
        }
    }

    Ok(())
}

fn format_stuck_alert(
    dispatch_id: &str,
    agent_id: Option<&str>,
    dispatch_type: Option<&str>,
    kanban_card_id: Option<&str>,
    idle_minutes: i64,
    threshold_minutes: i64,
) -> String {
    let agent = agent_id.unwrap_or("?");
    let kind = dispatch_type.unwrap_or("?");
    let card = kanban_card_id.unwrap_or("-");
    format!(
        "[STUCK] dispatch={dispatch_id} agent={agent} type={kind} card={card} idle={idle_minutes}m (>{threshold_minutes}m threshold)",
    )
}

async fn enqueue_stuck_alert(
    pool: &PgPool,
    target: &str,
    content: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO message_outbox (target, content, bot, source, reason_code, status)
         VALUES ($1, $2, 'notify', 'dispatch_watchdog', 'dispatch_stuck', 'pending')",
    )
    .bind(target)
    .bind(content)
    .execute(pool)
    .await?;
    Ok(())
}

async fn mark_alert_sent(pool: &PgPool, dispatch_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE task_dispatches SET last_stuck_alert_at = NOW() WHERE id = $1")
        .bind(dispatch_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stuck_alert_message_contains_actionable_context() {
        let msg = format_stuck_alert(
            "disp-abc",
            Some("ch-td"),
            Some("review"),
            Some("CARD-1"),
            120,
            STUCK_THRESHOLD_OTHER,
        );
        assert!(msg.contains("disp-abc"));
        assert!(msg.contains("ch-td"));
        assert!(msg.contains("review"));
        assert!(msg.contains("CARD-1"));
        assert!(msg.contains("120m"));
        assert!(msg.contains(">90m"));
        assert!(msg.contains("STUCK"));
    }

    #[test]
    fn stuck_alert_handles_missing_optional_fields() {
        let msg = format_stuck_alert("disp-x", None, None, None, 95, STUCK_THRESHOLD_OTHER);
        assert!(msg.contains("disp-x"));
        assert!(msg.contains("agent=?"));
        assert!(msg.contains("type=?"));
        assert!(msg.contains("card=-"));
    }

    #[test]
    fn stuck_alert_renders_per_type_threshold() {
        let impl_msg = format_stuck_alert(
            "disp-impl",
            Some("ch-td"),
            Some("implementation"),
            None,
            200,
            STUCK_THRESHOLD_IMPLEMENTATION,
        );
        assert!(impl_msg.contains("type=implementation"));
        assert!(impl_msg.contains(">180m"));

        let other_msg = format_stuck_alert(
            "disp-rev",
            None,
            Some("review"),
            None,
            120,
            STUCK_THRESHOLD_OTHER,
        );
        assert!(other_msg.contains(">90m"));
    }
}
