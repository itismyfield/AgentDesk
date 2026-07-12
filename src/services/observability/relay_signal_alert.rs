//! #3561 — operator monitor + Discord alert for relay-loss signals.
//!
//! The internal `relay_health` machinery (RelayHealthSnapshot, relay_recovery)
//! exists for *recovery*, but there was no operator-facing alert when the
//! relay-loss invariant signals spike — outages were only discovered after the
//! fact by grepping logs. This job aggregates the restart-safe
//! `observability_events` stream (the durable mirror of the relay root-cause
//! counters + offset invariant violations) over a 1-hour window and enqueues a
//! single de-duplicated Discord alert per signal per hour when a signal crosses
//! its threshold.
//!
//! Design notes (see #3561):
//!   * Source of truth is the persistent `observability_events` table, NOT the
//!     in-memory atomics — atomics reset to 0 on every process restart and are
//!     per-provider scoped, which breaks delta bookkeeping across deploys.
//!   * Driven by the hourly `MaintenanceJob` scheduler (PG pool in hand), the
//!     same proven scheduler harness the aggregation rollup uses — not the per-provider
//!     stall watchdog (hot path, single-provider scope).
//!   * Anti-spam is a TOCTOU-safe kv_meta dedupe-slot claim keyed by
//!     `relay_alert:{signal}:{hour_bucket}` with a
//!     1-hour TTL so each signal alerts at most once per hour.
//!   * Delivery reuses the existing `message_outbox` enqueue path with the
//!     announce bot because a threshold breach is operator-actionable. The
//!     shared #4449 worker policy falls back to notify only if announce delivery
//!     fails; cooldown and target off-switches still bound turn creation.
//!   * Double off-switch: the alert target (`kanban_human_alert_channel_id`)
//!     being unset short-circuits to 0 alerts, so an unconfigured deploy is
//!     guaranteed never to spam.

use anyhow::{Result, anyhow};
use sqlx::PgPool;

use super::{RELAY_SIGNAL_ALERT_DEDUPE_TTL_SECS, RELAY_SIGNAL_DEFINITIONS, RelaySignal};

fn normalize_channel_target(channel: &str) -> Option<String> {
    let channel = channel.trim();
    if channel.is_empty() {
        return None;
    }
    Some(if channel.starts_with("channel:") {
        channel.to_string()
    } else {
        format!("channel:{channel}")
    })
}

/// Resolve the operator alert target. Reuses the same kv_meta key the
/// agent-quality alert pipeline uses (`kanban_human_alert_channel_id`) so a
/// single operator config drives both. `None` ⇒ the job short-circuits and
/// never enqueues, guaranteeing an unconfigured deploy stays silent.
async fn relay_alert_target_pg(pool: &PgPool) -> Result<Option<String>> {
    let value = sqlx::query_scalar::<_, String>(
        "SELECT value
         FROM kv_meta
         WHERE key = 'kanban_human_alert_channel_id'
           AND value IS NOT NULL
           AND btrim(value) <> ''
         LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow!("load relay signal alert target: {error}"))?;
    Ok(value.as_deref().and_then(normalize_channel_target))
}

/// Read the operator threshold override from kv_meta (mirrored from
/// `config.kanban.relay_alert_threshold` by `services::settings`). A non-numeric
/// or absent value yields `None`, so each signal falls back to its conservative
/// built-in default. Defensive parse: legacy/operator-written junk never breaks
/// the job — it just means "use the defaults".
async fn relay_alert_threshold_override_pg(pool: &PgPool) -> Result<Option<u32>> {
    let value = sqlx::query_scalar::<_, String>(
        "SELECT value
         FROM kv_meta
         WHERE key = 'kanban_relay_alert_threshold'
           AND value IS NOT NULL
           AND btrim(value) <> ''
         LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow!("load relay alert threshold override: {error}"))?;
    Ok(value.and_then(|raw| raw.trim().parse::<u32>().ok()))
}

/// The hourly bucket a `now_ms` timestamp falls into. Stable within the hour so
/// repeated job ticks inside the same window resolve to the same dedupe key.
fn hour_bucket(now_ms: i64) -> i64 {
    now_ms.div_euclid(3_600_000)
}

/// Dedupe key for one signal in one hourly window. Mirrors the
/// `agent_quality_alert:*` key shape so the kv_meta slot semantics match.
pub(super) fn relay_alert_dedupe_key(signal_key: &str, now_ms: i64) -> String {
    format!("relay_alert:{signal_key}:{}", hour_bucket(now_ms))
}

/// Effective threshold for `signal`: the operator override when present, else
/// the conservative built-in default. A `0` override is ignored (treated as
/// "use default") so a misconfigured `relay_alert_threshold = 0` cannot turn
/// every window into a spam storm.
pub(super) fn effective_threshold(signal: &RelaySignal, override_threshold: Option<u32>) -> u32 {
    match override_threshold {
        Some(value) if value > 0 => value,
        _ => signal.default_threshold,
    }
}

/// #3561: atomically claim the dedupe slot for `key` iff the previous claim is
/// older than `RELAY_SIGNAL_ALERT_DEDUPE_TTL_SECS`. Mirrors
/// the established alert-slot pattern — single-statement TOCTOU-safe
/// claim, defensive `^[0-9]+$` guard against legacy non-numeric kv_meta values.
async fn claim_relay_alert_slot_pg(pool: &PgPool, key: &str, now_ms: i64) -> Result<bool> {
    let dedupe_ms = RELAY_SIGNAL_ALERT_DEDUPE_TTL_SECS.saturating_mul(1000);
    let claimed = sqlx::query_scalar::<_, i32>(
        "INSERT INTO kv_meta (key, value)
         VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE
             SET value = EXCLUDED.value
             WHERE CASE
                 WHEN kv_meta.value ~ '^[0-9]+$'
                     THEN kv_meta.value::bigint + $3 <= ($2)::bigint
                 ELSE TRUE
             END
         RETURNING 1",
    )
    .bind(key)
    .bind(now_ms.to_string())
    .bind(dedupe_ms)
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow!("claim relay alert dedupe key {key}: {error}"))?;
    Ok(claimed.is_some())
}

/// Best-effort rollback of a freshly-claimed dedupe slot when the subsequent
/// outbox INSERT fails, so the next cycle can retry.
async fn release_relay_alert_slot_pg(pool: &PgPool, key: &str) -> Result<()> {
    sqlx::query("DELETE FROM kv_meta WHERE key = $1")
        .bind(key)
        .execute(pool)
        .await
        .map_err(|error| anyhow!("release relay alert dedupe key {key}: {error}"))?;
    Ok(())
}

/// Count the rows for one signal inside the trailing 1-hour window. Uses the
/// indexed `created_at` column plus the `event_type` / `status` filters.
async fn count_signal_last_hour_pg(pool: &PgPool, signal: &RelaySignal) -> Result<i64> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::bigint
         FROM observability_events
         WHERE event_type = $1
           AND status = ANY($2)
           AND created_at >= NOW() - INTERVAL '1 hour'",
    )
    .bind(signal.event_type)
    .bind(signal.statuses)
    .fetch_one(pool)
    .await
    .map_err(|error| anyhow!("count relay signal {}: {error}", signal.key))?;
    Ok(count)
}

pub(super) fn relay_alert_content(signal: &RelaySignal, count: i64, threshold: u32) -> String {
    format!(
        "릴레이 누락 신호 임계 초과: `{}` ({}) 최근 1시간 {count}건 (임계 {threshold}). 운영 점검 필요.",
        signal.key, signal.label,
    )
}

async fn enqueue_relay_alert_pg(
    pool: &PgPool,
    target: &str,
    dedupe_key: &str,
    content: &str,
    now_ms: i64,
) -> Result<bool> {
    // Claim the dedupe slot atomically *before* enqueueing so concurrent
    // leaders cannot double-post the same signal in the same window.
    if !claim_relay_alert_slot_pg(pool, dedupe_key, now_ms).await? {
        return Ok(false);
    }

    let enqueued = match crate::services::message_outbox::enqueue_outbox_pg(
        pool,
        crate::services::message_outbox::OutboxMessage {
            target,
            content,
            bot: crate::services::message_outbox::ACTIONABLE_OPS_ALERT_BOT,
            source: "relay_signal_rollup",
            reason_code: Some("relay_signal.threshold"),
            session_key: Some(dedupe_key),
        },
    )
    .await
    {
        Ok(enqueued) => enqueued,
        Err(error) => {
            if let Err(rollback_err) = release_relay_alert_slot_pg(pool, dedupe_key).await {
                tracing::warn!(
                    "[relay-signal] failed to release dedupe slot {dedupe_key} after outbox error: {rollback_err}"
                );
            }
            return Err(anyhow!("enqueue relay signal alert: {error}"));
        }
    };

    Ok(enqueued)
}

/// #3561 entry point: evaluate every relay-loss signal over the trailing hour
/// and enqueue one de-duplicated operator alert per breached signal. Returns
/// the number of alerts enqueued this cycle (0 when no target is configured or
/// no signal breached its threshold). Never panics; an individual signal's
/// failure surfaces as the job error so the scheduler records it.
pub(crate) async fn enqueue_relay_signal_alerts_pg(pool: &PgPool) -> Result<u64> {
    // Off-switch #1: no operator alert target ⇒ stay completely silent.
    let Some(target) = relay_alert_target_pg(pool).await? else {
        return Ok(0);
    };

    let override_threshold = relay_alert_threshold_override_pg(pool).await?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut alert_count = 0u64;

    for signal in RELAY_SIGNAL_DEFINITIONS {
        let count = count_signal_last_hour_pg(pool, signal).await?;
        let threshold = effective_threshold(signal, override_threshold);
        if count < i64::from(threshold) {
            continue;
        }
        let dedupe_key = relay_alert_dedupe_key(signal.key, now_ms);
        let content = relay_alert_content(signal, count, threshold);
        if enqueue_relay_alert_pg(pool, &target, &dedupe_key, &content, now_ms).await? {
            alert_count = alert_count.saturating_add(1);
            tracing::warn!(
                signal = signal.key,
                count,
                threshold,
                "[relay-signal] relay-loss signal crossed threshold; operator alert enqueued"
            );
        }
    }

    Ok(alert_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(key: &'static str, default_threshold: u32) -> RelaySignal {
        RelaySignal {
            key,
            event_type: "relay_root_cause_counter",
            statuses: &["relay_terminal_ack_timeout"],
            default_threshold,
            label: "test signal",
        }
    }

    #[test]
    fn dedupe_key_is_stable_within_the_hour() {
        // Hour-aligned base so +59m stays inside the same hourly bucket.
        let base = 472_222i64 * 3_600_000;
        let a = relay_alert_dedupe_key("relay_terminal_ack_timeout", base);
        let b = relay_alert_dedupe_key("relay_terminal_ack_timeout", base + 59 * 60 * 1000);
        assert_eq!(a, b, "same hour bucket must share a dedupe key");
        assert!(a.starts_with("relay_alert:relay_terminal_ack_timeout:"));
    }

    #[test]
    fn dedupe_key_rolls_over_at_the_hour_boundary() {
        let bucket0 = 0i64;
        let bucket1 = 3_600_000i64; // exactly one hour later
        assert_ne!(
            relay_alert_dedupe_key("sig", bucket0),
            relay_alert_dedupe_key("sig", bucket1),
            "crossing the hour boundary must produce a fresh dedupe key"
        );
    }

    #[test]
    fn dedupe_key_is_per_signal() {
        let now = 1_700_000_000_000i64;
        assert_ne!(
            relay_alert_dedupe_key("relay_owner_unknown", now),
            relay_alert_dedupe_key("relay_terminal_ack_timeout", now),
            "distinct signals must not share a dedupe slot in the same hour"
        );
    }

    #[test]
    fn effective_threshold_prefers_positive_override() {
        let sig = signal("s", 5);
        assert_eq!(effective_threshold(&sig, Some(2)), 2);
    }

    #[test]
    fn effective_threshold_ignores_zero_and_missing_override() {
        let sig = signal("s", 5);
        assert_eq!(
            effective_threshold(&sig, None),
            5,
            "absent override falls back to the conservative default"
        );
        assert_eq!(
            effective_threshold(&sig, Some(0)),
            5,
            "a 0 override must not turn every window into a spam storm"
        );
    }

    #[test]
    fn alert_content_names_the_signal_and_counts() {
        let sig = signal("relay_terminal_ack_timeout", 5);
        let content = relay_alert_content(&sig, 7, 5);
        assert!(content.contains("relay_terminal_ack_timeout"));
        assert!(content.contains('7'));
        assert!(content.contains('5'));
    }

    #[test]
    fn channel_target_normalization() {
        assert_eq!(
            normalize_channel_target("123").as_deref(),
            Some("channel:123")
        );
        assert_eq!(
            normalize_channel_target("channel:123").as_deref(),
            Some("channel:123")
        );
        assert_eq!(normalize_channel_target("   ").as_deref(), None);
        assert_eq!(normalize_channel_target("").as_deref(), None);
    }

    /// The canonical signal table must cover every relay-loss vector #3561
    /// scopes: the three relay root-cause counters, offset invariant
    /// violations, and fail-closed ambiguous task responses. A missing entry
    /// silently drops a signal from the operator monitor, so guard membership.
    #[test]
    fn signal_table_covers_documented_relay_loss_vectors() {
        let keys: Vec<&str> = RELAY_SIGNAL_DEFINITIONS.iter().map(|s| s.key).collect();
        for expected in [
            "relay_terminal_ack_timeout",
            "relay_uncommitted_inflight_cleared",
            "relay_owner_unknown",
            "offset_invariant_violation",
            "task_response_chunk_ambiguous",
            "task_card_post_ambiguous",
        ] {
            assert!(
                keys.contains(&expected),
                "relay signal table must monitor {expected}; present: {keys:?}"
            );
        }
    }

    /// #3579: the operator alert table must NEVER count the watcher-owned
    /// `frame_ack_outcome` non-attempt as a relay-loss signal. `NotAttempted`
    /// (the session-bound ack-wait was intentionally SKIPPED because the watcher
    /// owns terminal delivery) is a BENIGN steady-state, distinct from the real
    /// `MissingTarget` failure. Wiring either the raw enum debug string
    /// (`NotAttempted`) or a generic `frame_ack`/`missing_target` counter into a
    /// signal here would resurrect the false-positive relay-loss tally #3579
    /// fixes (the ~2307/month phantom misses). Guard the exclusion explicitly so
    /// a future alert-table edit cannot silently re-conflate them.
    #[test]
    fn signal_table_excludes_benign_watcher_owned_non_attempt() {
        let mut keys: Vec<&str> = RELAY_SIGNAL_DEFINITIONS.iter().map(|s| s.key).collect();
        let statuses: Vec<&str> = RELAY_SIGNAL_DEFINITIONS
            .iter()
            .flat_map(|s| s.statuses.iter().copied())
            .collect();
        keys.extend_from_slice(&statuses);
        for benign in [
            "NotAttempted",
            "not_attempted",
            "frame_ack_outcome",
            "frame_ack",
        ] {
            assert!(
                !keys.contains(&benign),
                "relay signal table must NOT count the benign watcher-owned \
                 non-attempt `{benign}` as a relay-loss signal (#3579); present: {keys:?}"
            );
        }
    }

    /// Every status string in the table must be one the emit path actually
    /// writes to `observability_events.status`, otherwise the window query
    /// counts zero forever. These mirror `emit_relay_root_cause_counter`
    /// (metrics.rs) and the offset invariants (inflight.rs / tmux.rs).
    #[test]
    fn signal_statuses_match_emit_path_names() {
        let mut statuses: Vec<&str> = RELAY_SIGNAL_DEFINITIONS
            .iter()
            .flat_map(|s| s.statuses.iter().copied())
            .collect();
        statuses.sort_unstable();
        for expected in [
            "last_offset_monotonic",
            "relay_owner_unknown",
            "relay_terminal_ack_timeout",
            "relay_uncommitted_inflight_cleared",
            "response_sent_offset_monotonic",
            "task_response_chunk_delivery_ambiguous",
        ] {
            assert!(
                statuses.contains(&expected),
                "status `{expected}` must be monitored; present: {statuses:?}"
            );
        }
    }
}
