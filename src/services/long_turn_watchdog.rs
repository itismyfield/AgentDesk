//! Long-turn cluster watchdog (#3557 (A)).
//!
//! The stall watchdog (`watchdog.rs`) only fires when a turn's watchdog token is
//! marked `desynced=true`. The `delegated_to_watcher` handoff path leaves
//! `desynced=false`, so a cluster of legitimately *finished but very long* turns
//! (the #3557 symptom in #adk-cc: avg 548s, >180s 63%, Codex outliers to
//! 13125s) is invisible to it. This probe closes that blind spot.
//!
//! It scans `observability_events` for `turn_finished` rows whose payload
//! `duration_ms` exceeds 600000 (10 min) inside a rolling 5-minute window. When
//! at least `LONG_TURN_CLUSTER_THRESHOLD` such turns land in one window it pages
//! out once to the deadlock-manager channel (or the shared fallback), so an
//! operator sees a sustained slow-turn cluster forming.
//!
//! Detection only — it never cancels turns. The per-turn hard ceiling and Codex
//! recv timeout (the same issue) own enforcement; this is the human-visible
//! signal that those backstops (or a deeper deadlock) are being exercised.

use std::time::Duration;

use serde_json::json;
use sqlx::{PgPool, Row};

/// Scan cadence and window length. A 5-minute window matched to a 5-minute scan
/// keeps the probe cheap (one indexed range scan) while still catching a cluster
/// soon after it forms.
const SCAN_INTERVAL: Duration = Duration::from_secs(300);

/// A turn longer than this (ms) counts toward the cluster. 600000ms == 10 min.
/// The issue flags >180s as the chronic problem and >600s as the acute tail;
/// 600s is the conservative, low-noise tier worth paging on.
const LONG_TURN_MS_THRESHOLD: i64 = 600_000;

/// Number of long turns inside one window required to page out. 3 distinguishes
/// a genuine cluster from a single legitimately long turn.
const LONG_TURN_CLUSTER_THRESHOLD: i64 = 3;

/// Window length used by the SQL aggregation (kept equal to `SCAN_INTERVAL` so
/// successive scans tile the timeline without gaps or double counting).
const WINDOW_SECONDS: i64 = 300;

/// Spawn the watchdog as a background task. The query is a single indexed range
/// scan every 5 minutes, so always-on is fine.
pub fn spawn(pool: PgPool) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(SCAN_INTERVAL);
        // Skip the immediate first tick so boot reconcile finishes first.
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = scan_once(&pool).await {
                tracing::warn!("[long_turn_watchdog] scan failed: {error}");
            }
        }
    });
}

/// Per-window aggregate the SQL produces.
struct LongTurnWindow {
    long_turn_count: i64,
    max_duration_ms: i64,
    codex_count: i64,
}

async fn scan_once(pool: &PgPool) -> Result<(), sqlx::Error> {
    let window = query_long_turn_window(pool).await?;

    if !cluster_breached(window.long_turn_count) {
        return Ok(());
    }

    let target = resolve_alert_channel();
    let message = format_long_turn_alert(&window);

    tracing::warn!(
        long_turn_count = window.long_turn_count,
        max_duration_ms = window.max_duration_ms,
        codex_count = window.codex_count,
        "[long_turn_watchdog] long-turn cluster detected"
    );

    crate::services::observability::events::record_simple(
        "long_turn_cluster",
        None,
        None,
        json!({
            "long_turn_count": window.long_turn_count,
            "max_duration_ms": window.max_duration_ms,
            "codex_count": window.codex_count,
            "threshold_ms": LONG_TURN_MS_THRESHOLD,
            "cluster_threshold": LONG_TURN_CLUSTER_THRESHOLD,
            "window_seconds": WINDOW_SECONDS,
        }),
    );

    if let Err(error) = enqueue_alert(pool, &target, &message).await {
        tracing::warn!("[long_turn_watchdog] enqueue alert failed: {error}");
    }

    Ok(())
}

/// Aggregate `turn_finished` rows in the last `WINDOW_SECONDS` whose
/// `payload_json->>'duration_ms'` exceeds the long-turn threshold.
async fn query_long_turn_window(pool: &PgPool) -> Result<LongTurnWindow, sqlx::Error> {
    let row = sqlx::query(
        "SELECT
             COUNT(*)::bigint AS long_turn_count,
             COALESCE(MAX((payload_json->>'duration_ms')::bigint), 0)::bigint AS max_duration_ms,
             COUNT(*) FILTER (WHERE provider = 'codex')::bigint AS codex_count
         FROM observability_events
         WHERE event_type = 'turn_finished'
           AND created_at >= NOW() - make_interval(secs => $1::int)
           AND (payload_json->>'duration_ms') IS NOT NULL
           AND (payload_json->>'duration_ms') ~ '^[0-9]+$'
           AND (payload_json->>'duration_ms')::bigint > $2",
    )
    .bind(WINDOW_SECONDS)
    .bind(LONG_TURN_MS_THRESHOLD)
    .fetch_one(pool)
    .await?;

    Ok(LongTurnWindow {
        long_turn_count: row.try_get("long_turn_count").unwrap_or(0),
        max_duration_ms: row.try_get("max_duration_ms").unwrap_or(0),
        codex_count: row.try_get("codex_count").unwrap_or(0),
    })
}

/// Whether a window's long-turn count meets the cluster threshold.
fn cluster_breached(long_turn_count: i64) -> bool {
    long_turn_count >= LONG_TURN_CLUSTER_THRESHOLD
}

/// Resolve the alert channel: the configured deadlock-manager channel if set,
/// otherwise the shared fallback (`#adk-cc`). The fallback const in `slo` is the
/// adk-cc snowflake `1479671298497183835`, the issue's investigation channel.
fn resolve_alert_channel() -> String {
    crate::config::load()
        .ok()
        .and_then(|config| {
            config
                .kanban
                .deadlock_manager_channel_id
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| crate::services::slo::FALLBACK_ALERT_CHANNEL.to_string())
}

fn format_long_turn_alert(window: &LongTurnWindow) -> String {
    let window_min = WINDOW_SECONDS / 60;
    let max_min = window.max_duration_ms / 1000 / 60;
    format!(
        "[LONG-TURN] {} turns >{}m finished in last {}m (codex={}, max={}m) — possible turn-length cluster/deadlock, check #3557 backstops",
        window.long_turn_count,
        LONG_TURN_MS_THRESHOLD / 1000 / 60,
        window_min,
        window.codex_count,
        max_min,
    )
}

async fn enqueue_alert(pool: &PgPool, target: &str, content: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO message_outbox (target, content, bot, source, reason_code, status)
         VALUES ($1, $2, 'notify', 'long_turn_watchdog', 'long_turn_cluster', 'pending')",
    )
    .bind(target)
    .bind(content)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_threshold_triggers_at_three() {
        assert!(!cluster_breached(0));
        assert!(!cluster_breached(LONG_TURN_CLUSTER_THRESHOLD - 1));
        assert!(cluster_breached(LONG_TURN_CLUSTER_THRESHOLD));
        assert!(cluster_breached(LONG_TURN_CLUSTER_THRESHOLD + 5));
    }

    #[test]
    fn fallback_channel_is_adk_cc_when_unconfigured() {
        // With no deadlock_manager_channel_id configured in the test env, the
        // resolver must fall back to the shared adk-cc alert channel rather
        // than panicking or returning empty.
        let channel = resolve_alert_channel();
        assert!(!channel.is_empty());
        // The shared fallback const is the adk-cc investigation channel.
        assert_eq!(channel, crate::services::slo::FALLBACK_ALERT_CHANNEL);
    }

    #[test]
    fn alert_message_contains_actionable_context() {
        let window = LongTurnWindow {
            long_turn_count: 4,
            max_duration_ms: 13_125_000,
            codex_count: 2,
        };
        let msg = format_long_turn_alert(&window);
        assert!(msg.contains("LONG-TURN"));
        assert!(msg.contains("4 turns"));
        assert!(msg.contains(">10m"));
        assert!(msg.contains("codex=2"));
        // 13125s ≈ 218 min.
        assert!(msg.contains("max=218m"));
        assert!(msg.contains("#3557"));
    }
}
