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
//!     being unset yields 0 alerts, so an unconfigured deploy is guaranteed
//!     never to spam the channel.
//!   * #5993: an unset target must not make the drop itself invisible. Every
//!     alert discarded for want of a target is counted as an
//!     `operator_alert_dropped` observability event (status = reason_code), and
//!     one warn per reason_code per `RELAY_SIGNAL_ALERT_DEDUPE_TTL_SECS` names
//!     the missing setting, so the 5-minute idle-cleanup tick cannot flood logs.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde_json::json;
use sqlx::PgPool;

use super::{
    CounterDelta, RELAY_SIGNAL_ALERT_DEDUPE_TTL_SECS, RELAY_SIGNAL_DEFINITIONS, RelaySignal,
};

/// kv_meta key holding the operator alert channel. Named in the drop warn so the
/// log line says exactly which setting would have delivered the alert.
pub(super) const ALERT_TARGET_SETTING_KEY: &str = "kanban_human_alert_channel_id";

/// `observability_events.event_type` for an alert discarded because no target
/// is configured. `status` carries the discarded alert's reason_code, so
/// `COUNT(*) ... GROUP BY status` is the per-reason drop count.
pub(super) const ALERT_DROPPED_EVENT_TYPE: &str = "operator_alert_dropped";

const RELAY_SIGNAL_THRESHOLD_REASON_CODE: &str = "relay_signal.threshold";

const ALERT_DROP_WARN_INTERVAL: Duration =
    Duration::from_secs(RELAY_SIGNAL_ALERT_DEDUPE_TTL_SECS as u64);

#[derive(Debug, Default)]
struct AlertDropTally {
    total: u64,
    last_warned_at: Option<Instant>,
    suppressed_since_warn: u64,
}

/// Per-reason_code drop tally for this process. The count grows on every drop;
/// the warn is due at most once per `ALERT_DROP_WARN_INTERVAL` per reason_code.
#[derive(Debug, Default)]
struct AlertDropLedger {
    by_reason: HashMap<String, AlertDropTally>,
}

#[derive(Debug, PartialEq, Eq)]
struct AlertDropNote {
    total: u64,
    /// `Some(n)` when a warn is due now; `n` drops were suppressed since the last one.
    warn_due: Option<u64>,
}

impl AlertDropLedger {
    fn note(&mut self, reason_code: &str, now: Instant) -> AlertDropNote {
        let tally = self.by_reason.entry(reason_code.to_string()).or_default();
        tally.total = tally.total.saturating_add(1);
        let due = tally.last_warned_at.is_none_or(|warned_at| {
            now.saturating_duration_since(warned_at) >= ALERT_DROP_WARN_INTERVAL
        });
        let warn_due = if due {
            tally.last_warned_at = Some(now);
            Some(std::mem::take(&mut tally.suppressed_since_warn))
        } else {
            tally.suppressed_since_warn = tally.suppressed_since_warn.saturating_add(1);
            None
        };
        AlertDropNote {
            total: tally.total,
            warn_due,
        }
    }
}

fn alert_drop_ledger() -> &'static Mutex<AlertDropLedger> {
    static LEDGER: OnceLock<Mutex<AlertDropLedger>> = OnceLock::new();
    LEDGER.get_or_init(|| Mutex::new(AlertDropLedger::default()))
}

/// #5993: record one operator alert discarded because `ALERT_TARGET_SETTING_KEY`
/// is unset. Always counts (observability event + process tally); warns at most
/// once per reason_code per interval.
fn note_alert_dropped_without_target(reason_code: &str, dedupe_key: &str) {
    let note = alert_drop_ledger()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .note(reason_code, Instant::now());
    if let Some(suppressed) = note.warn_due {
        tracing::warn!(
            setting_key = ALERT_TARGET_SETTING_KEY,
            reason_code,
            dropped_total = note.total,
            suppressed_since_last_warn = suppressed,
            "[relay-signal] operator alert dropped: kv_meta `{ALERT_TARGET_SETTING_KEY}` is unset, so `{reason_code}` alerts are discarded; each drop is recorded as `{ALERT_DROPPED_EVENT_TYPE}`"
        );
    }
    super::emit::emit_event(
        ALERT_DROPPED_EVENT_TYPE,
        None,
        None,
        None,
        Some(dedupe_key),
        None,
        Some(reason_code),
        CounterDelta::default(),
        json!({
            "reason_code": reason_code,
            "missing_setting": ALERT_TARGET_SETTING_KEY,
            "dedupe_key": dedupe_key,
            "dropped_total_this_process": note.total,
        }),
    );
}

#[cfg(test)]
fn alert_drops_this_process(reason_code: &str) -> u64 {
    alert_drop_ledger()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .by_reason
        .get(reason_code)
        .map_or(0, |tally| tally.total)
}

#[cfg(test)]
fn reset_alert_drops_for_tests(reason_code: &str) {
    alert_drop_ledger()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .by_reason
        .remove(reason_code);
}

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
/// single operator config drives both. `None` ⇒ nothing is ever enqueued, so an
/// unconfigured deploy never posts; each discarded alert is still counted (#5993).
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
    reason_code: &str,
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
            reason_code: Some(reason_code),
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
    // Off-switch #1: no operator alert target ⇒ nothing reaches the channel, but
    // a breached signal is still recorded as a visible drop (#5993).
    let target = relay_alert_target_pg(pool).await?;

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
        let Some(target) = target.as_deref() else {
            note_alert_dropped_without_target(RELAY_SIGNAL_THRESHOLD_REASON_CODE, &dedupe_key);
            continue;
        };
        let content = relay_alert_content(signal, count, threshold);
        if enqueue_relay_alert_pg(
            pool,
            target,
            &dedupe_key,
            &content,
            now_ms,
            RELAY_SIGNAL_THRESHOLD_REASON_CODE,
        )
        .await?
        {
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

pub(super) const IDLE_CLEANUP_PRESERVED_REASON_CODE: &str = "relay_signal.idle_cleanup_preserved";

pub(super) fn idle_cleanup_preserved_alert_content(
    channel: &str,
    preserved_reason: &str,
    unobserved_minutes: Option<u64>,
) -> String {
    let duration = match unobserved_minutes {
        Some(minutes) if minutes >= 60 => format!("{}시간 {}분", minutes / 60, minutes % 60),
        Some(minutes) => format!("{minutes}분"),
        None => "알 수 없음".to_string(),
    };
    format!(
        "idle 자동 정리 보류: 채널 `{channel}` 사유 `{preserved_reason}` — 마지막 heartbeat 이후 {duration}, tmux 유지."
    )
}

/// One operator line when idle cleanup keeps a session it could not prove idle.
/// The per-session slot shares the relay alert TTL, so the 5-minute idle-kill
/// tick cannot repeat it while the session stays preserved. Without a target the
/// line is dropped visibly (#5993) and `Ok(false)` is returned.
pub(crate) async fn enqueue_idle_cleanup_preserved_alert_pg(
    pool: &PgPool,
    session_key: &str,
    channel: &str,
    preserved_reason: &str,
    unobserved_minutes: Option<u64>,
) -> Result<bool> {
    let dedupe_key = format!("relay_alert:idle_cleanup_preserved:{session_key}");
    let Some(target) = relay_alert_target_pg(pool).await? else {
        note_alert_dropped_without_target(IDLE_CLEANUP_PRESERVED_REASON_CODE, &dedupe_key);
        return Ok(false);
    };
    enqueue_relay_alert_pg(
        pool,
        &target,
        &dedupe_key,
        &idle_cleanup_preserved_alert_content(channel, preserved_reason, unobserved_minutes),
        chrono::Utc::now().timestamp_millis(),
        IDLE_CLEANUP_PRESERVED_REASON_CODE,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_cleanup_preserved_alert_is_one_actionable_line() {
        let content =
            idle_cleanup_preserved_alert_content("adk-cc", "transcript_unresolved", Some(435));
        assert!(!content.contains('\n'));
        for part in ["adk-cc", "transcript_unresolved", "7시간 15분"] {
            assert!(content.contains(part), "{content}");
        }
        assert!(idle_cleanup_preserved_alert_content("c", "r", None).contains("알 수 없음"));
        assert!(crate::services::message_outbox::is_actionable_ops_alert(
            "relay_signal_rollup",
            Some(IDLE_CLEANUP_PRESERVED_REASON_CODE)
        ));
    }

    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_logs() -> (Captured, tracing::Dispatch) {
        crate::logging::test_capture::pin_callsite_interest();
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(captured.clone())
            .finish();
        (captured, tracing::Dispatch::new(subscriber))
    }

    fn drop_warns(captured: &Captured, reason_code: &str) -> Vec<String> {
        String::from_utf8(captured.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter(|line| line.contains("operator alert dropped") && line.contains(reason_code))
            .map(str::to_string)
            .collect()
    }

    /// #5993: every drop counts, but the warn for one reason_code is due once per
    /// interval — the 5-minute idle-cleanup tick must not repeat it.
    #[test]
    fn alert_drop_ledger_counts_every_drop_and_warns_once_per_interval() {
        let mut ledger = AlertDropLedger::default();
        let t0 = Instant::now();
        let tick = Duration::from_secs(5 * 60);
        assert_eq!(
            ledger.note("r.a", t0),
            AlertDropNote {
                total: 1,
                warn_due: Some(0)
            }
        );
        for (i, expected_total) in (2..=4).enumerate() {
            assert_eq!(
                ledger.note("r.a", t0 + tick * (i as u32 + 1)),
                AlertDropNote {
                    total: expected_total,
                    warn_due: None
                },
                "a drop inside the interval counts but does not warn again"
            );
        }
        assert_eq!(
            ledger.note("r.b", t0 + tick),
            AlertDropNote {
                total: 1,
                warn_due: Some(0)
            },
            "each reason_code warns independently"
        );
        assert_eq!(
            ledger.note("r.a", t0 + ALERT_DROP_WARN_INTERVAL),
            AlertDropNote {
                total: 5,
                warn_due: Some(3)
            },
            "after the interval the warn repeats and reports what it suppressed"
        );
    }

    /// #5993: a drop names the missing setting and the discarded reason_code in
    /// one warn, and lands in the observability event stream on every call.
    #[test]
    fn alert_drop_note_warns_once_and_records_every_drop_as_event() {
        let _runtime = crate::services::observability::test_runtime_lock();
        let reason = "test.alert_drop_note";
        reset_alert_drops_for_tests(reason);
        let (captured, dispatch) = capture_logs();
        let _guard = tracing::dispatcher::set_default(&dispatch);
        let dedupe_key = format!("relay_alert:test:{}", uuid::Uuid::new_v4().simple());
        for _ in 0..3 {
            note_alert_dropped_without_target(reason, &dedupe_key);
        }

        let warns = drop_warns(&captured, reason);
        assert_eq!(warns.len(), 1, "{warns:?}");
        assert!(warns[0].trim_start().starts_with("WARN"), "{warns:?}");
        assert!(warns[0].contains(ALERT_TARGET_SETTING_KEY), "{warns:?}");
        assert_eq!(alert_drops_this_process(reason), 3);
        let events = crate::services::observability::events::recent(10_000)
            .into_iter()
            .filter(|event| {
                event.event_type == ALERT_DROPPED_EVENT_TYPE
                    && event.payload["session_key"] == dedupe_key.as_str()
            })
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 3, "every drop is an observability event");
        for event in events {
            assert_eq!(event.payload["status"], reason);
            assert_eq!(event.payload["missing_setting"], ALERT_TARGET_SETTING_KEY);
        }
    }

    /// #5993 through the real enqueue entry points: with no target, both relay
    /// alert paths drop visibly (one warn per reason, every drop counted, no
    /// outbox row); with a target, both enqueue exactly as before.
    #[tokio::test]
    async fn missing_alert_target_drops_are_visible_and_configured_target_enqueues_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_relay_alert_drop",
            "relay alert target missing drop",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        reset_alert_drops_for_tests(IDLE_CLEANUP_PRESERVED_REASON_CODE);
        reset_alert_drops_for_tests(RELAY_SIGNAL_THRESHOLD_REASON_CODE);
        // `relay_uncommitted_inflight_cleared` trips at a single occurrence.
        sqlx::query(
            "INSERT INTO observability_events (event_type, status)
             VALUES ('relay_root_cause_counter', 'relay_uncommitted_inflight_cleared')",
        )
        .execute(&pool)
        .await
        .expect("seed breached relay signal");
        let outbox_rows = || async {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*)::bigint FROM message_outbox WHERE source = 'relay_signal_rollup'",
            )
            .fetch_one(&pool)
            .await
            .expect("count relay outbox rows")
        };

        let (captured, dispatch) = capture_logs();
        let _guard = tracing::dispatcher::set_default(&dispatch);
        for tick in 0..3 {
            assert!(
                !enqueue_idle_cleanup_preserved_alert_pg(
                    &pool,
                    &format!("session-{tick}"),
                    "adk-cc",
                    "transcript_unresolved",
                    Some(10),
                )
                .await
                .expect("idle cleanup alert without target")
            );
        }
        assert_eq!(enqueue_relay_signal_alerts_pg(&pool).await.unwrap(), 0);
        assert_eq!(enqueue_relay_signal_alerts_pg(&pool).await.unwrap(), 0);

        assert_eq!(outbox_rows().await, 0, "no target ⇒ nothing reaches outbox");
        assert_eq!(
            drop_warns(&captured, IDLE_CLEANUP_PRESERVED_REASON_CODE).len(),
            1,
            "repeated idle-cleanup drops warn once"
        );
        assert_eq!(
            drop_warns(&captured, RELAY_SIGNAL_THRESHOLD_REASON_CODE).len(),
            1,
            "repeated threshold drops warn once"
        );
        assert_eq!(
            alert_drops_this_process(IDLE_CLEANUP_PRESERVED_REASON_CODE),
            3
        );
        assert_eq!(
            alert_drops_this_process(RELAY_SIGNAL_THRESHOLD_REASON_CODE),
            2
        );

        sqlx::query("INSERT INTO kv_meta (key, value) VALUES ($1, '555')")
            .bind(ALERT_TARGET_SETTING_KEY)
            .execute(&pool)
            .await
            .expect("seed alert target");
        assert!(
            enqueue_idle_cleanup_preserved_alert_pg(
                &pool,
                "session-0",
                "adk-cc",
                "transcript_unresolved",
                Some(10),
            )
            .await
            .unwrap()
        );
        assert_eq!(enqueue_relay_signal_alerts_pg(&pool).await.unwrap(), 1);
        let targets = sqlx::query_scalar::<_, String>(
            "SELECT target FROM message_outbox WHERE source = 'relay_signal_rollup'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(targets, vec!["channel:555", "channel:555"]);
        assert_eq!(
            alert_drops_this_process(IDLE_CLEANUP_PRESERVED_REASON_CODE),
            3,
            "a configured target is not a drop"
        );
        assert_eq!(
            alert_drops_this_process(RELAY_SIGNAL_THRESHOLD_REASON_CODE),
            2
        );
        drop(_guard);
        pool.close().await;
        pg_db.drop().await;
    }

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
            // #5941: the two halves of one loss — a frame with no delivery
            // owner, and that frame left without even a durable record.
            "relay_terminal_authority_denied",
            "terminal_frame_without_owner_or_record",
            // #5996 I20: a retirement decision taken with neither a witness nor
            // a measured tail to read.
            "retirement_without_progress_witness",
        ] {
            assert!(
                keys.contains(&expected),
                "relay signal table must monitor {expected}; present: {keys:?}"
            );
        }
    }

    /// #5948 (DoD): the resend-suppression counter must have a real CONSUMER, not
    /// just a producer. `relay_terminal_authority_denied` is the cautionary case
    /// #5941 surfaced — emitted for months with nothing reading it. Membership in
    /// this table is what makes the hourly #3561 operator alert read it, so guard
    /// both the key and the status string the emit path actually writes.
    #[test]
    fn signal_table_monitors_rewind_resend_suppression() {
        let signal = RELAY_SIGNAL_DEFINITIONS
            .iter()
            .find(|s| s.key == "relay_resend_suppressed")
            .expect("the #5948 resend-suppression counter must be monitored");
        assert_eq!(
            signal.event_type, "relay_root_cause_counter",
            "the parser emits it through `emit_relay_root_cause_counter`"
        );
        assert!(
            signal.statuses.contains(&"relay_resend_suppressed"),
            "the monitored status must equal the emitted one, or the window query \
             counts zero forever; present: {:?}",
            signal.statuses
        );
        assert!(
            signal.default_threshold > 1,
            "a rewind resend is recoverable and absorbed — paging on a single one \
             would make the signal noise instead of a root cause"
        );
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
            // #5941: `metrics::record_relay_terminal_authority_denied` writes
            // this status, and `orphan_terminal_frame` writes the invariant one.
            "relay_terminal_authority_denied",
            "terminal_frame_has_a_delivery_owner_or_a_record",
            // #5996: `stale_reclaim` writes this one, reading the SAME symbol
            // the table does — so this assertion guards the row's presence, not
            // a hand-copied string.
            super::super::LIVE_TURN_PROVEN_BY_PROGRESS_INVARIANT,
        ] {
            assert!(
                statuses.contains(&expected),
                "status `{expected}` must be monitored; present: {statuses:?}"
            );
        }
        assert!(
            !statuses.contains(&"turn_start_offset_monotonic"),
            "the alias-prone WARN-only birth observer remains queryable in observability_events but must not enter the threshold-1 operator paging table"
        );
    }
}
