use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use dashmap::DashMap;
use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::mpsc;

// Foundation observability layer introduced by #1070 (Epic #905 Phase 1).
// `metrics` → lightweight channel/provider atomic counters for hot paths.
// `events`  → bounded in-memory structured event log + periodic JSONL flush.
// `watcher_latency` → #1134 attach→first-relay latency histogram + counters.
// `turn_lifecycle` → durable turn lifecycle event model for session/context transitions.
pub mod events;
pub mod metrics;
pub mod recovery_audit;
pub mod session_inventory;
pub mod turn_lifecycle;
pub mod watcher_latency;

// #2049: mod.rs was a 3,946-line monolith. Splitting along responsibility
// boundaries (helpers / emit / worker / retention / pg I/O / queries) without
// changing the public API. Regression alerting is intentionally owned only by
// `services::agent_quality::regression_alerts`. Global state (`OnceLock`
// runtime) stays in this module to avoid relocating the singleton.
mod emit;
mod helpers;
mod pg_io;
mod queries;
mod relay_signal_alert;
mod retention;
mod worker;

// Public surface re-exports — keep `crate::services::observability::*`
// import paths working unchanged. `#[allow(unused_imports)]` because some
// of these are only consumed by narrow test modules or by downstream crates
// that ship outside the default build profile.
#[allow(unused_imports)]
pub use emit::{
    InvariantSeverity, emit_agent_quality_event, emit_bridge_latency_spans, emit_dispatch_result,
    emit_guard_fired, emit_inflight_lifecycle_event, emit_intake_latency_spans,
    emit_intake_placeholder_post_failed, emit_recovery_fired, emit_relay_delete,
    emit_relay_delete_result, emit_relay_delivery, emit_turn_cancelled,
    emit_turn_finished_with_dispatch_kind, emit_turn_started, emit_watcher_replaced,
    record_invariant_check, record_invariant_check_with_severity,
};
#[allow(unused_imports)]
pub use queries::{
    query_agent_quality_ranking_with, query_agent_quality_summary, run_agent_quality_rollup_pg,
};
// #3561 — operator relay-loss signal monitor. Driven by the hourly
// `RelaySignalAlerterJob` maintenance job (see `server::maintenance`).
pub(crate) use relay_signal_alert::enqueue_relay_signal_alerts_pg;

pub(super) const EVENT_BATCH_SIZE: usize = 64;
pub(super) const EVENT_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
pub(super) const SNAPSHOT_FLUSH_INTERVAL: Duration = Duration::from_secs(15);
// #2049 Finding 9: hourly retention sweep on observability tables. Defaults
// are conservative; tunable via `ADK_OBSERVABILITY_*_RETENTION_DAYS` env.
pub(super) const RETENTION_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);
pub(super) const DEFAULT_OBSERVABILITY_EVENT_RETENTION_DAYS: i64 = 90;
pub(super) const DEFAULT_QUALITY_EVENT_RETENTION_DAYS: i64 = 90;
pub(super) const DEFAULT_COUNTER_SNAPSHOT_RETENTION_DAYS: i64 = 7;
pub(super) const DEFAULT_COUNTER_LIMIT: usize = 200;
pub(super) const MAX_COUNTER_LIMIT: usize = 500;
pub(super) const DEFAULT_QUALITY_DAYS: i64 = 7;
pub(super) const MAX_QUALITY_DAYS: i64 = 365;
pub(super) const DEFAULT_QUALITY_DAILY_LIMIT: usize = 60;
pub(super) const MAX_QUALITY_DAILY_LIMIT: usize = 180;
pub(super) const DEFAULT_QUALITY_RANKING_LIMIT: usize = 50;
pub(super) const MAX_QUALITY_RANKING_LIMIT: usize = 200;
pub(super) const QUALITY_SAMPLE_GUARD: i64 = 5;

// #3561 — relay-loss operator monitor. Each signal alerts at most once per
// hour (`RELAY_SIGNAL_ALERT_DEDUPE_TTL_SECS`), so the dedupe window matches the
// job cadence. The per-signal `default_threshold` values are conservative:
//   * `relay_terminal_ack_timeout` (duplicate-emit vector) tolerates a few per
//     hour before it's worth waking an operator.
//   * `relay_owner_unknown` (relay started with unknown owner) is rarer and
//     more suspicious, so a lower bar.
//   * `relay_uncommitted_inflight_cleared` (LEAKED ANSWER — a non-empty reply
//     was dropped) and `offset_invariant_violation` (persisted bad offset
//     state) are severe enough that a single occurrence warrants an alert.
// All are overridable per-deploy via `kanban.relay_alert_threshold`
// (mirrored to kv_meta `kanban_relay_alert_threshold` by `services::settings`).
pub(super) const RELAY_SIGNAL_ALERT_DEDUPE_TTL_SECS: i64 = 60 * 60;

/// One relay-loss signal as projected onto the `observability_events` table.
/// `event_type` + `status` uniquely identify the persisted rows that count
/// toward this signal's hourly window. `default_threshold` is the conservative
/// built-in trip point (overridable per-deploy via
/// `kanban.relay_alert_threshold`).
#[derive(Debug, Clone, Copy)]
pub(super) struct RelaySignal {
    /// Stable identifier used in the dedupe key and the alert body.
    pub(super) key: &'static str,
    /// `observability_events.event_type` filter.
    pub(super) event_type: &'static str,
    /// `observability_events.status` values that map to this signal. The relay
    /// root-cause counters store the counter name in `status`
    /// (`emit_relay_root_cause_counter`); the offset invariants store the
    /// invariant name in `status` (`record_invariant_check`).
    pub(super) statuses: &'static [&'static str],
    /// Conservative built-in hourly trip threshold.
    pub(super) default_threshold: u32,
    /// Human-readable label for the operator alert.
    pub(super) label: &'static str,
}

/// Canonical relay-loss signal table monitored by the #3561 operator alert
/// job. Each entry maps 1:1 onto rows the emit path already persists to
/// `observability_events` (see `emit::emit_relay_root_cause_counter` and
/// `emit::record_invariant_check_with_severity`).
pub(super) const RELAY_SIGNAL_DEFINITIONS: &[RelaySignal] = &[
    RelaySignal {
        key: "relay_terminal_ack_timeout",
        event_type: "relay_root_cause_counter",
        statuses: &["relay_terminal_ack_timeout"],
        default_threshold: 5,
        label: "터미널 ACK 타임아웃(중복 송신 벡터)",
    },
    RelaySignal {
        key: "relay_uncommitted_inflight_cleared",
        event_type: "relay_root_cause_counter",
        statuses: &["relay_uncommitted_inflight_cleared"],
        default_threshold: 1,
        label: "미커밋 inflight 정리(답변 누출 벡터)",
    },
    RelaySignal {
        key: "relay_owner_unknown",
        event_type: "relay_root_cause_counter",
        statuses: &["relay_owner_unknown"],
        default_threshold: 3,
        label: "릴레이 owner 불명",
    },
    RelaySignal {
        key: "offset_invariant_violation",
        event_type: "invariant_violation",
        statuses: &["last_offset_monotonic", "response_sent_offset_monotonic"],
        default_threshold: 1,
        label: "오프셋 단조성 불변식 위반",
    },
];
pub(super) const AGENT_QUALITY_EVENT_TYPES: &[&str] = &[
    "turn_start",
    "turn_complete",
    "turn_error",
    "review_pass",
    "review_fail",
    "dispatch_dispatched",
    "dispatch_completed",
    "recovery_fired",
    "escalation",
    "card_transitioned",
    "stream_reattached",
    "watcher_lost",
    "outbox_delivery_failed",
    "ci_check_red",
    "queue_stuck",
];

pub(crate) fn live_analytics_counter_values(
    filters: &AnalyticsFilters,
    counter_limit: usize,
) -> Vec<Value> {
    let limit = helpers::normalized_counter_limit(counter_limit);
    worker::snapshot_rows(&runtime(), Some(filters))
        .into_iter()
        .take(limit)
        .filter_map(|row| {
            let snapshot = helpers::counter_snapshot_from_values(
                &row.provider,
                &row.channel_id,
                row.values,
                "live",
                helpers::now_kst(),
            );
            serde_json::to_value(snapshot).ok()
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct CounterKey {
    pub(super) provider: String,
    pub(super) channel_id: String,
}

#[derive(Debug, Default)]
pub(super) struct CounterBucket {
    turn_attempts: AtomicU64,
    guard_fires: AtomicU64,
    watcher_replacements: AtomicU64,
    recovery_fires: AtomicU64,
    turn_successes: AtomicU64,
    turn_failures: AtomicU64,
}

impl CounterBucket {
    pub(super) fn apply(&self, delta: CounterDelta) {
        if delta.turn_attempts > 0 {
            self.turn_attempts
                .fetch_add(delta.turn_attempts, Ordering::Relaxed);
        }
        if delta.guard_fires > 0 {
            self.guard_fires
                .fetch_add(delta.guard_fires, Ordering::Relaxed);
        }
        if delta.watcher_replacements > 0 {
            self.watcher_replacements
                .fetch_add(delta.watcher_replacements, Ordering::Relaxed);
        }
        if delta.recovery_fires > 0 {
            self.recovery_fires
                .fetch_add(delta.recovery_fires, Ordering::Relaxed);
        }
        if delta.turn_successes > 0 {
            self.turn_successes
                .fetch_add(delta.turn_successes, Ordering::Relaxed);
        }
        if delta.turn_failures > 0 {
            self.turn_failures
                .fetch_add(delta.turn_failures, Ordering::Relaxed);
        }
    }

    pub(super) fn snapshot(&self) -> CounterValues {
        CounterValues {
            turn_attempts: self.turn_attempts.load(Ordering::Relaxed),
            guard_fires: self.guard_fires.load(Ordering::Relaxed),
            watcher_replacements: self.watcher_replacements.load(Ordering::Relaxed),
            recovery_fires: self.recovery_fires.load(Ordering::Relaxed),
            turn_successes: self.turn_successes.load(Ordering::Relaxed),
            turn_failures: self.turn_failures.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct CounterDelta {
    pub(super) turn_attempts: u64,
    pub(super) guard_fires: u64,
    pub(super) watcher_replacements: u64,
    pub(super) recovery_fires: u64,
    pub(super) turn_successes: u64,
    pub(super) turn_failures: u64,
}

impl CounterDelta {
    pub(super) fn is_zero(self) -> bool {
        self.turn_attempts == 0
            && self.guard_fires == 0
            && self.watcher_replacements == 0
            && self.recovery_fires == 0
            && self.turn_successes == 0
            && self.turn_failures == 0
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct QueuedEvent {
    pub(super) event_type: String,
    pub(super) provider: Option<String>,
    pub(super) channel_id: Option<String>,
    pub(super) dispatch_id: Option<String>,
    pub(super) session_key: Option<String>,
    pub(super) turn_id: Option<String>,
    pub(super) status: Option<String>,
    pub(super) payload_json: String,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct QueuedQualityEvent {
    pub(super) source_event_id: Option<String>,
    pub(super) correlation_id: Option<String>,
    pub(super) agent_id: Option<String>,
    pub(super) provider: Option<String>,
    pub(super) channel_id: Option<String>,
    pub(super) card_id: Option<String>,
    pub(super) dispatch_id: Option<String>,
    pub(super) event_type: String,
    pub(super) payload_json: String,
}

#[derive(Debug)]
pub(super) enum WorkerMessage {
    Event(QueuedEvent),
    QualityEvent(QueuedQualityEvent),
}

#[derive(Clone, Default)]
pub(super) struct StorageHandles {
    pub(super) pg_pool: Option<PgPool>,
}

#[derive(Default)]
pub(super) struct ObservabilityRuntime {
    pub(super) counters: DashMap<CounterKey, Arc<CounterBucket>>,
    pub(super) storage: Mutex<StorageHandles>,
    pub(super) sender: Mutex<Option<mpsc::UnboundedSender<WorkerMessage>>>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct CounterValues {
    pub(super) turn_attempts: u64,
    pub(super) guard_fires: u64,
    pub(super) watcher_replacements: u64,
    pub(super) recovery_fires: u64,
    pub(super) turn_successes: u64,
    pub(super) turn_failures: u64,
}

#[derive(Debug, Clone)]
pub(super) struct SnapshotRow {
    pub(super) provider: String,
    pub(super) channel_id: String,
    pub(super) values: CounterValues,
}

#[derive(Debug, Clone, Default)]
pub struct AnalyticsFilters {
    pub provider: Option<String>,
    pub channel_id: Option<String>,
    pub event_type: Option<String>,
    pub event_limit: usize,
    pub counter_limit: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnalyticsCounterSnapshot {
    pub provider: String,
    pub channel_id: String,
    pub turn_attempts: u64,
    pub guard_fires: u64,
    pub watcher_replacements: u64,
    pub recovery_fires: u64,
    pub turn_successes: u64,
    pub turn_failures: u64,
    /// #2049 Finding 20: `Option<f64>` so consumers (dashboards) can tell
    /// "no observations yet" apart from "0% success". Before this change
    /// `turn_attempts.max(1)` divided successes by 1 whenever attempts=0,
    /// which made a stray success on a never-started counter render as
    /// "100% success rate of 0 attempts".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_rate: Option<f64>,
    pub snapshot_at: String,
    pub source: String,
}

#[derive(Debug, Clone, Default)]
pub struct InvariantAnalyticsFilters {
    pub provider: Option<String>,
    pub channel_id: Option<String>,
    pub invariant: Option<String>,
    pub limit: usize,
}

pub struct InvariantViolation<'a> {
    pub provider: Option<&'a str>,
    pub channel_id: Option<u64>,
    pub dispatch_id: Option<&'a str>,
    pub session_key: Option<&'a str>,
    pub turn_id: Option<&'a str>,
    pub invariant: &'a str,
    pub code_location: &'static str,
    pub message: &'a str,
    pub details: Value,
}

#[derive(Debug, Clone)]
pub struct AgentQualityEvent {
    pub source_event_id: Option<String>,
    pub correlation_id: Option<String>,
    pub agent_id: Option<String>,
    pub provider: Option<String>,
    pub channel_id: Option<String>,
    pub card_id: Option<String>,
    pub dispatch_id: Option<String>,
    pub event_type: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Default)]
pub struct AgentQualityFilters {
    pub agent_id: Option<String>,
    pub days: i64,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentQualityWindow {
    pub days: i64,
    pub sample_size: i64,
    pub measurement_unavailable: bool,
    pub measurement_label: Option<String>,
    pub turn_sample_size: i64,
    pub turn_success_rate: Option<f64>,
    pub review_sample_size: i64,
    pub review_pass_rate: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentQualityDailyRecord {
    pub agent_id: String,
    pub day: String,
    pub provider: Option<String>,
    pub channel_id: Option<String>,
    pub turn_success_count: i64,
    pub turn_error_count: i64,
    pub review_pass_count: i64,
    pub review_fail_count: i64,
    pub turn_sample_size: i64,
    pub review_sample_size: i64,
    pub sample_size: i64,
    pub turn_success_rate: Option<f64>,
    pub review_pass_rate: Option<f64>,
    pub rolling_7d: AgentQualityWindow,
    pub rolling_30d: AgentQualityWindow,
    pub computed_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentQualitySummary {
    pub generated_at: String,
    pub agent_id: String,
    pub latest: Option<AgentQualityDailyRecord>,
    /// Alias for `latest` — DoD-mandated field name (#1102).
    pub current: Option<AgentQualityDailyRecord>,
    pub daily: Vec<AgentQualityDailyRecord>,
    /// Last 7 days of daily rows (newest-first), DoD-mandated (#1102).
    pub trend_7d: Vec<AgentQualityDailyRecord>,
    /// Last 30 days of daily rows (newest-first), DoD-mandated (#1102).
    pub trend_30d: Vec<AgentQualityDailyRecord>,
    /// True when `daily` is synthesized from `agent_quality_event` because
    /// the daily rollup table was empty — see #1102 fallback path.
    pub fallback_from_events: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentQualityRankingEntry {
    pub rank: i64,
    pub agent_id: String,
    pub agent_name: Option<String>,
    pub provider: Option<String>,
    pub channel_id: Option<String>,
    pub latest_day: String,
    pub rolling_7d: AgentQualityWindow,
    pub rolling_30d: AgentQualityWindow,
    /// The chosen metric value for the requested (metric, window) pair
    /// — populated when the ranking endpoint is called with `metric`/`window`
    /// query params. `None` when the agent has no available measurement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metric_value: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityRankingMetric {
    TurnSuccessRate,
    ReviewPassRate,
}

impl QualityRankingMetric {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("review_pass_rate") | Some("review") => Self::ReviewPassRate,
            _ => Self::TurnSuccessRate,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::TurnSuccessRate => "turn_success_rate",
            Self::ReviewPassRate => "review_pass_rate",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityRankingWindow {
    Seven,
    Thirty,
}

impl QualityRankingWindow {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("30d") | Some("30") => Self::Thirty,
            _ => Self::Seven,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Seven => "7d",
            Self::Thirty => "30d",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentQualityRankingResponse {
    pub generated_at: String,
    pub metric: String,
    pub window: String,
    pub min_sample_size: i64,
    pub agents: Vec<AgentQualityRankingEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentQualityRollupReport {
    pub upserted_rows: u64,
    pub alert_count: u64,
}

static OBSERVABILITY_RUNTIME: OnceLock<Arc<ObservabilityRuntime>> = OnceLock::new();

pub(super) fn runtime() -> Arc<ObservabilityRuntime> {
    OBSERVABILITY_RUNTIME
        .get_or_init(|| Arc::new(ObservabilityRuntime::default()))
        .clone()
}

pub fn init_observability(pg_pool: Option<PgPool>) {
    let runtime = runtime();
    if let Ok(mut storage) = runtime.storage.lock() {
        storage.pg_pool = pg_pool;
    }
    worker::ensure_worker(&runtime);
    // #1070: start the periodic JSONL flush task for the in-memory event log.
    events::ensure_flusher();
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    let runtime = runtime();
    runtime.counters.clear();
    events::global().clear();
    if let Ok(mut sender) = runtime.sender.lock() {
        *sender = None;
    }
    if let Ok(mut storage) = runtime.storage.lock() {
        *storage = StorageHandles::default();
    }
}

#[cfg(test)]
pub(crate) fn test_runtime_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod cancellation_observability_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn turn_cancelled_emit_records_normalized_payload_without_pg() {
        let _guard = test_runtime_lock();
        reset_for_tests();
        init_observability(None);

        emit_turn_cancelled(
            Some("Codex"),
            Some(42),
            Some("dispatch-1"),
            Some("session-1"),
            Some("turn-1"),
            turn_lifecycle::TurnCancellationDetails::new(
                " queue-api cancel_turn (preserve) ",
                crate::services::turn_lifecycle::cleanup_policy_observability_surface(
                    crate::services::discord::TmuxCleanupPolicy::PreserveSessionAndInflight {
                        restart_mode: crate::services::discord::InflightRestartMode::HotSwapHandoff,
                    },
                ),
                "mailbox_canonical",
                false,
                false,
                Some(2),
                true,
                false,
            ),
        );

        let event = events::recent(10)
            .into_iter()
            .find(|event| event.event_type == "turn_cancelled")
            .expect("turn_cancelled event should be recorded");
        assert_eq!(event.channel_id, Some(42));
        assert_eq!(event.provider.as_deref(), Some("codex"));
        assert_eq!(event.payload["reason"], "queue-api cancel_turn (preserve)");
        assert_eq!(event.payload["surface"], "queue_cancel_preserve");
        assert_eq!(event.payload["lifecyclePath"], "mailbox_canonical");
        assert_eq!(event.payload["queueDepth"], 2);
        assert_eq!(event.payload["queuePreserved"], true);
        assert!(event.payload.get("emittedNoOp").is_none());
        assert_eq!(event.payload["dispatch_id"], "dispatch-1");
        assert_eq!(event.payload["session_key"], "session-1");
        assert_eq!(event.payload["turn_id"], "turn-1");
        let _ = json!({});
    }
}
