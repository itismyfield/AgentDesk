//! #3561 — operator monitor for relay-loss signals.
//!
//! The internal `relay_health` machinery (RelayHealthSnapshot, relay_recovery)
//! exists for *recovery*, but there was no operator-facing signal when the
//! relay-loss invariant signals spike — outages were only discovered after the
//! fact by grepping logs. This job aggregates the restart-safe
//! `observability_events` stream (the durable mirror of the relay root-cause
//! counters + offset invariant violations) over a 1-hour window and reports
//! every signal that crosses its threshold.
//!
//! Design notes (see #3561, #5993):
//!   * Source of truth is the persistent `observability_events` table, NOT the
//!     in-memory atomics — atomics reset to 0 on every process restart and are
//!     per-provider scoped, which breaks delta bookkeeping across deploys.
//!   * Driven by the hourly leader `MaintenanceJob` scheduler (PG pool in
//!     hand), the same proven scheduler harness the aggregation rollup uses —
//!     not the per-provider stall watchdog (hot path, single-provider scope).
//!   * #5993 retired the Discord human-alert channel. Production never set
//!     it, so every report was discarded without a trace. A crossing is now a
//!     WARN line plus a durable `relay_signal_threshold_crossed` observability
//!     event, and an idle-cleanup preservation is a WARN line plus an
//!     `idle_cleanup_preserved` event. Neither depends on operator
//!     configuration, so neither can be silently dropped.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde_json::json;
use sqlx::PgPool;

use super::{CounterDelta, RELAY_SIGNAL_DEFINITIONS, RELAY_SIGNAL_REPEAT_WARN_SECS, RelaySignal};

/// `observability_events.event_type` for one signal over its hourly threshold.
/// `status` carries the signal key.
pub(super) const RELAY_SIGNAL_THRESHOLD_EVENT_TYPE: &str = "relay_signal_threshold_crossed";

/// `observability_events.event_type` for an idle cleanup that kept a session it
/// could not prove idle. `status` carries the preserve reason.
pub(super) const IDLE_CLEANUP_PRESERVED_EVENT_TYPE: &str = "idle_cleanup_preserved";

const IDLE_CLEANUP_PRESERVED_WARN_INTERVAL: Duration =
    Duration::from_secs(RELAY_SIGNAL_REPEAT_WARN_SECS);

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

/// Effective threshold for `signal`: the operator override when present, else
/// the conservative built-in default. A `0` override is ignored (treated as
/// "use default") so a misconfigured `relay_alert_threshold = 0` cannot turn
/// every window into a warn storm.
pub(super) fn effective_threshold(signal: &RelaySignal, override_threshold: Option<u32>) -> u32 {
    match override_threshold {
        Some(value) if value > 0 => value,
        _ => signal.default_threshold,
    }
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

pub(super) fn relay_signal_threshold_summary(
    signal: &RelaySignal,
    count: i64,
    threshold: u32,
) -> String {
    format!(
        "릴레이 누락 신호 임계 초과: `{}` ({}) 최근 1시간 {count}건 (임계 {threshold}). 운영 점검 필요.",
        signal.key, signal.label,
    )
}

/// #3561 entry point: evaluate every relay-loss signal over the trailing hour
/// and report each breached one. Returns the number of signals over their
/// threshold this cycle. Never panics; an individual signal's query failure
/// surfaces as the job error so the scheduler records it.
pub(crate) async fn report_relay_signal_threshold_crossings_pg(pool: &PgPool) -> Result<u64> {
    let override_threshold = relay_alert_threshold_override_pg(pool).await?;
    let mut counts = Vec::with_capacity(RELAY_SIGNAL_DEFINITIONS.len());
    for signal in RELAY_SIGNAL_DEFINITIONS {
        counts.push((signal, count_signal_last_hour_pg(pool, signal).await?));
    }
    Ok(report_threshold_crossings(&counts, override_threshold))
}

/// Report every signal whose hourly count reached its threshold as a WARN line
/// plus a `relay_signal_threshold_crossed` event.
fn report_threshold_crossings(
    counts: &[(&RelaySignal, i64)],
    override_threshold: Option<u32>,
) -> u64 {
    let mut crossed = 0u64;
    for &(signal, count) in counts {
        let threshold = effective_threshold(signal, override_threshold);
        if count < i64::from(threshold) {
            continue;
        }
        crossed = crossed.saturating_add(1);
        let summary = relay_signal_threshold_summary(signal, count, threshold);
        tracing::warn!(
            signal = signal.key,
            count,
            threshold,
            "[relay-signal] relay-loss signal crossed threshold: {summary}"
        );
        super::emit::emit_event(
            RELAY_SIGNAL_THRESHOLD_EVENT_TYPE,
            None,
            None,
            None,
            None,
            None,
            Some(signal.key),
            CounterDelta::default(),
            json!({
                "signal": signal.key,
                "label": signal.label,
                "count": count,
                "threshold": threshold,
                "window_secs": 3600,
            }),
        );
    }
    crossed
}

pub(super) fn idle_cleanup_preserved_summary(
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

/// When the last idle-cleanup WARN went out, and how many preservations were
/// recorded as events only since. One global slot, no per-session state: the
/// 5-minute idle-kill tick can re-preserve any number of sessions, but the log
/// gets at most one line per interval. Events are the full record.
static PRESERVED_WARN: Mutex<(Option<Instant>, u64)> = Mutex::new((None, 0));

/// #5993: idle cleanup kept a session it could not prove idle. Always records an
/// `idle_cleanup_preserved` event; warns at most once per interval process-wide.
pub(crate) fn record_idle_cleanup_preserved(
    session_key: &str,
    channel: &str,
    preserved_reason: &str,
    unobserved_minutes: Option<u64>,
) {
    let now = Instant::now();
    let (key, reason, minutes) = (session_key, preserved_reason, unobserved_minutes);
    record_idle_cleanup_preserved_with(&PRESERVED_WARN, now, key, channel, reason, minutes);
}

fn record_idle_cleanup_preserved_with(
    last_warn: &Mutex<(Option<Instant>, u64)>,
    now: Instant,
    session_key: &str,
    channel: &str,
    preserved_reason: &str,
    unobserved_minutes: Option<u64>,
) {
    let suppressed = {
        let mut last = last_warn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match last.0 {
            Some(at)
                if now.saturating_duration_since(at) < IDLE_CLEANUP_PRESERVED_WARN_INTERVAL =>
            {
                last.1 += 1;
                None
            }
            _ => Some(std::mem::replace(&mut *last, (Some(now), 0)).1),
        }
    };
    if let Some(suppressed) = suppressed {
        let summary = idle_cleanup_preserved_summary(channel, preserved_reason, unobserved_minutes);
        tracing::warn!(
            session_key,
            channel,
            preserved_reason,
            unobserved_minutes,
            suppressed_since_last_warn = suppressed,
            "[relay-signal] {summary}"
        );
    }
    super::emit::emit_event(
        IDLE_CLEANUP_PRESERVED_EVENT_TYPE,
        None,
        None,
        None,
        Some(session_key),
        None,
        Some(preserved_reason),
        CounterDelta::default(),
        json!({
            "channel": channel,
            "preserved_reason": preserved_reason,
            "unobserved_minutes": unobserved_minutes,
        }),
    );
}

/// `idle_cleanup_preserved` events recorded for `session_key` (tests only).
#[cfg(test)]
pub(crate) fn idle_cleanup_preserved_count(session_key: &str) -> usize {
    super::events::recent(10_000)
        .into_iter()
        .filter(|event| event.event_type == IDLE_CLEANUP_PRESERVED_EVENT_TYPE)
        .filter(|event| event.payload["session_key"] == session_key)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn warn_lines(captured: &Captured, needle: &str) -> Vec<String> {
        String::from_utf8(captured.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter(|line| line.trim_start().starts_with("WARN") && line.contains(needle))
            .map(str::to_string)
            .collect()
    }

    fn recent_events(
        event_type: &str,
    ) -> Vec<crate::services::observability::events::StructuredEvent> {
        crate::services::observability::events::recent(10_000)
            .into_iter()
            .filter(|event| event.event_type == event_type)
            .collect()
    }

    #[test]
    fn idle_cleanup_preserved_summary_is_one_line() {
        let summary = idle_cleanup_preserved_summary("adk-cc", "transcript_unresolved", Some(435));
        assert!(!summary.contains('\n'));
        for part in ["adk-cc", "transcript_unresolved", "7시간 15분"] {
            assert!(summary.contains(part), "{summary}");
        }
        assert!(idle_cleanup_preserved_summary("c", "r", None).contains("알 수 없음"));
    }

    /// #5993: every idle-cleanup preservation is an `idle_cleanup_preserved`
    /// event, but a burst of them logs one WARN line per interval, not one each.
    #[test]
    fn idle_cleanup_preserved_burst_warns_once_per_interval_and_records_every_event() {
        let _runtime = crate::services::observability::test_runtime_lock();
        let (captured, dispatch) = capture_logs();
        let _guard = tracing::dispatcher::set_default(&dispatch);
        let (last_warn, t0) = (Mutex::new((None, 0)), Instant::now());
        let session_key = format!("host:AgentDesk-claude-{}", uuid::Uuid::new_v4().simple());
        let record = |now| {
            let reason = "transcript_unresolved";
            record_idle_cleanup_preserved_with(
                &last_warn,
                now,
                &session_key,
                "adk-cc",
                reason,
                Some(435),
            );
        };
        for i in 0..50 {
            record(t0 + Duration::from_secs(i * 60));
        }

        let warns = warn_lines(&captured, &session_key);
        assert_eq!(warns.len(), 1, "{warns:?}");
        for part in [
            "idle 자동 정리 보류",
            "adk-cc",
            "transcript_unresolved",
            "7시간 15분",
        ] {
            assert!(warns[0].contains(part), "{warns:?}");
        }
        assert_eq!(idle_cleanup_preserved_count(&session_key), 50);

        record(t0 + Duration::from_secs(50 * 60) + IDLE_CLEANUP_PRESERVED_WARN_INTERVAL);
        let warns = warn_lines(&captured, &session_key);
        assert_eq!(warns.len(), 2, "{warns:?}");
        assert!(
            warns[1].contains("suppressed_since_last_warn=49"),
            "{warns:?}"
        );
        let events = recent_events(IDLE_CLEANUP_PRESERVED_EVENT_TYPE);
        let event = events
            .iter()
            .find(|event| event.payload["session_key"] == session_key.as_str());
        let payload = &event.expect("preservation event").payload;
        assert_eq!(payload["status"], "transcript_unresolved");
        assert_eq!(payload["channel"], "adk-cc");
        assert_eq!(payload["unobserved_minutes"], 435);
    }

    /// #5993: each crossed signal is one WARN line and one
    /// `relay_signal_threshold_crossed` event; signals under threshold are silent.
    #[test]
    fn threshold_report_warns_and_records_event_per_crossed_signal() {
        let _runtime = crate::services::observability::test_runtime_lock();
        let (captured, dispatch) = capture_logs();
        let _guard = tracing::dispatcher::set_default(&dispatch);
        let key: &'static str =
            Box::leak(format!("test_signal_{}", uuid::Uuid::new_v4().simple()).into_boxed_str());
        let under: &'static str =
            Box::leak(format!("test_signal_{}", uuid::Uuid::new_v4().simple()).into_boxed_str());
        let crossed_signal = signal(key, 3);
        let quiet_signal = signal(under, 3);

        let crossed = report_threshold_crossings(&[(&crossed_signal, 4), (&quiet_signal, 2)], None);

        assert_eq!(crossed, 1);
        let warns = warn_lines(&captured, key);
        assert_eq!(warns.len(), 1, "{warns:?}");
        assert!(warns[0].contains("crossed threshold"), "{warns:?}");
        assert!(warn_lines(&captured, under).is_empty());
        let events: Vec<_> = recent_events(RELAY_SIGNAL_THRESHOLD_EVENT_TYPE)
            .into_iter()
            .filter(|event| event.payload["status"] == key)
            .collect();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].payload["count"], 4);
        assert_eq!(events[0].payload["threshold"], 3);
        assert!(
            recent_events(RELAY_SIGNAL_THRESHOLD_EVENT_TYPE)
                .iter()
                .all(|event| event.payload["status"] != under)
        );
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
            "a 0 override must not turn every window into a warn storm"
        );
    }

    #[test]
    fn threshold_summary_names_the_signal_and_counts() {
        let sig = signal("relay_terminal_ack_timeout", 5);
        let content = relay_signal_threshold_summary(&sig, 7, 5);
        assert!(content.contains("relay_terminal_ack_timeout"));
        assert!(content.contains('7'));
        assert!(content.contains('5'));
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
