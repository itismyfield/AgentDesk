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

/// #3557 (A) Codex-review fix: alert cooldown (seconds). A sustained or
/// overlapping cluster spans many consecutive 5-minute scans; without a cooldown
/// the raw insert paged once per scan (every 5 min) for the cluster's whole
/// lifetime. This dedupe window (via `message_outbox` `dedupe_key` + TTL,
/// following the #3561/#3564 `enqueue_outbox_pg_with_ttl` precedent) collapses a
/// persistent cluster to one page per window. 30 min ≫ the 5-min scan cadence so
/// repeated scans coalesce, while still re-paging if a cluster is still forming a
/// half-hour later. Override via `AGENTDESK_LONG_TURN_ALERT_COOLDOWN_SECS`.
const LONG_TURN_ALERT_COOLDOWN_SECS: i64 = 1800;

/// Stable dedupe session key for the cluster alert. The same key every scan is
/// what lets the `dedupe_key` (derived from target + reason_code + session_key)
/// collapse repeated alerts inside the cooldown TTL into a single outbox row.
const LONG_TURN_ALERT_SESSION_KEY: &str = "long_turn_watchdog:cluster";

/// `message_outbox.reason_code` for the cluster alert. Combined with the stable
/// session key above this gives a reason-code identity (content-independent)
/// dedupe key, so the alert dedupes even though its rendered counts vary scan to
/// scan.
const LONG_TURN_ALERT_REASON_CODE: &str = "long_turn_cluster";

/// Read the alert cooldown, allowing an env override for ops tuning.
fn alert_cooldown_secs() -> i64 {
    std::env::var("AGENTDESK_LONG_TURN_ALERT_COOLDOWN_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<i64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(LONG_TURN_ALERT_COOLDOWN_SECS)
}

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

    match enqueue_alert(pool, &target, &message).await {
        Ok(true) => {}
        Ok(false) => {
            // Suppressed by the cooldown dedupe key — the cluster is still
            // active but already paged inside this window.
            tracing::debug!(
                "[long_turn_watchdog] cluster alert suppressed by cooldown ({}s window)",
                alert_cooldown_secs()
            );
        }
        Err(error) => {
            tracing::warn!("[long_turn_watchdog] enqueue alert failed: {error}");
        }
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

/// Enqueue the cluster alert through the deduped `message_outbox` path so a
/// persistent/overlapping cluster pages at most once per cooldown window.
///
/// #3557 (A) Codex-review fix: the previous raw `INSERT` had no `dedupe_key`, so
/// every 5-minute scan over a long-lived cluster inserted a fresh row and the
/// channel got spammed. Routing through `enqueue_outbox_pg_with_ttl` with a
/// stable session key + reason code mirrors the `dispatch_watchdog`
/// `last_stuck_alert_at` cooldown intent using the durable DB dedupe key
/// (#3561/#3564 precedent), so suppression survives process restarts too.
///
/// Returns `Ok(true)` when a new alert row was actually enqueued and `Ok(false)`
/// when the cooldown suppressed it as a duplicate.
async fn enqueue_alert(
    pool: &PgPool,
    target: &str,
    content: &str,
) -> Result<bool, crate::services::message_outbox::OutboxEnqueueError> {
    crate::services::message_outbox::enqueue_outbox_pg_with_ttl(
        pool,
        crate::services::message_outbox::OutboxMessage {
            target,
            content,
            bot: crate::services::message_outbox::ACTIONABLE_OPS_ALERT_BOT,
            source: "long_turn_watchdog",
            reason_code: Some(LONG_TURN_ALERT_REASON_CODE),
            session_key: Some(LONG_TURN_ALERT_SESSION_KEY),
        },
        alert_cooldown_secs(),
    )
    .await
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

    /// #3557 (A) Codex-review fix: the alert cooldown must be a positive window
    /// strictly larger than the 5-minute scan cadence, otherwise consecutive
    /// scans over a persistent cluster would each page (the spam the dedupe
    /// fixes).
    #[test]
    fn alert_cooldown_exceeds_scan_cadence_by_default() {
        if std::env::var("AGENTDESK_LONG_TURN_ALERT_COOLDOWN_SECS").is_err() {
            assert_eq!(alert_cooldown_secs(), LONG_TURN_ALERT_COOLDOWN_SECS);
            assert!(
                alert_cooldown_secs() > SCAN_INTERVAL.as_secs() as i64,
                "cooldown ({}s) must exceed the scan cadence ({}s) so repeated scans coalesce",
                alert_cooldown_secs(),
                SCAN_INTERVAL.as_secs()
            );
        }
    }

    /// The dedupe identity (session key + reason code) must be stable across
    /// scans — that stability is exactly what collapses repeated alerts into a
    /// single outbox row via the `dedupe_key`. A drifting key would defeat the
    /// suppression, so pin both constants to non-empty, content-independent
    /// values.
    #[test]
    fn dedupe_identity_is_stable_and_content_independent() {
        assert!(!LONG_TURN_ALERT_SESSION_KEY.trim().is_empty());
        assert!(!LONG_TURN_ALERT_REASON_CODE.trim().is_empty());
        // The reason code is the message_outbox dedupe identity kind; with a
        // reason_code present the dedupe key ignores the (varying) rendered
        // content, so two different alert bodies dedupe to the same row.
        let key_a = crate::services::message_outbox::dedupe_key_for_message_for_test(
            "channel:123",
            "3 turns >10m ...",
            Some(LONG_TURN_ALERT_REASON_CODE),
            Some(LONG_TURN_ALERT_SESSION_KEY),
        );
        let key_b = crate::services::message_outbox::dedupe_key_for_message_for_test(
            "channel:123",
            "9 turns >10m ... (different counts)",
            Some(LONG_TURN_ALERT_REASON_CODE),
            Some(LONG_TURN_ALERT_SESSION_KEY),
        );
        assert_eq!(
            key_a, key_b,
            "same session key + reason code must dedupe regardless of rendered counts"
        );
        assert!(key_a.is_some());
    }
}
