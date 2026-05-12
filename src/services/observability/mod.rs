use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use dashmap::DashMap;
use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::{mpsc, oneshot};

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
// boundaries (helpers / emit / worker / retention / pg I/O / quality alerts
// / queries) without changing the public API. Global state (`OnceLock`
// runtime) stays in this module to avoid relocating the singleton.
mod emit;
mod helpers;
mod pg_io;
mod quality_alert;
mod queries;
mod retention;
mod worker;

// Public surface re-exports — keep `crate::services::observability::*`
// import paths working unchanged. `#[allow(unused_imports)]` because some
// of these are only consumed by the gated test module (legacy-sqlite-tests)
// or by downstream crates that ship outside the default build profile.
#[allow(unused_imports)]
pub use emit::{
    emit_agent_quality_event, emit_dispatch_result, emit_guard_fired,
    emit_inflight_lifecycle_event, emit_intake_placeholder_post_failed, emit_recovery_fired,
    emit_turn_cancelled, emit_turn_finished, emit_turn_finished_with_dispatch_kind,
    emit_turn_started, emit_watcher_replaced, record_invariant_check,
};
#[allow(unused_imports)]
pub use queries::{
    query_agent_quality_events, query_agent_quality_ranking, query_agent_quality_ranking_with,
    query_agent_quality_summary, query_analytics, query_invariant_analytics,
    run_agent_quality_rollup_pg,
};

pub(super) const EVENT_BATCH_SIZE: usize = 64;
pub(super) const EVENT_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
pub(super) const SNAPSHOT_FLUSH_INTERVAL: Duration = Duration::from_secs(15);
// #2049 Finding 9: hourly retention sweep on observability tables. Defaults
// are conservative; tunable via `ADK_OBSERVABILITY_*_RETENTION_DAYS` env.
pub(super) const RETENTION_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);
pub(super) const DEFAULT_OBSERVABILITY_EVENT_RETENTION_DAYS: i64 = 90;
pub(super) const DEFAULT_QUALITY_EVENT_RETENTION_DAYS: i64 = 90;
pub(super) const DEFAULT_COUNTER_SNAPSHOT_RETENTION_DAYS: i64 = 7;
pub(super) const DEFAULT_EVENT_LIMIT: usize = 100;
pub(super) const DEFAULT_COUNTER_LIMIT: usize = 200;
pub(super) const MAX_EVENT_LIMIT: usize = 500;
pub(super) const MAX_COUNTER_LIMIT: usize = 500;
pub(super) const DEFAULT_INVARIANT_LIMIT: usize = 50;
pub(super) const MAX_INVARIANT_LIMIT: usize = 500;
pub(super) const DEFAULT_QUALITY_LIMIT: usize = 200;
pub(super) const MAX_QUALITY_LIMIT: usize = 500;
pub(super) const DEFAULT_QUALITY_DAYS: i64 = 7;
pub(super) const MAX_QUALITY_DAYS: i64 = 365;
pub(super) const DEFAULT_QUALITY_DAILY_LIMIT: usize = 60;
pub(super) const MAX_QUALITY_DAILY_LIMIT: usize = 180;
pub(super) const DEFAULT_QUALITY_RANKING_LIMIT: usize = 50;
pub(super) const MAX_QUALITY_RANKING_LIMIT: usize = 200;
pub(super) const QUALITY_SAMPLE_GUARD: i64 = 5;
pub(super) const QUALITY_ALERT_DEDUPE_MS: i64 = 24 * 60 * 60 * 1000;
pub(super) const QUALITY_REVIEW_DROP_THRESHOLD: f64 = 0.20;
pub(super) const QUALITY_TURN_DROP_THRESHOLD: f64 = 0.15;
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
    Flush(oneshot::Sender<()>),
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
pub struct AnalyticsEventRecord {
    pub id: i64,
    pub event_type: String,
    pub provider: Option<String>,
    pub channel_id: Option<String>,
    pub dispatch_id: Option<String>,
    pub session_key: Option<String>,
    pub turn_id: Option<String>,
    pub status: Option<String>,
    pub payload: Value,
    pub created_at: String,
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

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnalyticsResponse {
    pub generated_at: String,
    pub counters: Vec<AnalyticsCounterSnapshot>,
    pub events: Vec<AnalyticsEventRecord>,
}

#[derive(Debug, Clone, Default)]
pub struct InvariantAnalyticsFilters {
    pub provider: Option<String>,
    pub channel_id: Option<String>,
    pub invariant: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InvariantViolationCount {
    pub invariant: String,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InvariantViolationRecord {
    pub id: i64,
    pub invariant: String,
    pub provider: Option<String>,
    pub channel_id: Option<String>,
    pub dispatch_id: Option<String>,
    pub session_key: Option<String>,
    pub turn_id: Option<String>,
    pub message: Option<String>,
    pub code_location: Option<String>,
    pub details: Value,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InvariantAnalyticsResponse {
    pub generated_at: String,
    pub total_violations: u64,
    pub counts: Vec<InvariantViolationCount>,
    pub recent: Vec<InvariantViolationRecord>,
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
pub struct AgentQualityEventRecord {
    pub id: i64,
    pub source_event_id: Option<String>,
    pub correlation_id: Option<String>,
    pub agent_id: Option<String>,
    pub provider: Option<String>,
    pub channel_id: Option<String>,
    pub card_id: Option<String>,
    pub dispatch_id: Option<String>,
    pub event_type: String,
    pub payload: Value,
    pub created_at: String,
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) async fn flush_for_tests() {
    let Some(sender) = worker::worker_sender() else {
        return;
    };
    let (done_tx, done_rx) = oneshot::channel();
    let _ = sender.send(WorkerMessage::Flush(done_tx));
    let _ = tokio::time::timeout(Duration::from_secs(5), done_rx).await;
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn test_storage_presence() -> (bool, bool) {
    let handles = worker::storage_handles(&runtime());
    (false, handles.pg_pool.is_some())
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
        assert_eq!(event.payload["dispatch_id"], "dispatch-1");
        assert_eq!(event.payload["session_key"], "session-1");
        assert_eq!(event.payload["turn_id"], "turn-1");
        let _ = json!({});
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use serde_json::json;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    #[tokio::test]
    async fn event_flush_without_pg_keeps_live_counters() {
        let _guard = test_runtime_lock();
        reset_for_tests();
        init_observability(None);

        emit_turn_started(
            "codex",
            42,
            Some("dispatch-1"),
            Some("session-1"),
            Some("turn-1"),
        );
        emit_guard_fired(
            "codex",
            42,
            Some("dispatch-1"),
            Some("session-1"),
            Some("turn-1"),
            "review_dispatch_pending",
        );
        emit_turn_finished(
            "codex",
            42,
            Some("dispatch-1"),
            Some("session-1"),
            Some("turn-1"),
            "completed",
            321,
            false,
        );
        flush_for_tests().await;

        let response = query_analytics(
            None,
            &AnalyticsFilters {
                provider: Some("codex".to_string()),
                channel_id: Some("42".to_string()),
                ..AnalyticsFilters::default()
            },
        )
        .await
        .expect("query analytics");

        assert_eq!(response.counters.len(), 1);
        assert_eq!(response.counters[0].turn_attempts, 1);
        assert_eq!(response.counters[0].guard_fires, 1);
        assert_eq!(response.counters[0].turn_successes, 1);
        assert!(response.events.is_empty());
    }

    #[tokio::test]
    async fn invariant_true_check_does_not_record_violation() {
        let _guard = test_runtime_lock();
        reset_for_tests();
        init_observability(None);

        assert!(record_invariant_check(
            true,
            InvariantViolation {
                provider: Some("codex"),
                channel_id: Some(7),
                dispatch_id: None,
                session_key: None,
                turn_id: Some("discord:7:70"),
                invariant: "response_sent_offset_monotonic",
                code_location: "src/services/discord/turn_bridge/mod.rs:test",
                message: "known-true invariant check should not emit",
                details: json!({
                    "previous": 8,
                    "next": 12,
                }),
            },
        ));
        flush_for_tests().await;

        let error = query_invariant_analytics(
            None,
            &InvariantAnalyticsFilters {
                provider: Some("codex".to_string()),
                channel_id: Some("7".to_string()),
                invariant: Some("response_sent_offset_monotonic".to_string()),
                limit: 10,
            },
        )
        .await
        .expect_err("invariant analytics requires postgres");
        assert!(error.to_string().contains("postgres pool unavailable"));
    }

    #[tokio::test]
    async fn invariant_violation_emit_and_query_round_trip() {
        let _guard = test_runtime_lock();
        reset_for_tests();
        init_observability(None);

        assert!(!record_invariant_check(
            false,
            InvariantViolation {
                provider: Some("claude"),
                channel_id: Some(42),
                dispatch_id: Some("dispatch-invariant"),
                session_key: Some("host:session"),
                turn_id: Some("discord:42:420"),
                invariant: "inflight_tmux_one_to_one",
                code_location: "src/services/discord/inflight.rs:test",
                message: "test violation",
                details: json!({
                    "tmux_session_name": "AgentDesk-claude-test",
                }),
            },
        ));
        flush_for_tests().await;

        let error = query_invariant_analytics(
            None,
            &InvariantAnalyticsFilters {
                provider: Some("claude".to_string()),
                channel_id: Some("42".to_string()),
                invariant: Some("inflight_tmux_one_to_one".to_string()),
                limit: 10,
            },
        )
        .await
        .expect_err("invariant analytics requires postgres");
        assert!(error.to_string().contains("postgres pool unavailable"));
    }

    #[tokio::test]
    async fn agent_quality_emit_and_query_round_trip() -> anyhow::Result<()> {
        let _guard = test_runtime_lock();
        reset_for_tests();
        init_observability(None);

        emit_agent_quality_event(AgentQualityEvent {
            source_event_id: Some("turn-1".to_string()),
            correlation_id: Some("dispatch-1".to_string()),
            agent_id: Some("agent-1".to_string()),
            provider: Some("codex".to_string()),
            channel_id: Some("42".to_string()),
            card_id: Some("card-1".to_string()),
            dispatch_id: Some("dispatch-1".to_string()),
            event_type: "review_pass".to_string(),
            payload: json!({
                "verdict": "pass",
            }),
        });
        flush_for_tests().await;

        let error = query_agent_quality_events(
            None,
            &AgentQualityFilters {
                agent_id: Some("agent-1".to_string()),
                days: 7,
                limit: 10,
            },
        )
        .await
        .expect_err("agent quality events require postgres");
        assert!(error.to_string().contains("postgres pool unavailable"));

        Ok(())
    }

    #[tokio::test]
    async fn agent_quality_query_without_pg_pool_is_unavailable() -> anyhow::Result<()> {
        let _guard = test_runtime_lock();
        reset_for_tests();
        init_observability(None);

        let error = query_agent_quality_events(
            None,
            &AgentQualityFilters {
                agent_id: Some("agent-window".to_string()),
                days: 7,
                limit: 50,
            },
        )
        .await
        .expect_err("agent quality events require postgres");
        assert!(error.to_string().contains("postgres pool unavailable"));

        Ok(())
    }

    #[tokio::test]
    async fn agent_quality_unscoped_query_without_pg_pool_is_unavailable() -> anyhow::Result<()> {
        let _guard = test_runtime_lock();
        reset_for_tests();
        init_observability(None);

        let error = query_agent_quality_events(
            None,
            &AgentQualityFilters {
                agent_id: Some("agent-A".to_string()),
                days: 7,
                limit: 50,
            },
        )
        .await
        .expect_err("agent quality events require postgres");
        assert!(error.to_string().contains("postgres pool unavailable"));

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn counter_updates_are_thread_safe() {
        let _guard = test_runtime_lock();
        reset_for_tests();
        init_observability(None);

        let iterations = 500usize;
        let mut tasks = Vec::new();
        for _ in 0..8 {
            tasks.push(tokio::spawn(async move {
                for _ in 0..iterations {
                    emit_turn_started("claude", 99, None, None, None);
                }
            }));
        }
        for task in tasks {
            task.await.expect("counter task");
        }
        flush_for_tests().await;

        let response = query_analytics(
            None,
            &AnalyticsFilters {
                provider: Some("claude".to_string()),
                channel_id: Some("99".to_string()),
                ..AnalyticsFilters::default()
            },
        )
        .await
        .expect("query analytics");

        assert_eq!(response.counters[0].turn_attempts, (iterations * 8) as u64);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn init_observability_retains_only_pg_pool_when_configured() {
        let _guard = test_runtime_lock();
        reset_for_tests();
        let pg_pool = PgPoolOptions::new().connect_lazy_with(
            PgConnectOptions::new()
                .host("localhost")
                .username("agentdesk")
                .database("agentdesk"),
        );

        init_observability(Some(pg_pool));

        let (has_db, has_pg_pool) = test_storage_presence();
        assert!(
            !has_db,
            "PG runtime should not retain legacy fallback storage"
        );
        assert!(has_pg_pool, "PG runtime should retain the postgres pool");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn emit_overhead_stays_well_below_hot_path_budget() {
        let _guard = test_runtime_lock();
        reset_for_tests();
        init_observability(None);

        let iterations = 20_000usize;
        let baseline_start = std::time::Instant::now();
        let mut baseline_acc = 0usize;
        for idx in 0..iterations {
            baseline_acc = baseline_acc.wrapping_add(std::hint::black_box(idx));
        }
        let baseline_elapsed = baseline_start.elapsed();

        let emit_start = std::time::Instant::now();
        for idx in 0..iterations {
            std::hint::black_box(idx);
            emit_turn_started("codex", 7, None, None, None);
        }
        let emit_elapsed = emit_start.elapsed();
        flush_for_tests().await;

        let overhead = emit_elapsed.saturating_sub(baseline_elapsed);
        let overhead_per_emit_ns = overhead.as_nanos() / iterations as u128;
        println!("observability emit overhead: {overhead_per_emit_ns}ns/op");
        assert!(baseline_acc > 0);
        assert!(
            overhead_per_emit_ns < 50_000,
            "emit overhead too high: {overhead_per_emit_ns}ns/op"
        );
    }

    // -----------------------------------------------------------------
    // #1101 agent_quality_daily rollup PG integration tests.
    //
    // These tests create a scratch Postgres DB via the admin connection,
    // run migrations, seed `agent_quality_event` rows, invoke the rollup,
    // and assert the upserted `agent_quality_daily` row matches the
    // expected metrics. They are gated on a live Postgres admin URL,
    // matching the retrospectives PG test harness.
    // -----------------------------------------------------------------

    fn quality_rollup_postgres_base_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }
        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());
        format!("postgresql://{user}@{host}:{port}")
    }

    fn quality_rollup_admin_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", quality_rollup_postgres_base_url(), admin_db)
    }

    struct QualityRollupPgHarness {
        admin_url: String,
        database_name: String,
        pool: PgPool,
    }

    impl QualityRollupPgHarness {
        async fn try_setup() -> Option<Self> {
            let admin_url = quality_rollup_admin_url();
            let admin_pool = match sqlx::PgPool::connect(&admin_url).await {
                Ok(pool) => pool,
                Err(error) => {
                    eprintln!(
                        "[agent_quality_daily tests] skipping — Postgres admin unavailable: {error}"
                    );
                    return None;
                }
            };
            let database_name = format!("agentdesk_aqd_{}", uuid::Uuid::new_v4().simple());
            if let Err(error) = sqlx::query(&format!("CREATE DATABASE \"{database_name}\""))
                .execute(&admin_pool)
                .await
            {
                eprintln!("[agent_quality_daily tests] CREATE DATABASE failed, skipping: {error}");
                admin_pool.close().await;
                return None;
            }
            admin_pool.close().await;

            let db_url = format!("{}/{}", quality_rollup_postgres_base_url(), database_name);
            let pool = sqlx::PgPool::connect(&db_url).await.ok()?;
            if let Err(error) = crate::db::postgres::migrate(&pool).await {
                eprintln!("[agent_quality_daily tests] migrate failed, skipping: {error}");
                pool.close().await;
                return None;
            }
            Some(Self {
                admin_url,
                database_name,
                pool,
            })
        }

        async fn teardown(self) {
            self.pool.close().await;
            if let Ok(admin_pool) = sqlx::PgPool::connect(&self.admin_url).await {
                let _ = sqlx::query(
                    "SELECT pg_terminate_backend(pid)
                     FROM pg_stat_activity
                     WHERE datname = $1 AND pid <> pg_backend_pid()",
                )
                .bind(&self.database_name)
                .execute(&admin_pool)
                .await;
                let _ = sqlx::query(&format!(
                    "DROP DATABASE IF EXISTS \"{}\"",
                    self.database_name
                ))
                .execute(&admin_pool)
                .await;
                admin_pool.close().await;
            }
        }
    }

    async fn insert_quality_event(
        pool: &PgPool,
        agent_id: &str,
        event_type: &str,
        card_id: Option<&str>,
        payload: Value,
    ) {
        sqlx::query(
            "INSERT INTO agent_quality_event (
                agent_id, event_type, card_id, payload, created_at
             ) VALUES ($1, $2::agent_quality_event_type, $3, CAST($4 AS jsonb), NOW())",
        )
        .bind(agent_id)
        .bind(event_type)
        .bind(card_id)
        .bind(payload.to_string())
        .execute(pool)
        .await
        .expect("insert agent_quality_event");
    }

    #[tokio::test]
    async fn agent_quality_daily_rollup_computes_core_metrics_for_two_agents() {
        let Some(harness) = QualityRollupPgHarness::try_setup().await else {
            return;
        };
        let pool = harness.pool.clone();

        // agent-A — meets sample-size guard (>=5 events), with reworks on card-1.
        for _ in 0..4 {
            insert_quality_event(
                &pool,
                "agent-A",
                "turn_complete",
                None,
                json!({"duration_ms": 500}),
            )
            .await;
        }
        insert_quality_event(&pool, "agent-A", "turn_error", None, json!({})).await;
        insert_quality_event(&pool, "agent-A", "review_pass", Some("card-1"), json!({})).await;
        insert_quality_event(&pool, "agent-A", "review_fail", Some("card-1"), json!({})).await;
        insert_quality_event(&pool, "agent-A", "review_fail", Some("card-1"), json!({})).await;

        // agent-B — below guard (only 2 events).
        insert_quality_event(
            &pool,
            "agent-B",
            "turn_complete",
            None,
            json!({"duration_ms": 100}),
        )
        .await;
        insert_quality_event(&pool, "agent-B", "review_pass", Some("card-2"), json!({})).await;

        let report = run_agent_quality_rollup_pg(&pool)
            .await
            .expect("rollup succeeds");
        assert!(
            report.upserted_rows >= 2,
            "expected 2+ rows, got {:?}",
            report
        );

        let row_a = sqlx::query(
            "SELECT turn_success_rate, review_pass_rate, sample_size, avg_rework_count,
                    measurement_unavailable_7d
             FROM agent_quality_daily WHERE agent_id = 'agent-A'",
        )
        .fetch_one(&pool)
        .await
        .expect("agent-A row");
        let turn_rate: Option<f64> = sqlx::Row::try_get(&row_a, "turn_success_rate").unwrap();
        let review_rate: Option<f64> = sqlx::Row::try_get(&row_a, "review_pass_rate").unwrap();
        let sample_size: i64 = sqlx::Row::try_get(&row_a, "sample_size").unwrap();
        let rework: Option<f64> = sqlx::Row::try_get(&row_a, "avg_rework_count").unwrap();
        assert_eq!(sample_size, 8, "agent-A sample_size (4+1+1+2)");
        assert!(
            (turn_rate.unwrap() - 0.8).abs() < 1e-6,
            "turn_rate={turn_rate:?}"
        );
        assert!(
            (review_rate.unwrap() - (1.0 / 3.0)).abs() < 1e-6,
            "review_rate={review_rate:?}"
        );
        assert_eq!(rework, Some(2.0), "avg_rework_count for card-1 = 2 fails");

        // agent-B sample_size=2 < 5 guard → measurement_unavailable_7d = TRUE.
        let row_b = sqlx::query(
            "SELECT sample_size, measurement_unavailable_7d FROM agent_quality_daily
             WHERE agent_id = 'agent-B'",
        )
        .fetch_one(&pool)
        .await
        .expect("agent-B row");
        let sb: i64 = sqlx::Row::try_get(&row_b, "sample_size").unwrap();
        let unavail: bool = sqlx::Row::try_get(&row_b, "measurement_unavailable_7d").unwrap();
        assert_eq!(sb, 2);
        assert!(unavail, "agent-B must trigger 측정 불가 guard");

        harness.teardown().await;
    }

    #[tokio::test]
    async fn agent_quality_daily_rollup_is_idempotent_on_repeated_runs() {
        let Some(harness) = QualityRollupPgHarness::try_setup().await else {
            return;
        };
        let pool = harness.pool.clone();

        for _ in 0..5 {
            insert_quality_event(&pool, "agent-idem", "turn_complete", None, json!({})).await;
        }

        run_agent_quality_rollup_pg(&pool)
            .await
            .expect("first rollup");
        let row_count_1: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_quality_daily WHERE agent_id = 'agent-idem'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row_count_1, 1);

        // Re-run — row count must stay 1 (upsert, not append).
        run_agent_quality_rollup_pg(&pool)
            .await
            .expect("second rollup");
        let row_count_2: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_quality_daily WHERE agent_id = 'agent-idem'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row_count_2, 1, "re-running rollup must not duplicate row");

        harness.teardown().await;
    }
}
