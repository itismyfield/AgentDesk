use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Result, anyhow};
use dashmap::DashMap;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
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

const EVENT_BATCH_SIZE: usize = 64;
const EVENT_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const SNAPSHOT_FLUSH_INTERVAL: Duration = Duration::from_secs(15);
// #2049 Finding 9: hourly retention sweep on observability tables. Defaults
// are conservative; tunable via `ADK_OBSERVABILITY_*_RETENTION_DAYS` env.
const RETENTION_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);
const DEFAULT_OBSERVABILITY_EVENT_RETENTION_DAYS: i64 = 90;
const DEFAULT_QUALITY_EVENT_RETENTION_DAYS: i64 = 90;
const DEFAULT_COUNTER_SNAPSHOT_RETENTION_DAYS: i64 = 7;
const DEFAULT_EVENT_LIMIT: usize = 100;
const DEFAULT_COUNTER_LIMIT: usize = 200;
const MAX_EVENT_LIMIT: usize = 500;
const MAX_COUNTER_LIMIT: usize = 500;
const DEFAULT_INVARIANT_LIMIT: usize = 50;
const MAX_INVARIANT_LIMIT: usize = 500;
const DEFAULT_QUALITY_LIMIT: usize = 200;
const MAX_QUALITY_LIMIT: usize = 500;
const DEFAULT_QUALITY_DAYS: i64 = 7;
const MAX_QUALITY_DAYS: i64 = 365;
const DEFAULT_QUALITY_DAILY_LIMIT: usize = 60;
const MAX_QUALITY_DAILY_LIMIT: usize = 180;
const DEFAULT_QUALITY_RANKING_LIMIT: usize = 50;
const MAX_QUALITY_RANKING_LIMIT: usize = 200;
const QUALITY_SAMPLE_GUARD: i64 = 5;
const QUALITY_ALERT_DEDUPE_MS: i64 = 24 * 60 * 60 * 1000;
const QUALITY_REVIEW_DROP_THRESHOLD: f64 = 0.20;
const QUALITY_TURN_DROP_THRESHOLD: f64 = 0.15;
const AGENT_QUALITY_EVENT_TYPES: &[&str] = &[
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
struct CounterKey {
    provider: String,
    channel_id: String,
}

#[derive(Debug, Default)]
struct CounterBucket {
    turn_attempts: AtomicU64,
    guard_fires: AtomicU64,
    watcher_replacements: AtomicU64,
    recovery_fires: AtomicU64,
    turn_successes: AtomicU64,
    turn_failures: AtomicU64,
}

impl CounterBucket {
    fn apply(&self, delta: CounterDelta) {
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

    fn snapshot(&self) -> CounterValues {
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
struct CounterDelta {
    turn_attempts: u64,
    guard_fires: u64,
    watcher_replacements: u64,
    recovery_fires: u64,
    turn_successes: u64,
    turn_failures: u64,
}

impl CounterDelta {
    fn is_zero(self) -> bool {
        self.turn_attempts == 0
            && self.guard_fires == 0
            && self.watcher_replacements == 0
            && self.recovery_fires == 0
            && self.turn_successes == 0
            && self.turn_failures == 0
    }
}

#[derive(Debug, Clone, Serialize)]
struct QueuedEvent {
    event_type: String,
    provider: Option<String>,
    channel_id: Option<String>,
    dispatch_id: Option<String>,
    session_key: Option<String>,
    turn_id: Option<String>,
    status: Option<String>,
    payload_json: String,
}

#[derive(Debug, Clone, Serialize)]
struct QueuedQualityEvent {
    source_event_id: Option<String>,
    correlation_id: Option<String>,
    agent_id: Option<String>,
    provider: Option<String>,
    channel_id: Option<String>,
    card_id: Option<String>,
    dispatch_id: Option<String>,
    event_type: String,
    payload_json: String,
}

#[derive(Debug)]
enum WorkerMessage {
    Event(QueuedEvent),
    QualityEvent(QueuedQualityEvent),
    Flush(oneshot::Sender<()>),
}

#[derive(Clone, Default)]
struct StorageHandles {
    pg_pool: Option<PgPool>,
}

#[derive(Default)]
struct ObservabilityRuntime {
    counters: DashMap<CounterKey, Arc<CounterBucket>>,
    storage: Mutex<StorageHandles>,
    sender: Mutex<Option<mpsc::UnboundedSender<WorkerMessage>>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct CounterValues {
    turn_attempts: u64,
    guard_fires: u64,
    watcher_replacements: u64,
    recovery_fires: u64,
    turn_successes: u64,
    turn_failures: u64,
}

#[derive(Debug, Clone)]
struct SnapshotRow {
    provider: String,
    channel_id: String,
    values: CounterValues,
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

fn runtime() -> Arc<ObservabilityRuntime> {
    OBSERVABILITY_RUNTIME
        .get_or_init(|| Arc::new(ObservabilityRuntime::default()))
        .clone()
}

pub fn init_observability(pg_pool: Option<PgPool>) {
    let runtime = runtime();
    if let Ok(mut storage) = runtime.storage.lock() {
        storage.pg_pool = pg_pool;
    }
    ensure_worker(&runtime);
    // #1070: start the periodic JSONL flush task for the in-memory event log.
    events::ensure_flusher();
}

pub fn emit_turn_started(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
) {
    // #1070: lightweight atomic counter for turn_bridge attempt entry.
    metrics::record_attempt(channel_id, provider);
    events::record(events::StructuredEvent::new(
        "turn_started",
        Some(channel_id),
        Some(provider),
        json!({
            "dispatch_id": dispatch_id,
            "session_key": session_key,
            "turn_id": turn_id,
        }),
    ));
    emit_event(
        "turn_started",
        Some(provider),
        Some(channel_id),
        dispatch_id,
        session_key,
        turn_id,
        Some("started"),
        CounterDelta {
            turn_attempts: 1,
            ..CounterDelta::default()
        },
        json!({}),
    );
}

pub fn emit_turn_finished(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    outcome: &str,
    duration_ms: i64,
    tmux_handoff: bool,
) {
    emit_turn_finished_with_dispatch_kind(
        provider,
        channel_id,
        dispatch_id,
        session_key,
        turn_id,
        outcome,
        duration_ms,
        tmux_handoff,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
pub fn emit_turn_finished_with_dispatch_kind(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    outcome: &str,
    duration_ms: i64,
    tmux_handoff: bool,
    dispatch_kind: Option<&str>,
) {
    let normalized_outcome = normalize_string(outcome);
    let dispatch_kind = dispatch_kind.and_then(normalize_string);
    let is_success = matches!(
        normalized_outcome.as_deref(),
        Some("completed") | Some("tmux_handoff")
    );
    // #1070: atomic success/fail counters for dispatch outcome.
    if is_success {
        metrics::record_success(channel_id, provider);
    } else {
        metrics::record_fail(channel_id, provider);
    }
    events::record(events::StructuredEvent::new(
        "turn_finished",
        Some(channel_id),
        Some(provider),
        json!({
            "dispatch_id": dispatch_id,
            "session_key": session_key,
            "turn_id": turn_id,
            "outcome": normalized_outcome,
            "duration_ms": duration_ms.max(0),
            "tmux_handoff": tmux_handoff,
            "dispatch_kind": dispatch_kind.as_deref(),
        }),
    ));
    emit_event(
        "turn_finished",
        Some(provider),
        Some(channel_id),
        dispatch_id,
        session_key,
        turn_id,
        normalized_outcome.as_deref(),
        CounterDelta {
            turn_successes: u64::from(is_success),
            turn_failures: u64::from(!is_success),
            ..CounterDelta::default()
        },
        json!({
            "duration_ms": duration_ms.max(0),
            "tmux_handoff": tmux_handoff,
            "dispatch_kind": dispatch_kind.as_deref(),
        }),
    );
}

pub fn emit_guard_fired(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    guard_type: &str,
) {
    // #1070: atomic guard-fire counter.
    metrics::record_guard_fire(channel_id, provider);
    events::record(events::StructuredEvent::new(
        "guard_fired",
        Some(channel_id),
        Some(provider),
        json!({
            "dispatch_id": dispatch_id,
            "session_key": session_key,
            "turn_id": turn_id,
            "guard_type": normalize_string(guard_type),
        }),
    ));
    emit_event(
        "guard_fired",
        Some(provider),
        Some(channel_id),
        dispatch_id,
        session_key,
        turn_id,
        normalize_string(guard_type).as_deref(),
        CounterDelta {
            guard_fires: 1,
            ..CounterDelta::default()
        },
        json!({
            "guard_type": normalize_string(guard_type),
        }),
    );
}

pub fn emit_watcher_replaced(provider: &str, channel_id: u64, source: &str) {
    // #1070: atomic watcher-replacement counter for claim_or_replace stale cancel.
    metrics::record_watcher_replacement(channel_id, provider);
    events::record(events::StructuredEvent::new(
        "watcher_replaced",
        Some(channel_id),
        Some(provider),
        json!({ "source": normalize_string(source) }),
    ));
    emit_event(
        "watcher_replaced",
        Some(provider),
        Some(channel_id),
        None,
        None,
        None,
        Some("replaced"),
        CounterDelta {
            watcher_replacements: 1,
            ..CounterDelta::default()
        },
        json!({
            "source": normalize_string(source),
        }),
    );
}

pub fn emit_recovery_fired(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    reason: &str,
) {
    emit_event(
        "recovery_fired",
        Some(provider),
        Some(channel_id),
        dispatch_id,
        session_key,
        None,
        normalize_string(reason).as_deref(),
        CounterDelta {
            recovery_fires: 1,
            ..CounterDelta::default()
        },
        json!({
            "reason": normalize_string(reason),
        }),
    );
}

/// Records a cancellation attempt/result from shared lifecycle paths. Callers
/// should pass correlation IDs when they own them; lower-level stop helpers may
/// only know provider/channel and still emit a channel-scoped event.
pub fn emit_turn_cancelled(
    provider: Option<&str>,
    channel_id: Option<u64>,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    details: turn_lifecycle::TurnCancellationDetails,
) {
    let mut payload = serde_json::to_value(&details).unwrap_or_else(|_| json!({}));
    if let Value::Object(fields) = &mut payload {
        fields.insert("dispatch_id".to_string(), json!(dispatch_id));
        fields.insert("session_key".to_string(), json!(session_key));
        fields.insert("turn_id".to_string(), json!(turn_id));
    }
    events::record(events::StructuredEvent::new(
        "turn_cancelled",
        channel_id,
        provider,
        payload.clone(),
    ));
    emit_event(
        "turn_cancelled",
        provider,
        channel_id,
        dispatch_id,
        session_key,
        turn_id,
        Some("cancelled"),
        CounterDelta::default(),
        payload,
    );
}

/// Inflight lifecycle observability: pair-tracking events so external monitors
/// can detect cleanup leaks. `kind` is the lifecycle phase identifier:
/// `"delegated_to_watcher"`, `"cleared_by_bridge"`, `"cleared_by_watcher"`,
/// `"leak_detected_completed_stale"`. `delegated_to_watcher` and
/// `cleared_by_watcher` should pair 1:1; a sustained drift between counters
/// indicates the bridge handed off cleanup but the watcher never executed it.
/// `leak_detected_completed_stale` fires from the stall-watchdog when an
/// inflight is healthy/synced but the mailbox is idle past the staleness
/// threshold — the smoking-gun signal for the deadlock-manager alarm pattern.
pub fn emit_inflight_lifecycle_event(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    kind: &str,
    extra: Value,
) {
    emit_event(
        "inflight_lifecycle",
        Some(provider),
        Some(channel_id),
        dispatch_id,
        session_key,
        turn_id,
        normalize_string(kind).as_deref(),
        CounterDelta::default(),
        json!({
            "kind": normalize_string(kind),
            "extra": extra,
        }),
    );
}

pub fn record_invariant_check(condition: bool, violation: InvariantViolation<'_>) -> bool {
    if condition {
        return true;
    }

    let invariant = normalize_string(violation.invariant).unwrap_or_else(|| "unknown".to_string());
    tracing::error!(
        invariant = %invariant,
        provider = violation.provider.unwrap_or_default(),
        channel_id = violation.channel_id.unwrap_or_default(),
        dispatch_id = violation.dispatch_id.unwrap_or_default(),
        session_key = violation.session_key.unwrap_or_default(),
        turn_id = violation.turn_id.unwrap_or_default(),
        code_location = violation.code_location,
        "[invariant] {}",
        violation.message
    );

    emit_event(
        "invariant_violation",
        violation.provider,
        violation.channel_id,
        violation.dispatch_id,
        violation.session_key,
        violation.turn_id,
        Some(invariant.as_str()),
        CounterDelta {
            guard_fires: 1,
            ..CounterDelta::default()
        },
        json!({
            "invariant": invariant,
            "code_location": violation.code_location,
            "message": violation.message,
            "details": violation.details,
        }),
    );
    false
}

pub fn emit_dispatch_result(
    dispatch_id: &str,
    kanban_card_id: Option<&str>,
    dispatch_type: Option<&str>,
    from_status: Option<&str>,
    to_status: &str,
    transition_source: &str,
    payload: Option<&Value>,
) {
    emit_event(
        "dispatch_result",
        None,
        None,
        Some(dispatch_id),
        None,
        None,
        Some(to_status),
        CounterDelta::default(),
        json!({
            "kanban_card_id": normalize_string(kanban_card_id.unwrap_or_default()),
            "dispatch_type": normalize_string(dispatch_type.unwrap_or_default()),
            "from_status": normalize_string(from_status.unwrap_or_default()),
            "to_status": normalize_string(to_status),
            "transition_source": normalize_string(transition_source),
            "payload": payload.cloned().unwrap_or_else(|| json!({})),
        }),
    );
}

/// #1984 (codex C — observation): record a structured event whenever a
/// Discord placeholder POST fails inside one of the intake/queue/race code
/// paths. Each call site sets a stable `phase` label so the resulting PG
/// rows can be aggregated into a daily failure count, and a `recovery`
/// label so we can confirm whether the user message stayed in queue, the
/// mailbox slot was released, or it dropped on the floor.
///
/// This is the measurement instrumentation the retro asked for before
/// considering whether to re-apply the rolled-back 7f8184b9 retry guard.
/// The event payload is intentionally minimal — we want raw counts, not a
/// full incident timeline.
pub fn emit_intake_placeholder_post_failed(
    provider: &str,
    channel_id: u64,
    user_msg_id: Option<u64>,
    phase: &str,
    recovery: &str,
    error: &str,
) {
    emit_event(
        "intake_placeholder_post_failed",
        Some(provider),
        Some(channel_id),
        None,
        None,
        None,
        normalize_string(phase).as_deref(),
        CounterDelta::default(),
        json!({
            "phase": normalize_string(phase),
            "recovery": normalize_string(recovery),
            "user_msg_id": user_msg_id,
            "error": normalize_string(error),
        }),
    );
}

pub fn emit_agent_quality_event(event: AgentQualityEvent) {
    let Some(event_type) = normalize_quality_event_type(&event.event_type) else {
        tracing::warn!(
            event_type = %event.event_type,
            "[quality] dropping unknown agent quality event type"
        );
        return;
    };

    let queued = QueuedQualityEvent {
        source_event_id: event.source_event_id.as_deref().and_then(normalize_string),
        correlation_id: event.correlation_id.as_deref().and_then(normalize_string),
        agent_id: event.agent_id.as_deref().and_then(normalize_string),
        provider: event.provider.as_deref().and_then(normalize_string),
        channel_id: event.channel_id.as_deref().and_then(normalize_string),
        card_id: event.card_id.as_deref().and_then(normalize_string),
        dispatch_id: event.dispatch_id.as_deref().and_then(normalize_string),
        event_type,
        payload_json: serde_json::to_string(&event.payload).unwrap_or_else(|_| "{}".to_string()),
    };

    if let Some(sender) = worker_sender() {
        let _ = sender.send(WorkerMessage::QualityEvent(queued));
    }
}

fn emit_event(
    event_type: &str,
    provider: Option<&str>,
    channel_id: Option<u64>,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    status: Option<&str>,
    counter_delta: CounterDelta,
    payload: Value,
) {
    let event_type = normalize_string(event_type);
    if event_type.is_none() {
        return;
    }

    let provider = provider.and_then(normalize_string);
    let channel_id = channel_id.map(|value| value.to_string());
    if !counter_delta.is_zero()
        && let (Some(provider), Some(channel_id)) = (provider.as_ref(), channel_id.as_ref())
    {
        let bucket = runtime()
            .counters
            .entry(CounterKey {
                provider: provider.clone(),
                channel_id: channel_id.clone(),
            })
            .or_insert_with(|| Arc::new(CounterBucket::default()))
            .clone();
        bucket.apply(counter_delta);
    }

    let payload_json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    let queued = QueuedEvent {
        event_type: event_type.unwrap_or_default(),
        provider,
        channel_id,
        dispatch_id: dispatch_id.and_then(normalize_string),
        session_key: session_key.and_then(normalize_string),
        turn_id: turn_id.and_then(normalize_string),
        status: status.and_then(normalize_string),
        payload_json,
    };

    if let Some(sender) = worker_sender() {
        let _ = sender.send(WorkerMessage::Event(queued));
    }
}

fn worker_sender() -> Option<mpsc::UnboundedSender<WorkerMessage>> {
    let runtime = runtime();
    ensure_worker(&runtime);
    runtime.sender.lock().ok().and_then(|sender| sender.clone())
}

fn ensure_worker(runtime: &Arc<ObservabilityRuntime>) {
    let mut sender_guard = match runtime.sender.lock() {
        Ok(guard) => guard,
        Err(_) => return,
    };
    if sender_guard
        .as_ref()
        .is_some_and(|sender| !sender.is_closed())
    {
        return;
    }

    let (tx, rx) = mpsc::unbounded_channel();
    *sender_guard = Some(tx);
    spawn_worker(runtime.clone(), rx);
}

fn spawn_worker(runtime: Arc<ObservabilityRuntime>, rx: mpsc::UnboundedReceiver<WorkerMessage>) {
    let task = async move {
        worker_loop(runtime, rx).await;
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(task);
        return;
    }

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        match runtime {
            Ok(runtime) => runtime.block_on(task),
            Err(error) => {
                tracing::warn!("[observability] failed to bootstrap worker runtime: {error}");
            }
        }
    });
}

async fn worker_loop(
    runtime: Arc<ObservabilityRuntime>,
    mut rx: mpsc::UnboundedReceiver<WorkerMessage>,
) {
    let mut batch = Vec::new();
    let mut quality_batch = Vec::new();
    let mut flush_tick = tokio::time::interval(EVENT_FLUSH_INTERVAL);
    let mut snapshot_tick = tokio::time::interval(SNAPSHOT_FLUSH_INTERVAL);
    // #2049 Finding 9: hourly retention sweep.
    let mut retention_tick = tokio::time::interval(RETENTION_SWEEP_INTERVAL);
    flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    snapshot_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    retention_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            maybe_message = rx.recv() => {
                match maybe_message {
                    Some(WorkerMessage::Event(event)) => {
                        batch.push(event);
                        if batch.len() >= EVENT_BATCH_SIZE {
                            flush_event_batch(&runtime, &mut batch).await;
                        }
                    }
                    Some(WorkerMessage::QualityEvent(event)) => {
                        quality_batch.push(event);
                        if quality_batch.len() >= EVENT_BATCH_SIZE {
                            flush_quality_event_batch(&runtime, &mut quality_batch).await;
                        }
                    }
                    Some(WorkerMessage::Flush(done)) => {
                        flush_event_batch(&runtime, &mut batch).await;
                        flush_quality_event_batch(&runtime, &mut quality_batch).await;
                        flush_counter_snapshots(&runtime).await;
                        let _ = done.send(());
                    }
                    None => break,
                }
            }
            _ = flush_tick.tick() => {
                flush_event_batch(&runtime, &mut batch).await;
                flush_quality_event_batch(&runtime, &mut quality_batch).await;
            }
            _ = snapshot_tick.tick() => {
                flush_counter_snapshots(&runtime).await;
            }
            _ = retention_tick.tick() => {
                run_retention_sweep(&runtime).await;
            }
        }
    }

    flush_event_batch(&runtime, &mut batch).await;
    flush_quality_event_batch(&runtime, &mut quality_batch).await;
    flush_counter_snapshots(&runtime).await;
}

/// #2049 Finding 1: Flush observability events with multi-tier fallback.
/// (1) Try bulk insert in one tx. (2) On bulk failure, retry each row in its
/// own tx so a single broken row does not lose its 63 healthy neighbors.
/// (3) Rows that still fail are appended to a dead-letter JSONL on disk so
/// the sample is recoverable. (4) If JSONL fallback also fails, push the
/// events back to the front of the worker batch so the next tick retries.
async fn flush_event_batch(runtime: &Arc<ObservabilityRuntime>, batch: &mut Vec<QueuedEvent>) {
    if batch.is_empty() {
        return;
    }
    let drained = std::mem::take(batch);
    let handles = storage_handles(runtime);

    let Some(pool) = handles.pg_pool.as_ref() else {
        // No PG configured (standalone mode) — preserve pre-#2049 behaviour;
        // standalone mode never persisted these events.
        return;
    };

    if let Err(error) = insert_events_pg(pool, &drained).await {
        tracing::warn!(
            "[observability] postgres event bulk flush failed ({} events): {error}; retrying per-row",
            drained.len()
        );
        let failed = insert_events_pg_row_isolated(pool, &drained).await;
        if !failed.is_empty() {
            handle_event_flush_fallback(batch, failed, "events");
        }
    }
}

/// #2049 Finding 1 — same multi-tier fallback for `agent_quality_event`.
async fn flush_quality_event_batch(
    runtime: &Arc<ObservabilityRuntime>,
    batch: &mut Vec<QueuedQualityEvent>,
) {
    if batch.is_empty() {
        return;
    }
    let drained = std::mem::take(batch);
    let handles = storage_handles(runtime);

    let Some(pool) = handles.pg_pool.as_ref() else {
        return;
    };

    if let Err(error) = insert_quality_events_pg(pool, &drained).await {
        tracing::warn!(
            "[quality] postgres event bulk flush failed ({} events): {error}; retrying per-row",
            drained.len()
        );
        let failed = insert_quality_events_pg_row_isolated(pool, &drained).await;
        if !failed.is_empty() {
            handle_quality_event_flush_fallback(batch, failed);
        }
    }
}

/// #2049 Finding 1: Dump failed event rows to JSONL; if disk fallback fails,
/// push them back to the front of `batch` so the next worker tick retries.
fn handle_event_flush_fallback(
    batch: &mut Vec<QueuedEvent>,
    failed: Vec<QueuedEvent>,
    suffix: &str,
) {
    let count = failed.len();
    match events::flush_dead_letter_jsonl(suffix, &failed) {
        Ok(()) => {
            tracing::warn!(
                "[observability] {count} events dumped to dead-letter JSONL (suffix={suffix}) after PG flush failure"
            );
        }
        Err(disk_error) => {
            tracing::error!(
                "[observability] dead-letter JSONL also failed (suffix={suffix}, {count} events): {disk_error}; pushing back to worker batch"
            );
            let mut rescued = failed;
            rescued.extend(std::mem::take(batch));
            *batch = rescued;
        }
    }
}

fn handle_quality_event_flush_fallback(
    batch: &mut Vec<QueuedQualityEvent>,
    failed: Vec<QueuedQualityEvent>,
) {
    let count = failed.len();
    match events::flush_dead_letter_jsonl("quality-events", &failed) {
        Ok(()) => {
            tracing::warn!(
                "[quality] {count} events dumped to dead-letter JSONL after PG flush failure"
            );
        }
        Err(disk_error) => {
            tracing::error!(
                "[quality] dead-letter JSONL also failed ({count} events): {disk_error}; pushing back to worker batch"
            );
            let mut rescued = failed;
            rescued.extend(std::mem::take(batch));
            *batch = rescued;
        }
    }
}

async fn flush_counter_snapshots(runtime: &Arc<ObservabilityRuntime>) {
    let snapshots = snapshot_rows(runtime, None);
    if snapshots.is_empty() {
        return;
    }

    let handles = storage_handles(runtime);
    if let Some(pool) = handles.pg_pool.as_ref() {
        match insert_snapshots_pg(pool, &snapshots).await {
            Ok(()) => return,
            Err(error) => {
                tracing::warn!("[observability] postgres snapshot flush failed: {error}");
            }
        }
    }
}

/// #2049 Finding 9: prune old rows from observability tables to bound disk
/// and index growth on long-lived single-node deployments. Retention windows
/// are conservative; `observability_counter_snapshots` is pruned aggressively
/// (7d) because analytics only queries the latest snapshot per
/// `(provider, channel_id)`.
async fn run_retention_sweep(runtime: &Arc<ObservabilityRuntime>) {
    let handles = storage_handles(runtime);
    let Some(pool) = handles.pg_pool.as_ref() else {
        return;
    };

    let event_days = env_retention_days(
        "ADK_OBSERVABILITY_EVENT_RETENTION_DAYS",
        DEFAULT_OBSERVABILITY_EVENT_RETENTION_DAYS,
    );
    let quality_days = env_retention_days(
        "ADK_OBSERVABILITY_QUALITY_RETENTION_DAYS",
        DEFAULT_QUALITY_EVENT_RETENTION_DAYS,
    );
    let snapshot_days = env_retention_days(
        "ADK_OBSERVABILITY_COUNTER_SNAPSHOT_RETENTION_DAYS",
        DEFAULT_COUNTER_SNAPSHOT_RETENTION_DAYS,
    );

    if event_days > 0 {
        prune_table_by_created_at(pool, "observability_events", event_days).await;
    }
    if quality_days > 0 {
        prune_table_by_created_at(pool, "agent_quality_event", quality_days).await;
    }
    if snapshot_days > 0 {
        prune_table_by_snapshot_at(pool, "observability_counter_snapshots", snapshot_days).await;
    }
}

fn env_retention_days(var: &str, default_days: i64) -> i64 {
    std::env::var(var)
        .ok()
        .and_then(|raw| raw.trim().parse::<i64>().ok())
        .filter(|value| *value >= 0)
        .unwrap_or(default_days)
}

async fn prune_table_by_created_at(pool: &PgPool, table: &'static str, days: i64) {
    // `table` is sourced from a `&'static str` whitelist — never user input.
    let sql = format!(
        "DELETE FROM {table} WHERE created_at < NOW() - ($1::bigint || ' days')::interval"
    );
    match sqlx::query(&sql).bind(days).execute(pool).await {
        Ok(result) => {
            let affected = result.rows_affected();
            if affected > 0 {
                tracing::info!(
                    "[observability] retention sweep deleted {affected} rows from {table} (>{days}d)"
                );
            }
        }
        Err(error) => {
            tracing::warn!(
                "[observability] retention sweep on {table} failed (days={days}): {error}"
            );
        }
    }
}

async fn prune_table_by_snapshot_at(pool: &PgPool, table: &'static str, days: i64) {
    let sql = format!(
        "DELETE FROM {table} WHERE snapshot_at < NOW() - ($1::bigint || ' days')::interval"
    );
    match sqlx::query(&sql).bind(days).execute(pool).await {
        Ok(result) => {
            let affected = result.rows_affected();
            if affected > 0 {
                tracing::info!(
                    "[observability] retention sweep deleted {affected} rows from {table} (>{days}d)"
                );
            }
        }
        Err(error) => {
            tracing::warn!(
                "[observability] retention sweep on {table} failed (days={days}): {error}"
            );
        }
    }
}

fn storage_handles(runtime: &Arc<ObservabilityRuntime>) -> StorageHandles {
    runtime
        .storage
        .lock()
        .map(|handles| handles.clone())
        .unwrap_or_default()
}

fn snapshot_rows(
    runtime: &Arc<ObservabilityRuntime>,
    filters: Option<&AnalyticsFilters>,
) -> Vec<SnapshotRow> {
    let mut rows = runtime
        .counters
        .iter()
        .filter_map(|entry| {
            if !matches_filters(
                filters,
                &entry.key().provider,
                &entry.key().channel_id,
                None,
            ) {
                return None;
            }
            Some(SnapshotRow {
                provider: entry.key().provider.clone(),
                channel_id: entry.key().channel_id.clone(),
                values: entry.value().snapshot(),
            })
        })
        .collect::<Vec<_>>();

    rows.sort_by(|left, right| {
        right
            .values
            .turn_attempts
            .cmp(&left.values.turn_attempts)
            .then_with(|| left.provider.cmp(&right.provider))
            .then_with(|| left.channel_id.cmp(&right.channel_id))
    });
    rows
}

pub async fn query_analytics(
    pg_pool: Option<&PgPool>,
    filters: &AnalyticsFilters,
) -> Result<AnalyticsResponse> {
    let event_limit = normalized_event_limit(filters.event_limit);
    let counter_limit = normalized_counter_limit(filters.counter_limit);
    let live_counter_rows = snapshot_rows(&runtime(), Some(filters))
        .into_iter()
        .take(counter_limit)
        .collect::<Vec<_>>();
    let mut counters = live_counter_rows
        .into_iter()
        .map(|row| {
            counter_snapshot_from_values(
                &row.provider,
                &row.channel_id,
                row.values,
                "live",
                now_kst(),
            )
        })
        .collect::<Vec<_>>();

    let persisted_counters = query_counter_snapshots_db(pg_pool, filters, counter_limit).await?;
    let mut seen = counters
        .iter()
        .map(|snapshot| (snapshot.provider.clone(), snapshot.channel_id.clone()))
        .collect::<HashSet<_>>();
    for snapshot in persisted_counters {
        if seen.insert((snapshot.provider.clone(), snapshot.channel_id.clone())) {
            counters.push(snapshot);
        }
    }
    counters.truncate(counter_limit);

    let events = query_events_db(pg_pool, filters, event_limit).await?;
    Ok(AnalyticsResponse {
        generated_at: now_kst(),
        counters,
        events,
    })
}

pub async fn query_agent_quality_events(
    pg_pool: Option<&PgPool>,
    filters: &AgentQualityFilters,
) -> Result<Vec<AgentQualityEventRecord>> {
    let days = normalized_quality_days(filters.days);
    let limit = normalized_quality_limit(filters.limit);
    let Some(pool) = pg_pool else {
        return Err(anyhow!(
            "postgres pool unavailable for agent quality events"
        ));
    };
    query_agent_quality_events_pg(pool, filters.agent_id.as_deref(), days, limit).await
}

pub async fn run_agent_quality_rollup_pg(pool: &PgPool) -> Result<AgentQualityRollupReport> {
    let upserted_rows = upsert_agent_quality_daily_pg(pool).await?;
    let alert_count = enqueue_quality_regression_alerts_pg(pool).await?;
    Ok(AgentQualityRollupReport {
        upserted_rows,
        alert_count,
    })
}

pub async fn query_agent_quality_summary(
    pg_pool: Option<&PgPool>,
    agent_id: &str,
    days: i64,
    limit: usize,
) -> Result<AgentQualitySummary> {
    let days = normalized_quality_days(days);
    let limit = normalized_quality_daily_limit(limit);
    let Some(pool) = pg_pool else {
        return Err(anyhow!(
            "postgres pool unavailable for agent quality summary"
        ));
    };
    let daily = match query_agent_quality_daily_pg(pool, Some(agent_id), days, limit).await {
        Ok(records) => records,
        Err(error) => {
            tracing::warn!("[quality] postgres daily query failed: {error}");
            Vec::new()
        }
    };

    // #1102 fallback: when the daily rollup is empty (e.g. the rollup job
    // from #1101 hasn't run yet in this environment), synthesize a mini
    // rollup directly from `agent_quality_event`.
    let (daily, fallback_from_events) = if daily.is_empty() {
        let synthetic = synth_agent_quality_daily_from_events_pg(pool, agent_id, 30)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!("[quality] pg event-fallback mini-rollup failed: {error}");
                Vec::new()
            });
        let fallback = !synthetic.is_empty();
        (synthetic, fallback)
    } else {
        (daily, false)
    };

    let trend_7d = slice_daily_trend(&daily, 7);
    let trend_30d = slice_daily_trend(&daily, 30);
    let latest = daily.first().cloned();

    Ok(AgentQualitySummary {
        generated_at: now_kst(),
        agent_id: agent_id.to_string(),
        current: latest.clone(),
        latest,
        daily,
        trend_7d,
        trend_30d,
        fallback_from_events,
    })
}

fn slice_daily_trend(
    daily: &[AgentQualityDailyRecord],
    max_days: usize,
) -> Vec<AgentQualityDailyRecord> {
    daily.iter().take(max_days).cloned().collect()
}

pub async fn query_agent_quality_ranking(
    pg_pool: Option<&PgPool>,
    limit: usize,
) -> Result<AgentQualityRankingResponse> {
    query_agent_quality_ranking_with(
        pg_pool,
        limit,
        QualityRankingMetric::TurnSuccessRate,
        QualityRankingWindow::Seven,
        QUALITY_SAMPLE_GUARD,
    )
    .await
}

pub async fn query_agent_quality_ranking_with(
    pg_pool: Option<&PgPool>,
    limit: usize,
    metric: QualityRankingMetric,
    window: QualityRankingWindow,
    min_sample_size: i64,
) -> Result<AgentQualityRankingResponse> {
    let limit = normalized_quality_ranking_limit(limit);
    let min_sample_size = min_sample_size.max(0);
    let Some(pool) = pg_pool else {
        return Err(anyhow!(
            "postgres pool unavailable for agent quality ranking"
        ));
    };
    let agents = match query_agent_quality_ranking_pg(pool, limit).await {
        Ok(records) => records,
        Err(error) => {
            tracing::warn!("[quality] postgres ranking query failed: {error}");
            Vec::new()
        }
    };

    // Filter by sample_size >= min_sample_size (#1102 DoD), then attach
    // metric_value and re-rank by the requested (metric, window) pair.
    let mut filtered: Vec<AgentQualityRankingEntry> = agents
        .into_iter()
        .filter(|entry| ranking_window_sample_size(entry, window) >= min_sample_size)
        .map(|mut entry| {
            entry.metric_value = pick_ranking_metric_value(&entry, metric, window);
            entry
        })
        .collect();

    // Sort by the chosen metric desc, NULLs last; tiebreak on sample size desc,
    // then agent_id asc for determinism.
    filtered.sort_by(|a, b| {
        let av = a.metric_value;
        let bv = b.metric_value;
        match (av, bv) {
            (Some(ax), Some(bx)) => bx
                .partial_cmp(&ax)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    ranking_window_sample_size(b, window)
                        .cmp(&ranking_window_sample_size(a, window))
                })
                .then_with(|| a.agent_id.cmp(&b.agent_id)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.agent_id.cmp(&b.agent_id),
        }
    });

    // Re-rank 1..n after filtering/sorting.
    for (idx, entry) in filtered.iter_mut().enumerate() {
        entry.rank = (idx + 1) as i64;
    }

    Ok(AgentQualityRankingResponse {
        generated_at: now_kst(),
        metric: metric.label().to_string(),
        window: window.label().to_string(),
        min_sample_size,
        agents: filtered,
    })
}

pub async fn query_invariant_analytics(
    pg_pool: Option<&PgPool>,
    filters: &InvariantAnalyticsFilters,
) -> Result<InvariantAnalyticsResponse> {
    let limit = normalized_invariant_limit(filters.limit);
    let counts = query_invariant_counts_db(pg_pool, filters).await?;
    let total_violations = counts.iter().map(|count| count.count).sum();
    let recent = query_invariant_events_db(pg_pool, filters, limit).await?;

    Ok(InvariantAnalyticsResponse {
        generated_at: now_kst(),
        total_violations,
        counts,
        recent,
    })
}

async fn query_invariant_counts_db(
    pg_pool: Option<&PgPool>,
    filters: &InvariantAnalyticsFilters,
) -> Result<Vec<InvariantViolationCount>> {
    let Some(pool) = pg_pool else {
        return Err(anyhow!("postgres pool unavailable for invariant counts"));
    };
    query_invariant_counts_pg(pool, filters).await
}

async fn query_invariant_events_db(
    pg_pool: Option<&PgPool>,
    filters: &InvariantAnalyticsFilters,
    limit: usize,
) -> Result<Vec<InvariantViolationRecord>> {
    let Some(pool) = pg_pool else {
        return Err(anyhow!("postgres pool unavailable for invariant events"));
    };
    query_invariant_events_pg(pool, filters, limit).await
}

async fn query_events_db(
    pg_pool: Option<&PgPool>,
    filters: &AnalyticsFilters,
    limit: usize,
) -> Result<Vec<AnalyticsEventRecord>> {
    let Some(pool) = pg_pool else {
        return Ok(Vec::new());
    };
    query_events_pg(pool, filters, limit).await
}

async fn query_counter_snapshots_db(
    pg_pool: Option<&PgPool>,
    filters: &AnalyticsFilters,
    limit: usize,
) -> Result<Vec<AnalyticsCounterSnapshot>> {
    let Some(pool) = pg_pool else {
        return Ok(Vec::new());
    };
    query_counter_snapshots_pg(pool, filters, limit).await
}

async fn query_events_pg(
    pool: &PgPool,
    filters: &AnalyticsFilters,
    limit: usize,
) -> Result<Vec<AnalyticsEventRecord>> {
    let rows = sqlx::query(
        "SELECT id,
                event_type,
                provider,
                channel_id,
                dispatch_id,
                session_key,
                turn_id,
                status,
                payload_json::text AS payload_json,
                to_char(created_at AT TIME ZONE 'Asia/Seoul', 'YYYY-MM-DD HH24:MI:SS') AS created_at_kst
         FROM observability_events
         WHERE ($1::text IS NULL OR provider = $1)
           AND ($2::text IS NULL OR channel_id = $2)
           AND ($3::text IS NULL OR event_type = $3)
         ORDER BY id DESC
         LIMIT $4",
    )
    .bind(filters.provider.as_deref())
    .bind(filters.channel_id.as_deref())
    .bind(filters.event_type.as_deref())
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("query postgres observability events: {error}"))?;

    rows.into_iter()
        .map(|row| {
            let payload_json: Option<String> = row
                .try_get("payload_json")
                .map_err(|error| anyhow!("decode observability payload_json: {error}"))?;
            Ok(AnalyticsEventRecord {
                id: row
                    .try_get("id")
                    .map_err(|error| anyhow!("decode observability event id: {error}"))?,
                event_type: row
                    .try_get("event_type")
                    .map_err(|error| anyhow!("decode observability event_type: {error}"))?,
                provider: row
                    .try_get("provider")
                    .map_err(|error| anyhow!("decode observability provider: {error}"))?,
                channel_id: row
                    .try_get("channel_id")
                    .map_err(|error| anyhow!("decode observability channel_id: {error}"))?,
                dispatch_id: row
                    .try_get("dispatch_id")
                    .map_err(|error| anyhow!("decode observability dispatch_id: {error}"))?,
                session_key: row
                    .try_get("session_key")
                    .map_err(|error| anyhow!("decode observability session_key: {error}"))?,
                turn_id: row
                    .try_get("turn_id")
                    .map_err(|error| anyhow!("decode observability turn_id: {error}"))?,
                status: row
                    .try_get("status")
                    .map_err(|error| anyhow!("decode observability status: {error}"))?,
                payload: payload_json
                    .as_deref()
                    .and_then(|value| serde_json::from_str(value).ok())
                    .unwrap_or_else(|| json!({})),
                created_at: row
                    .try_get("created_at_kst")
                    .map_err(|error| anyhow!("decode observability created_at: {error}"))?,
            })
        })
        .collect()
}

async fn query_invariant_counts_pg(
    pool: &PgPool,
    filters: &InvariantAnalyticsFilters,
) -> Result<Vec<InvariantViolationCount>> {
    let rows = sqlx::query(
        "SELECT COALESCE(status, 'unknown') AS invariant,
                COUNT(*) AS violation_count
         FROM observability_events
         WHERE event_type = 'invariant_violation'
           AND ($1::text IS NULL OR provider = $1)
           AND ($2::text IS NULL OR channel_id = $2)
           AND ($3::text IS NULL OR status = $3)
         GROUP BY COALESCE(status, 'unknown')
         ORDER BY violation_count DESC, invariant ASC",
    )
    .bind(filters.provider.as_deref())
    .bind(filters.channel_id.as_deref())
    .bind(filters.invariant.as_deref())
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("query postgres invariant counts: {error}"))?;

    rows.into_iter()
        .map(|row| {
            Ok(InvariantViolationCount {
                invariant: row
                    .try_get("invariant")
                    .map_err(|error| anyhow!("decode invariant: {error}"))?,
                count: row
                    .try_get::<i64, _>("violation_count")
                    .map_err(|error| anyhow!("decode violation_count: {error}"))?
                    .max(0) as u64,
            })
        })
        .collect()
}

async fn query_invariant_events_pg(
    pool: &PgPool,
    filters: &InvariantAnalyticsFilters,
    limit: usize,
) -> Result<Vec<InvariantViolationRecord>> {
    let rows = sqlx::query(
        "SELECT id,
                provider,
                channel_id,
                dispatch_id,
                session_key,
                turn_id,
                status,
                payload_json::text AS payload_json,
                to_char(created_at AT TIME ZONE 'Asia/Seoul', 'YYYY-MM-DD HH24:MI:SS') AS created_at_kst
         FROM observability_events
         WHERE event_type = 'invariant_violation'
           AND ($1::text IS NULL OR provider = $1)
           AND ($2::text IS NULL OR channel_id = $2)
           AND ($3::text IS NULL OR status = $3)
         ORDER BY id DESC
         LIMIT $4",
    )
    .bind(filters.provider.as_deref())
    .bind(filters.channel_id.as_deref())
    .bind(filters.invariant.as_deref())
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("query postgres invariant events: {error}"))?;

    rows.into_iter()
        .map(|row| {
            let payload_json: Option<String> = row
                .try_get("payload_json")
                .map_err(|error| anyhow!("decode invariant payload_json: {error}"))?;
            let payload = payload_json
                .as_deref()
                .and_then(|value| serde_json::from_str::<Value>(value).ok())
                .unwrap_or_else(|| json!({}));
            let invariant = row
                .try_get::<Option<String>, _>("status")
                .map_err(|error| anyhow!("decode invariant status: {error}"))?
                .or_else(|| payload.get("invariant").and_then(value_as_string))
                .unwrap_or_else(|| "unknown".to_string());
            Ok(invariant_record_from_parts(
                row.try_get("id")
                    .map_err(|error| anyhow!("decode invariant event id: {error}"))?,
                invariant,
                row.try_get("provider")
                    .map_err(|error| anyhow!("decode invariant provider: {error}"))?,
                row.try_get("channel_id")
                    .map_err(|error| anyhow!("decode invariant channel_id: {error}"))?,
                row.try_get("dispatch_id")
                    .map_err(|error| anyhow!("decode invariant dispatch_id: {error}"))?,
                row.try_get("session_key")
                    .map_err(|error| anyhow!("decode invariant session_key: {error}"))?,
                row.try_get("turn_id")
                    .map_err(|error| anyhow!("decode invariant turn_id: {error}"))?,
                payload,
                row.try_get("created_at_kst")
                    .map_err(|error| anyhow!("decode invariant created_at: {error}"))?,
            ))
        })
        .collect()
}

async fn query_agent_quality_events_pg(
    pool: &PgPool,
    agent_id: Option<&str>,
    days: i64,
    limit: usize,
) -> Result<Vec<AgentQualityEventRecord>> {
    let rows = sqlx::query(
        "SELECT id,
                source_event_id,
                correlation_id,
                agent_id,
                provider,
                channel_id,
                card_id,
                dispatch_id,
                event_type::text AS event_type,
                payload::text AS payload_json,
                to_char(created_at AT TIME ZONE 'Asia/Seoul', 'YYYY-MM-DD HH24:MI:SS') AS created_at_kst
         FROM agent_quality_event
         WHERE ($1::text IS NULL OR agent_id = $1)
           AND created_at >= NOW() - ($2::int * INTERVAL '1 day')
         ORDER BY created_at DESC, id DESC
         LIMIT $3",
    )
    .bind(agent_id)
    .bind(days as i32)
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("query postgres agent quality events: {error}"))?;

    rows.into_iter()
        .map(|row| {
            let payload_json: Option<String> = row
                .try_get("payload_json")
                .map_err(|error| anyhow!("decode agent quality payload_json: {error}"))?;
            Ok(AgentQualityEventRecord {
                id: row
                    .try_get("id")
                    .map_err(|error| anyhow!("decode agent quality event id: {error}"))?,
                source_event_id: row
                    .try_get("source_event_id")
                    .map_err(|error| anyhow!("decode agent quality source_event_id: {error}"))?,
                correlation_id: row
                    .try_get("correlation_id")
                    .map_err(|error| anyhow!("decode agent quality correlation_id: {error}"))?,
                agent_id: row
                    .try_get("agent_id")
                    .map_err(|error| anyhow!("decode agent quality agent_id: {error}"))?,
                provider: row
                    .try_get("provider")
                    .map_err(|error| anyhow!("decode agent quality provider: {error}"))?,
                channel_id: row
                    .try_get("channel_id")
                    .map_err(|error| anyhow!("decode agent quality channel_id: {error}"))?,
                card_id: row
                    .try_get("card_id")
                    .map_err(|error| anyhow!("decode agent quality card_id: {error}"))?,
                dispatch_id: row
                    .try_get("dispatch_id")
                    .map_err(|error| anyhow!("decode agent quality dispatch_id: {error}"))?,
                event_type: row
                    .try_get("event_type")
                    .map_err(|error| anyhow!("decode agent quality event_type: {error}"))?,
                payload: payload_json
                    .as_deref()
                    .and_then(|value| serde_json::from_str(value).ok())
                    .unwrap_or_else(|| json!({})),
                created_at: row
                    .try_get("created_at_kst")
                    .map_err(|error| anyhow!("decode agent quality created_at: {error}"))?,
            })
        })
        .collect()
}

async fn upsert_agent_quality_daily_pg(pool: &PgPool) -> Result<u64> {
    // #1101 extends #930's rollup with four additional daily metrics:
    //   * avg_rework_count   — avg(review_fail per card) over cards that had
    //                          at least one review_fail that day.
    //   * cost_per_done_card — sum(card_transitioned.payload->>'cost') /
    //                          count(card_transitioned → done).
    //   * latency_p50_ms     — percentile_cont(0.5) over turn_complete
    //                          payload->>'duration_ms'.
    //   * latency_p99_ms     — percentile_cont(0.99) over turn_complete
    //                          payload->>'duration_ms'.
    //
    // All four are nullable; they land as NULL when the requisite events are
    // absent for an agent/day, which is also how the dashboard renders
    // "측정 불가" downstream.
    let row_count = sqlx::query_scalar::<_, i64>(
        "WITH daily_counts AS (
             SELECT agent_id,
                    (created_at AT TIME ZONE 'Asia/Seoul')::date AS day,
                    MAX(provider) FILTER (WHERE provider IS NOT NULL AND btrim(provider) <> '') AS provider,
                    MAX(channel_id) FILTER (WHERE channel_id IS NOT NULL AND btrim(channel_id) <> '') AS channel_id,
                    COUNT(*) FILTER (WHERE event_type = 'turn_complete')::bigint AS turn_success_count,
                    COUNT(*) FILTER (WHERE event_type = 'turn_error')::bigint AS turn_error_count,
                    COUNT(*) FILTER (WHERE event_type = 'review_pass')::bigint AS review_pass_count,
                    COUNT(*) FILTER (WHERE event_type = 'review_fail')::bigint AS review_fail_count
             FROM agent_quality_event
             WHERE agent_id IS NOT NULL
               AND btrim(agent_id) <> ''
               AND created_at >= NOW() - INTERVAL '45 days'
             GROUP BY agent_id, (created_at AT TIME ZONE 'Asia/Seoul')::date
         ),
         rework_per_card AS (
             SELECT agent_id,
                    (created_at AT TIME ZONE 'Asia/Seoul')::date AS day,
                    card_id,
                    COUNT(*)::double precision AS review_fail_count
             FROM agent_quality_event
             WHERE event_type = 'review_fail'
               AND agent_id IS NOT NULL AND btrim(agent_id) <> ''
               AND card_id IS NOT NULL AND btrim(card_id) <> ''
               AND created_at >= NOW() - INTERVAL '45 days'
             GROUP BY agent_id, (created_at AT TIME ZONE 'Asia/Seoul')::date, card_id
         ),
         rework_agg AS (
             SELECT agent_id, day, AVG(review_fail_count) AS avg_rework_count
             FROM rework_per_card
             GROUP BY agent_id, day
         ),
         cost_agg AS (
             SELECT agent_id,
                    (created_at AT TIME ZONE 'Asia/Seoul')::date AS day,
                    SUM(COALESCE(NULLIF(payload->>'cost', ''), '0')::double precision) AS cost_total,
                    COUNT(*) FILTER (
                        WHERE event_type = 'card_transitioned'
                          AND payload->>'to' = 'done'
                    )::bigint AS done_card_count
             FROM agent_quality_event
             WHERE agent_id IS NOT NULL AND btrim(agent_id) <> ''
               AND created_at >= NOW() - INTERVAL '45 days'
             GROUP BY agent_id, (created_at AT TIME ZONE 'Asia/Seoul')::date
         ),
         latency_agg AS (
             SELECT agent_id,
                    (created_at AT TIME ZONE 'Asia/Seoul')::date AS day,
                    percentile_cont(0.5) WITHIN GROUP (
                        ORDER BY NULLIF(payload->>'duration_ms', '')::double precision
                    ) AS latency_p50_ms,
                    percentile_cont(0.99) WITHIN GROUP (
                        ORDER BY NULLIF(payload->>'duration_ms', '')::double precision
                    ) AS latency_p99_ms
             FROM agent_quality_event
             WHERE event_type = 'turn_complete'
               AND agent_id IS NOT NULL AND btrim(agent_id) <> ''
               AND payload ? 'duration_ms'
               AND created_at >= NOW() - INTERVAL '45 days'
             GROUP BY agent_id, (created_at AT TIME ZONE 'Asia/Seoul')::date
         ),
         windowed AS (
             SELECT d.agent_id,
                    d.day,
                    d.provider,
                    d.channel_id,
                    d.turn_success_count,
                    d.turn_error_count,
                    d.review_pass_count,
                    d.review_fail_count,
                    d.turn_success_count + d.turn_error_count AS turn_sample_size,
                    d.review_pass_count + d.review_fail_count AS review_sample_size,
                    d.turn_success_count + d.turn_error_count + d.review_pass_count + d.review_fail_count AS sample_size,
                    COALESCE(SUM(w.turn_success_count) FILTER (WHERE w.day >= d.day - 6), 0)::bigint AS turn_success_count_7d,
                    COALESCE(SUM(w.turn_error_count) FILTER (WHERE w.day >= d.day - 6), 0)::bigint AS turn_error_count_7d,
                    COALESCE(SUM(w.review_pass_count) FILTER (WHERE w.day >= d.day - 6), 0)::bigint AS review_pass_count_7d,
                    COALESCE(SUM(w.review_fail_count) FILTER (WHERE w.day >= d.day - 6), 0)::bigint AS review_fail_count_7d,
                    COALESCE(SUM(w.turn_success_count), 0)::bigint AS turn_success_count_30d,
                    COALESCE(SUM(w.turn_error_count), 0)::bigint AS turn_error_count_30d,
                    COALESCE(SUM(w.review_pass_count), 0)::bigint AS review_pass_count_30d,
                    COALESCE(SUM(w.review_fail_count), 0)::bigint AS review_fail_count_30d
             FROM daily_counts d
             JOIN daily_counts w
               ON w.agent_id = d.agent_id
              AND w.day BETWEEN d.day - 29 AND d.day
             GROUP BY d.agent_id,
                      d.day,
                      d.provider,
                      d.channel_id,
                      d.turn_success_count,
                      d.turn_error_count,
                      d.review_pass_count,
                      d.review_fail_count
         ),
         normalized AS (
             SELECT w.*,
                    w.turn_success_count_7d + w.turn_error_count_7d AS turn_sample_size_7d,
                    w.review_pass_count_7d + w.review_fail_count_7d AS review_sample_size_7d,
                    w.turn_success_count_7d + w.turn_error_count_7d + w.review_pass_count_7d + w.review_fail_count_7d AS sample_size_7d,
                    w.turn_success_count_30d + w.turn_error_count_30d AS turn_sample_size_30d,
                    w.review_pass_count_30d + w.review_fail_count_30d AS review_sample_size_30d,
                    w.turn_success_count_30d + w.turn_error_count_30d + w.review_pass_count_30d + w.review_fail_count_30d AS sample_size_30d,
                    r.avg_rework_count,
                    CASE WHEN COALESCE(c.done_card_count, 0) > 0
                         THEN COALESCE(c.cost_total, 0) / c.done_card_count
                         ELSE NULL END AS cost_per_done_card,
                    l.latency_p50_ms,
                    l.latency_p99_ms
             FROM windowed w
             LEFT JOIN rework_agg  r ON r.agent_id = w.agent_id AND r.day = w.day
             LEFT JOIN cost_agg    c ON c.agent_id = w.agent_id AND c.day = w.day
             LEFT JOIN latency_agg l ON l.agent_id = w.agent_id AND l.day = w.day
         ),
         upserted AS (
             INSERT INTO agent_quality_daily (
                 agent_id,
                 day,
                 provider,
                 channel_id,
                 turn_success_count,
                 turn_error_count,
                 review_pass_count,
                 review_fail_count,
                 turn_sample_size,
                 review_sample_size,
                 sample_size,
                 turn_success_rate,
                 review_pass_rate,
                 turn_success_count_7d,
                 turn_error_count_7d,
                 review_pass_count_7d,
                 review_fail_count_7d,
                 turn_sample_size_7d,
                 review_sample_size_7d,
                 sample_size_7d,
                 turn_success_rate_7d,
                 review_pass_rate_7d,
                 measurement_unavailable_7d,
                 turn_success_count_30d,
                 turn_error_count_30d,
                 review_pass_count_30d,
                 review_fail_count_30d,
                 turn_sample_size_30d,
                 review_sample_size_30d,
                 sample_size_30d,
                 turn_success_rate_30d,
                 review_pass_rate_30d,
                 measurement_unavailable_30d,
                 avg_rework_count,
                 cost_per_done_card,
                 latency_p50_ms,
                 latency_p99_ms,
                 computed_at
             )
             SELECT agent_id,
                    day,
                    provider,
                    channel_id,
                    turn_success_count,
                    turn_error_count,
                    review_pass_count,
                    review_fail_count,
                    turn_sample_size,
                    review_sample_size,
                    sample_size,
                    CASE WHEN turn_sample_size > 0 THEN turn_success_count::double precision / turn_sample_size ELSE NULL END,
                    CASE WHEN review_sample_size > 0 THEN review_pass_count::double precision / review_sample_size ELSE NULL END,
                    turn_success_count_7d,
                    turn_error_count_7d,
                    review_pass_count_7d,
                    review_fail_count_7d,
                    turn_sample_size_7d,
                    review_sample_size_7d,
                    sample_size_7d,
                    CASE WHEN turn_sample_size_7d > 0 THEN turn_success_count_7d::double precision / turn_sample_size_7d ELSE NULL END,
                    CASE WHEN review_sample_size_7d > 0 THEN review_pass_count_7d::double precision / review_sample_size_7d ELSE NULL END,
                    sample_size_7d < $1,
                    turn_success_count_30d,
                    turn_error_count_30d,
                    review_pass_count_30d,
                    review_fail_count_30d,
                    turn_sample_size_30d,
                    review_sample_size_30d,
                    sample_size_30d,
                    CASE WHEN turn_sample_size_30d > 0 THEN turn_success_count_30d::double precision / turn_sample_size_30d ELSE NULL END,
                    CASE WHEN review_sample_size_30d > 0 THEN review_pass_count_30d::double precision / review_sample_size_30d ELSE NULL END,
                    sample_size_30d < $1,
                    avg_rework_count,
                    cost_per_done_card,
                    CASE WHEN latency_p50_ms IS NULL THEN NULL ELSE latency_p50_ms::bigint END,
                    CASE WHEN latency_p99_ms IS NULL THEN NULL ELSE latency_p99_ms::bigint END,
                    NOW()
             FROM normalized
             ON CONFLICT (agent_id, day) DO UPDATE SET
                 provider = EXCLUDED.provider,
                 channel_id = EXCLUDED.channel_id,
                 turn_success_count = EXCLUDED.turn_success_count,
                 turn_error_count = EXCLUDED.turn_error_count,
                 review_pass_count = EXCLUDED.review_pass_count,
                 review_fail_count = EXCLUDED.review_fail_count,
                 turn_sample_size = EXCLUDED.turn_sample_size,
                 review_sample_size = EXCLUDED.review_sample_size,
                 sample_size = EXCLUDED.sample_size,
                 turn_success_rate = EXCLUDED.turn_success_rate,
                 review_pass_rate = EXCLUDED.review_pass_rate,
                 turn_success_count_7d = EXCLUDED.turn_success_count_7d,
                 turn_error_count_7d = EXCLUDED.turn_error_count_7d,
                 review_pass_count_7d = EXCLUDED.review_pass_count_7d,
                 review_fail_count_7d = EXCLUDED.review_fail_count_7d,
                 turn_sample_size_7d = EXCLUDED.turn_sample_size_7d,
                 review_sample_size_7d = EXCLUDED.review_sample_size_7d,
                 sample_size_7d = EXCLUDED.sample_size_7d,
                 turn_success_rate_7d = EXCLUDED.turn_success_rate_7d,
                 review_pass_rate_7d = EXCLUDED.review_pass_rate_7d,
                 measurement_unavailable_7d = EXCLUDED.measurement_unavailable_7d,
                 turn_success_count_30d = EXCLUDED.turn_success_count_30d,
                 turn_error_count_30d = EXCLUDED.turn_error_count_30d,
                 review_pass_count_30d = EXCLUDED.review_pass_count_30d,
                 review_fail_count_30d = EXCLUDED.review_fail_count_30d,
                 turn_sample_size_30d = EXCLUDED.turn_sample_size_30d,
                 review_sample_size_30d = EXCLUDED.review_sample_size_30d,
                 sample_size_30d = EXCLUDED.sample_size_30d,
                 turn_success_rate_30d = EXCLUDED.turn_success_rate_30d,
                 review_pass_rate_30d = EXCLUDED.review_pass_rate_30d,
                 measurement_unavailable_30d = EXCLUDED.measurement_unavailable_30d,
                 avg_rework_count = EXCLUDED.avg_rework_count,
                 cost_per_done_card = EXCLUDED.cost_per_done_card,
                 latency_p50_ms = EXCLUDED.latency_p50_ms,
                 latency_p99_ms = EXCLUDED.latency_p99_ms,
                 computed_at = EXCLUDED.computed_at
             RETURNING 1
         )
         SELECT COUNT(*) FROM upserted",
    )
    .bind(QUALITY_SAMPLE_GUARD)
    .fetch_one(pool)
    .await
    .map_err(|error| anyhow!("upsert postgres agent quality daily: {error}"))?;

    Ok(row_count.max(0) as u64)
}

/// #1102 event-based fallback (postgres): synthesize per-day records from
/// `agent_quality_event` when the daily rollup table is empty for this
/// agent. Only counts turn/review success/fail events, rolling windows are
/// computed in-memory over the returned days.
async fn synth_agent_quality_daily_from_events_pg(
    pool: &PgPool,
    agent_id: &str,
    days: i64,
) -> Result<Vec<AgentQualityDailyRecord>> {
    let days = days.clamp(1, MAX_QUALITY_DAYS);
    let rows = sqlx::query(
        "SELECT to_char((created_at AT TIME ZONE 'Asia/Seoul')::date, 'YYYY-MM-DD') AS day_text,
                MAX(provider) FILTER (WHERE provider IS NOT NULL AND btrim(provider) <> '') AS provider,
                MAX(channel_id) FILTER (WHERE channel_id IS NOT NULL AND btrim(channel_id) <> '') AS channel_id,
                COUNT(*) FILTER (WHERE event_type = 'turn_complete')::bigint AS turn_success_count,
                COUNT(*) FILTER (WHERE event_type = 'turn_error')::bigint AS turn_error_count,
                COUNT(*) FILTER (WHERE event_type = 'review_pass')::bigint AS review_pass_count,
                COUNT(*) FILTER (WHERE event_type = 'review_fail')::bigint AS review_fail_count
         FROM agent_quality_event
         WHERE agent_id = $1
           AND created_at >= NOW() - ($2::bigint || ' days')::interval
         GROUP BY day_text
         ORDER BY day_text DESC",
    )
    .bind(agent_id)
    .bind(days)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("synth agent quality daily (pg): {error}"))?;

    let buckets: Vec<SynthDailyBucket> = rows
        .into_iter()
        .map(|row| {
            Ok::<_, anyhow::Error>(SynthDailyBucket {
                day: row
                    .try_get::<String, _>("day_text")
                    .map_err(|e| anyhow!("decode synth day: {e}"))?,
                provider: row.try_get::<Option<String>, _>("provider").ok().flatten(),
                channel_id: row
                    .try_get::<Option<String>, _>("channel_id")
                    .ok()
                    .flatten(),
                turn_success: row
                    .try_get::<i64, _>("turn_success_count")
                    .unwrap_or(0)
                    .max(0),
                turn_error: row
                    .try_get::<i64, _>("turn_error_count")
                    .unwrap_or(0)
                    .max(0),
                review_pass: row
                    .try_get::<i64, _>("review_pass_count")
                    .unwrap_or(0)
                    .max(0),
                review_fail: row
                    .try_get::<i64, _>("review_fail_count")
                    .unwrap_or(0)
                    .max(0),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(buckets_to_synth_records(agent_id, buckets))
}

#[derive(Debug, Clone)]
struct SynthDailyBucket {
    day: String,
    provider: Option<String>,
    channel_id: Option<String>,
    turn_success: i64,
    turn_error: i64,
    review_pass: i64,
    review_fail: i64,
}

fn buckets_to_synth_records(
    agent_id: &str,
    buckets: Vec<SynthDailyBucket>,
) -> Vec<AgentQualityDailyRecord> {
    let now = now_kst();
    // #2049 Finding 6: index buckets by calendar date so rolling windows sum
    // over `target_day - N + 1 ..= target_day` (calendar-day semantics),
    // matching the SQL `upsert_agent_quality_daily_pg` rollup. Previously the
    // window walked `buckets[idx..idx+N]` which used "Nth event-having day"
    // — agents that only had events on 5 of the last 30 days would have
    // their `rolling_7d` silently aggregate over 25+ calendar days.
    let indexed: BTreeMap<chrono::NaiveDate, &SynthDailyBucket> = buckets
        .iter()
        .filter_map(|b| {
            chrono::NaiveDate::parse_from_str(&b.day, "%Y-%m-%d")
                .ok()
                .map(|d| (d, b))
        })
        .collect();
    buckets
        .iter()
        .map(|bucket| {
            let target_day = chrono::NaiveDate::parse_from_str(&bucket.day, "%Y-%m-%d").ok();
            let window = |size: i64| -> (i64, i64, i64, i64) {
                let Some(target) = target_day else {
                    return (
                        bucket.turn_success,
                        bucket.turn_error,
                        bucket.review_pass,
                        bucket.review_fail,
                    );
                };
                let start = target - chrono::Duration::days(size - 1);
                let mut ts = 0i64;
                let mut te = 0i64;
                let mut rp = 0i64;
                let mut rf = 0i64;
                for (_, b) in indexed.range(start..=target) {
                    ts += b.turn_success;
                    te += b.turn_error;
                    rp += b.review_pass;
                    rf += b.review_fail;
                }
                (ts, te, rp, rf)
            };
            let (ts7, te7, rp7, rf7) = window(7);
            let (ts30, te30, rp30, rf30) = window(30);
            let turn_sample = bucket.turn_success + bucket.turn_error;
            let review_sample = bucket.review_pass + bucket.review_fail;
            let sample = turn_sample + review_sample;
            let turn_rate = if turn_sample > 0 {
                Some(bucket.turn_success as f64 / turn_sample as f64)
            } else {
                None
            };
            let review_rate = if review_sample > 0 {
                Some(bucket.review_pass as f64 / review_sample as f64)
            } else {
                None
            };
            let turn_sample_7d = ts7 + te7;
            let review_sample_7d = rp7 + rf7;
            let sample_7d = turn_sample_7d + review_sample_7d;
            let turn_sample_30d = ts30 + te30;
            let review_sample_30d = rp30 + rf30;
            let sample_30d = turn_sample_30d + review_sample_30d;
            let unavailable_7d = sample_7d < QUALITY_SAMPLE_GUARD;
            let unavailable_30d = sample_30d < QUALITY_SAMPLE_GUARD;
            AgentQualityDailyRecord {
                agent_id: agent_id.to_string(),
                day: bucket.day.clone(),
                provider: bucket.provider.clone(),
                channel_id: bucket.channel_id.clone(),
                turn_success_count: bucket.turn_success,
                turn_error_count: bucket.turn_error,
                review_pass_count: bucket.review_pass,
                review_fail_count: bucket.review_fail,
                turn_sample_size: turn_sample,
                review_sample_size: review_sample,
                sample_size: sample,
                turn_success_rate: turn_rate,
                review_pass_rate: review_rate,
                rolling_7d: quality_window(
                    7,
                    sample_7d,
                    unavailable_7d,
                    turn_sample_7d,
                    if turn_sample_7d > 0 {
                        Some(ts7 as f64 / turn_sample_7d as f64)
                    } else {
                        None
                    },
                    review_sample_7d,
                    if review_sample_7d > 0 {
                        Some(rp7 as f64 / review_sample_7d as f64)
                    } else {
                        None
                    },
                ),
                rolling_30d: quality_window(
                    30,
                    sample_30d,
                    unavailable_30d,
                    turn_sample_30d,
                    if turn_sample_30d > 0 {
                        Some(ts30 as f64 / turn_sample_30d as f64)
                    } else {
                        None
                    },
                    review_sample_30d,
                    if review_sample_30d > 0 {
                        Some(rp30 as f64 / review_sample_30d as f64)
                    } else {
                        None
                    },
                ),
                computed_at: now.clone(),
            }
        })
        .collect()
}

fn measurement_label(unavailable: bool) -> Option<String> {
    unavailable.then(|| "측정 불가".to_string())
}

fn quality_window(
    days: i64,
    sample_size: i64,
    measurement_unavailable: bool,
    turn_sample_size: i64,
    turn_success_rate: Option<f64>,
    review_sample_size: i64,
    review_pass_rate: Option<f64>,
) -> AgentQualityWindow {
    AgentQualityWindow {
        days,
        sample_size: sample_size.max(0),
        measurement_unavailable,
        measurement_label: measurement_label(measurement_unavailable),
        turn_sample_size: turn_sample_size.max(0),
        turn_success_rate,
        review_sample_size: review_sample_size.max(0),
        review_pass_rate,
    }
}

fn quality_daily_record_from_pg_row(
    row: &sqlx::postgres::PgRow,
) -> Result<AgentQualityDailyRecord> {
    let measurement_unavailable_7d = row
        .try_get::<bool, _>("measurement_unavailable_7d")
        .map_err(|error| anyhow!("decode measurement_unavailable_7d: {error}"))?;
    let measurement_unavailable_30d = row
        .try_get::<bool, _>("measurement_unavailable_30d")
        .map_err(|error| anyhow!("decode measurement_unavailable_30d: {error}"))?;
    Ok(AgentQualityDailyRecord {
        agent_id: row
            .try_get("agent_id")
            .map_err(|error| anyhow!("decode agent quality daily agent_id: {error}"))?,
        day: row
            .try_get("day_text")
            .map_err(|error| anyhow!("decode agent quality daily day: {error}"))?,
        provider: row
            .try_get("provider")
            .map_err(|error| anyhow!("decode agent quality daily provider: {error}"))?,
        channel_id: row
            .try_get("channel_id")
            .map_err(|error| anyhow!("decode agent quality daily channel_id: {error}"))?,
        turn_success_count: row
            .try_get::<i64, _>("turn_success_count")
            .map_err(|error| anyhow!("decode turn_success_count: {error}"))?
            .max(0),
        turn_error_count: row
            .try_get::<i64, _>("turn_error_count")
            .map_err(|error| anyhow!("decode turn_error_count: {error}"))?
            .max(0),
        review_pass_count: row
            .try_get::<i64, _>("review_pass_count")
            .map_err(|error| anyhow!("decode review_pass_count: {error}"))?
            .max(0),
        review_fail_count: row
            .try_get::<i64, _>("review_fail_count")
            .map_err(|error| anyhow!("decode review_fail_count: {error}"))?
            .max(0),
        turn_sample_size: row
            .try_get::<i64, _>("turn_sample_size")
            .map_err(|error| anyhow!("decode turn_sample_size: {error}"))?
            .max(0),
        review_sample_size: row
            .try_get::<i64, _>("review_sample_size")
            .map_err(|error| anyhow!("decode review_sample_size: {error}"))?
            .max(0),
        sample_size: row
            .try_get::<i64, _>("sample_size")
            .map_err(|error| anyhow!("decode sample_size: {error}"))?
            .max(0),
        turn_success_rate: row
            .try_get("turn_success_rate")
            .map_err(|error| anyhow!("decode turn_success_rate: {error}"))?,
        review_pass_rate: row
            .try_get("review_pass_rate")
            .map_err(|error| anyhow!("decode review_pass_rate: {error}"))?,
        rolling_7d: quality_window(
            7,
            row.try_get("sample_size_7d")
                .map_err(|error| anyhow!("decode sample_size_7d: {error}"))?,
            measurement_unavailable_7d,
            row.try_get("turn_sample_size_7d")
                .map_err(|error| anyhow!("decode turn_sample_size_7d: {error}"))?,
            row.try_get("turn_success_rate_7d")
                .map_err(|error| anyhow!("decode turn_success_rate_7d: {error}"))?,
            row.try_get("review_sample_size_7d")
                .map_err(|error| anyhow!("decode review_sample_size_7d: {error}"))?,
            row.try_get("review_pass_rate_7d")
                .map_err(|error| anyhow!("decode review_pass_rate_7d: {error}"))?,
        ),
        rolling_30d: quality_window(
            30,
            row.try_get("sample_size_30d")
                .map_err(|error| anyhow!("decode sample_size_30d: {error}"))?,
            measurement_unavailable_30d,
            row.try_get("turn_sample_size_30d")
                .map_err(|error| anyhow!("decode turn_sample_size_30d: {error}"))?,
            row.try_get("turn_success_rate_30d")
                .map_err(|error| anyhow!("decode turn_success_rate_30d: {error}"))?,
            row.try_get("review_sample_size_30d")
                .map_err(|error| anyhow!("decode review_sample_size_30d: {error}"))?,
            row.try_get("review_pass_rate_30d")
                .map_err(|error| anyhow!("decode review_pass_rate_30d: {error}"))?,
        ),
        computed_at: row
            .try_get("computed_at_kst")
            .map_err(|error| anyhow!("decode computed_at_kst: {error}"))?,
    })
}

fn agent_quality_daily_select_pg() -> &'static str {
    "SELECT agent_id,
            to_char(day, 'YYYY-MM-DD') AS day_text,
            provider,
            channel_id,
            turn_success_count,
            turn_error_count,
            review_pass_count,
            review_fail_count,
            turn_sample_size,
            review_sample_size,
            sample_size,
            turn_success_rate,
            review_pass_rate,
            turn_sample_size_7d,
            sample_size_7d,
            turn_success_rate_7d,
            review_sample_size_7d,
            review_pass_rate_7d,
            measurement_unavailable_7d,
            turn_sample_size_30d,
            sample_size_30d,
            turn_success_rate_30d,
            review_sample_size_30d,
            review_pass_rate_30d,
            measurement_unavailable_30d,
            to_char(computed_at AT TIME ZONE 'Asia/Seoul', 'YYYY-MM-DD HH24:MI:SS') AS computed_at_kst
     FROM agent_quality_daily"
}

async fn query_agent_quality_daily_pg(
    pool: &PgPool,
    agent_id: Option<&str>,
    days: i64,
    limit: usize,
) -> Result<Vec<AgentQualityDailyRecord>> {
    let sql = format!(
        "{} WHERE ($1::text IS NULL OR agent_id = $1)
              AND day >= (CURRENT_DATE - $2::int)
            ORDER BY day DESC, agent_id ASC
            LIMIT $3",
        agent_quality_daily_select_pg()
    );
    let rows = sqlx::query(&sql)
        .bind(agent_id)
        .bind(days as i32)
        .bind(limit as i64)
        .fetch_all(pool)
        .await
        .map_err(|error| anyhow!("query postgres agent quality daily: {error}"))?;

    rows.iter().map(quality_daily_record_from_pg_row).collect()
}

fn quality_ranking_entry_from_daily(
    rank: i64,
    record: AgentQualityDailyRecord,
    agent_name: Option<String>,
) -> AgentQualityRankingEntry {
    AgentQualityRankingEntry {
        rank,
        agent_id: record.agent_id,
        agent_name,
        provider: record.provider,
        channel_id: record.channel_id,
        latest_day: record.day,
        rolling_7d: record.rolling_7d,
        rolling_30d: record.rolling_30d,
        metric_value: None,
    }
}

/// Pick the metric value for a ranking entry given the (metric, window)
/// pair. Returns `None` when the window is measurement-unavailable or the
/// underlying rate is NULL.
fn pick_ranking_metric_value(
    entry: &AgentQualityRankingEntry,
    metric: QualityRankingMetric,
    window: QualityRankingWindow,
) -> Option<f64> {
    let win = match window {
        QualityRankingWindow::Seven => &entry.rolling_7d,
        QualityRankingWindow::Thirty => &entry.rolling_30d,
    };
    if win.measurement_unavailable {
        return None;
    }
    match metric {
        QualityRankingMetric::TurnSuccessRate => win.turn_success_rate,
        QualityRankingMetric::ReviewPassRate => win.review_pass_rate,
    }
}

/// Return the sample size for the requested window on a ranking entry.
fn ranking_window_sample_size(
    entry: &AgentQualityRankingEntry,
    window: QualityRankingWindow,
) -> i64 {
    match window {
        QualityRankingWindow::Seven => entry.rolling_7d.sample_size,
        QualityRankingWindow::Thirty => entry.rolling_30d.sample_size,
    }
}

async fn query_agent_quality_ranking_pg(
    pool: &PgPool,
    limit: usize,
) -> Result<Vec<AgentQualityRankingEntry>> {
    let sql = format!(
        "WITH latest AS (
             SELECT DISTINCT ON (agent_id) *
             FROM agent_quality_daily
             ORDER BY agent_id, day DESC
         ),
         ranked AS (
             SELECT row_number() OVER (
                        ORDER BY measurement_unavailable_7d ASC,
                                 turn_success_rate_7d DESC NULLS LAST,
                                 review_pass_rate_7d DESC NULLS LAST,
                                 sample_size_7d DESC,
                                 agent_id ASC
                    )::bigint AS rank,
                    latest.*,
                    COALESCE(a.name_ko, a.name) AS agent_name
             FROM latest
             LEFT JOIN agents a
               ON a.id = latest.agent_id
         )
         SELECT rank,
                agent_id,
                to_char(day, 'YYYY-MM-DD') AS day_text,
                provider,
                channel_id,
                turn_success_count,
                turn_error_count,
                review_pass_count,
                review_fail_count,
                turn_sample_size,
                review_sample_size,
                sample_size,
                turn_success_rate,
                review_pass_rate,
                turn_sample_size_7d,
                sample_size_7d,
                turn_success_rate_7d,
                review_sample_size_7d,
                review_pass_rate_7d,
                measurement_unavailable_7d,
                turn_sample_size_30d,
                sample_size_30d,
                turn_success_rate_30d,
                review_sample_size_30d,
                review_pass_rate_30d,
                measurement_unavailable_30d,
                to_char(computed_at AT TIME ZONE 'Asia/Seoul', 'YYYY-MM-DD HH24:MI:SS') AS computed_at_kst,
                agent_name
         FROM ranked
         ORDER BY rank ASC
         LIMIT $1"
    );
    let rows = sqlx::query(&sql)
        .bind(limit as i64)
        .fetch_all(pool)
        .await
        .map_err(|error| anyhow!("query postgres agent quality ranking: {error}"))?;

    rows.iter()
        .map(|row| {
            let rank = row
                .try_get::<i64, _>("rank")
                .map_err(|error| anyhow!("decode quality rank: {error}"))?;
            let agent_name = row
                .try_get("agent_name")
                .map_err(|error| anyhow!("decode quality ranking agent_name: {error}"))?;
            Ok(quality_ranking_entry_from_daily(
                rank,
                quality_daily_record_from_pg_row(row)?,
                agent_name,
            ))
        })
        .collect()
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

async fn quality_alert_target_pg(pool: &PgPool) -> Result<Option<String>> {
    let value = sqlx::query_scalar::<_, String>(
        "SELECT value
         FROM kv_meta
         WHERE key IN ('agent_quality_monitoring_channel_id', 'kanban_human_alert_channel_id')
           AND value IS NOT NULL
           AND btrim(value) <> ''
         ORDER BY CASE key
                      WHEN 'agent_quality_monitoring_channel_id' THEN 0
                      ELSE 1
                  END
         LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow!("load agent quality alert target: {error}"))?;
    Ok(value.as_deref().and_then(normalize_channel_target))
}

#[allow(dead_code)] // #2049 Finding 7: superseded by claim_quality_alert_slot_pg.
                    // Kept for unit tests and operator-facing ad-hoc tooling.
async fn quality_alert_recently_sent_pg(pool: &PgPool, key: &str, now_ms: i64) -> Result<bool> {
    let last_ms =
        sqlx::query_scalar::<_, Option<String>>("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
            .bind(key)
            .fetch_optional(pool)
            .await
            .map_err(|error| anyhow!("load quality alert dedupe key {key}: {error}"))?
            .flatten()
            .and_then(|value| value.parse::<i64>().ok());
    Ok(last_ms.is_some_and(|last_ms| now_ms.saturating_sub(last_ms) < QUALITY_ALERT_DEDUPE_MS))
}

#[allow(dead_code)] // #2049 Finding 7: superseded by claim_quality_alert_slot_pg.
async fn mark_quality_alert_sent_pg(pool: &PgPool, key: &str, now_ms: i64) -> Result<()> {
    sqlx::query(
        "INSERT INTO kv_meta (key, value)
         VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(key)
    .bind(now_ms.to_string())
    .execute(pool)
    .await
    .map_err(|error| anyhow!("mark quality alert dedupe key {key}: {error}"))?;
    Ok(())
}

/// #2049 Finding 7: atomically claim the dedupe slot for `key` if and only if
/// the previous claim is older than `QUALITY_ALERT_DEDUPE_MS`. Returns `true`
/// when this caller won the race and may proceed to enqueue the outbox row;
/// `false` when another concurrent rollup already claimed the slot. Compresses
/// the prior `quality_alert_recently_sent_pg` + `mark_quality_alert_sent_pg`
/// two-step (TOCTOU) into a single statement using `INSERT ... ON CONFLICT ...
/// WHERE`.
async fn claim_quality_alert_slot_pg(pool: &PgPool, key: &str, now_ms: i64) -> Result<bool> {
    let dedupe_ms = QUALITY_ALERT_DEDUPE_MS;
    // Defensive cast: kv_meta.value is shared by other writers and may hold
    // non-numeric legacy strings (operator scripts, older code). A raw
    // `value::bigint` cast would raise `invalid input syntax for type bigint`
    // and break dedupe entirely for the alert key (#2049 F7 review). Match
    // `^[0-9]+$` first and treat non-numeric values as "no prior claim" so
    // the UPDATE proceeds and writes a fresh numeric stamp.
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
    .map_err(|error| anyhow!("claim quality alert dedupe key {key}: {error}"))?;
    Ok(claimed.is_some())
}

/// #2049 Finding 7: best-effort rollback for `claim_quality_alert_slot_pg`
/// when the subsequent outbox INSERT fails. Deletes the freshly-claimed
/// dedupe row so the next rollup cycle can retry.
async fn release_quality_alert_slot_pg(pool: &PgPool, key: &str) -> Result<()> {
    sqlx::query("DELETE FROM kv_meta WHERE key = $1")
        .bind(key)
        .execute(pool)
        .await
        .map_err(|error| anyhow!("release quality alert dedupe key {key}: {error}"))?;
    Ok(())
}

fn format_rate_for_alert(value: f64) -> String {
    format!("{:.1}%", value * 100.0)
}

fn quality_alert_content(
    agent_id: &str,
    metric_label: &str,
    rate_7d: f64,
    rate_30d: f64,
    sample_7d: i64,
    sample_30d: i64,
) -> String {
    let drop_points = (rate_30d - rate_7d) * 100.0;
    format!(
        "에이전트 품질 회귀 감지: `{agent_id}` {metric_label} 7d {} / 30d {} ({drop_points:.1}%p 하락, sample {sample_7d}/{sample_30d})",
        format_rate_for_alert(rate_7d),
        format_rate_for_alert(rate_30d),
    )
}

async fn enqueue_quality_alert_pg(
    pool: &PgPool,
    target: &str,
    dedupe_key: &str,
    content: &str,
    now_ms: i64,
) -> Result<bool> {
    // #2049 Finding 7: claim the dedupe slot atomically *before* enqueueing the
    // outbox row. Previously (SELECT, ENQUEUE, MARK) could interleave across
    // concurrent rollup callers and double-post the regression alert.
    if !claim_quality_alert_slot_pg(pool, dedupe_key, now_ms).await? {
        return Ok(false);
    }

    let enqueued = match crate::services::message_outbox::enqueue_outbox_pg(
        pool,
        crate::services::message_outbox::OutboxMessage {
            target,
            content,
            bot: "notify",
            source: "agent_quality_rollup",
            reason_code: Some("agent_quality.regression"),
            session_key: Some(dedupe_key),
        },
    )
    .await
    {
        Ok(enqueued) => enqueued,
        Err(error) => {
            // Slot was claimed but outbox INSERT failed: roll back the claim
            // so we don't suppress the next dedupe window.
            if let Err(rollback_err) = release_quality_alert_slot_pg(pool, dedupe_key).await {
                tracing::warn!(
                    "[quality] failed to release dedupe slot {dedupe_key} after outbox error: {rollback_err}"
                );
            }
            return Err(anyhow!("enqueue quality regression alert: {error}"));
        }
    };

    Ok(enqueued)
}

async fn enqueue_quality_regression_alerts_pg(pool: &PgPool) -> Result<u64> {
    let Some(target) = quality_alert_target_pg(pool).await? else {
        return Ok(0);
    };

    let rows = sqlx::query(
        "WITH latest AS (
             SELECT DISTINCT ON (agent_id)
                    agent_id,
                    day,
                    turn_success_rate_7d,
                    turn_success_rate_30d,
                    review_pass_rate_7d,
                    review_pass_rate_30d,
                    turn_sample_size_7d,
                    turn_sample_size_30d,
                    review_sample_size_7d,
                    review_sample_size_30d,
                    measurement_unavailable_7d,
                    measurement_unavailable_30d
             FROM agent_quality_daily
             ORDER BY agent_id, day DESC
         )
         SELECT *
         FROM latest
         WHERE measurement_unavailable_7d = FALSE
           AND measurement_unavailable_30d = FALSE
           AND (
               (review_pass_rate_7d IS NOT NULL
                AND review_pass_rate_30d IS NOT NULL
                AND review_pass_rate_30d - review_pass_rate_7d > $1)
            OR (turn_success_rate_7d IS NOT NULL
                AND turn_success_rate_30d IS NOT NULL
                AND turn_success_rate_30d - turn_success_rate_7d > $2)
           )",
    )
    .bind(QUALITY_REVIEW_DROP_THRESHOLD)
    .bind(QUALITY_TURN_DROP_THRESHOLD)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("query quality regression alert candidates: {error}"))?;

    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut alert_count = 0u64;
    for row in rows {
        let agent_id: String = row
            .try_get("agent_id")
            .map_err(|error| anyhow!("decode alert agent_id: {error}"))?;
        let review_7d: Option<f64> = row
            .try_get("review_pass_rate_7d")
            .map_err(|error| anyhow!("decode alert review_pass_rate_7d: {error}"))?;
        let review_30d: Option<f64> = row
            .try_get("review_pass_rate_30d")
            .map_err(|error| anyhow!("decode alert review_pass_rate_30d: {error}"))?;
        let turn_7d: Option<f64> = row
            .try_get("turn_success_rate_7d")
            .map_err(|error| anyhow!("decode alert turn_success_rate_7d: {error}"))?;
        let turn_30d: Option<f64> = row
            .try_get("turn_success_rate_30d")
            .map_err(|error| anyhow!("decode alert turn_success_rate_30d: {error}"))?;
        let review_sample_7d: i64 = row
            .try_get("review_sample_size_7d")
            .map_err(|error| anyhow!("decode alert review_sample_size_7d: {error}"))?;
        let review_sample_30d: i64 = row
            .try_get("review_sample_size_30d")
            .map_err(|error| anyhow!("decode alert review_sample_size_30d: {error}"))?;
        let turn_sample_7d: i64 = row
            .try_get("turn_sample_size_7d")
            .map_err(|error| anyhow!("decode alert turn_sample_size_7d: {error}"))?;
        let turn_sample_30d: i64 = row
            .try_get("turn_sample_size_30d")
            .map_err(|error| anyhow!("decode alert turn_sample_size_30d: {error}"))?;

        if let (Some(rate_7d), Some(rate_30d)) = (review_7d, review_30d)
            && rate_30d - rate_7d > QUALITY_REVIEW_DROP_THRESHOLD
        {
            let key = format!("agent_quality_alert:{agent_id}:review_pass_rate");
            let content = quality_alert_content(
                &agent_id,
                "review pass rate",
                rate_7d,
                rate_30d,
                review_sample_7d,
                review_sample_30d,
            );
            if enqueue_quality_alert_pg(pool, &target, &key, &content, now_ms).await? {
                alert_count = alert_count.saturating_add(1);
            }
        }

        if let (Some(rate_7d), Some(rate_30d)) = (turn_7d, turn_30d)
            && rate_30d - rate_7d > QUALITY_TURN_DROP_THRESHOLD
        {
            let key = format!("agent_quality_alert:{agent_id}:turn_success_rate");
            let content = quality_alert_content(
                &agent_id,
                "turn success rate",
                rate_7d,
                rate_30d,
                turn_sample_7d,
                turn_sample_30d,
            );
            if enqueue_quality_alert_pg(pool, &target, &key, &content, now_ms).await? {
                alert_count = alert_count.saturating_add(1);
            }
        }
    }

    Ok(alert_count)
}

async fn query_counter_snapshots_pg(
    pool: &PgPool,
    filters: &AnalyticsFilters,
    limit: usize,
) -> Result<Vec<AnalyticsCounterSnapshot>> {
    let rows = sqlx::query(
        "SELECT provider,
                channel_id,
                turn_attempts,
                guard_fires,
                watcher_replacements,
                recovery_fires,
                turn_successes,
                turn_failures,
                to_char(snapshot_at AT TIME ZONE 'Asia/Seoul', 'YYYY-MM-DD HH24:MI:SS') AS snapshot_at_kst
         FROM (
             SELECT DISTINCT ON (provider, channel_id)
                    provider,
                    channel_id,
                    turn_attempts,
                    guard_fires,
                    watcher_replacements,
                    recovery_fires,
                    turn_successes,
                    turn_failures,
                    snapshot_at,
                    id
             FROM observability_counter_snapshots
             WHERE ($1::text IS NULL OR provider = $1)
               AND ($2::text IS NULL OR channel_id = $2)
             ORDER BY provider, channel_id, id DESC
         ) snapshots
         ORDER BY turn_attempts DESC, provider, channel_id
         LIMIT $3",
    )
    .bind(filters.provider.as_deref())
    .bind(filters.channel_id.as_deref())
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("query postgres observability snapshots: {error}"))?;

    rows.into_iter()
        .map(|row| {
            let values = CounterValues {
                turn_attempts: row
                    .try_get::<i64, _>("turn_attempts")
                    .map_err(|error| anyhow!("decode turn_attempts: {error}"))?
                    .max(0) as u64,
                guard_fires: row
                    .try_get::<i64, _>("guard_fires")
                    .map_err(|error| anyhow!("decode guard_fires: {error}"))?
                    .max(0) as u64,
                watcher_replacements: row
                    .try_get::<i64, _>("watcher_replacements")
                    .map_err(|error| anyhow!("decode watcher_replacements: {error}"))?
                    .max(0) as u64,
                recovery_fires: row
                    .try_get::<i64, _>("recovery_fires")
                    .map_err(|error| anyhow!("decode recovery_fires: {error}"))?
                    .max(0) as u64,
                turn_successes: row
                    .try_get::<i64, _>("turn_successes")
                    .map_err(|error| anyhow!("decode turn_successes: {error}"))?
                    .max(0) as u64,
                turn_failures: row
                    .try_get::<i64, _>("turn_failures")
                    .map_err(|error| anyhow!("decode turn_failures: {error}"))?
                    .max(0) as u64,
            };
            Ok(counter_snapshot_from_values(
                &row.try_get::<String, _>("provider")
                    .map_err(|error| anyhow!("decode snapshot provider: {error}"))?,
                &row.try_get::<String, _>("channel_id")
                    .map_err(|error| anyhow!("decode snapshot channel_id: {error}"))?,
                values,
                "persisted",
                row.try_get::<String, _>("snapshot_at_kst")
                    .map_err(|error| anyhow!("decode snapshot_at: {error}"))?,
            ))
        })
        .collect()
}

async fn insert_events_pg(pool: &PgPool, events: &[QueuedEvent]) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| anyhow!("begin postgres observability event tx: {error}"))?;
    for event in events {
        sqlx::query(
            "INSERT INTO observability_events (
                event_type,
                provider,
                channel_id,
                dispatch_id,
                session_key,
                turn_id,
                status,
                payload_json
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, CAST($8 AS jsonb))",
        )
        .bind(&event.event_type)
        .bind(&event.provider)
        .bind(&event.channel_id)
        .bind(&event.dispatch_id)
        .bind(&event.session_key)
        .bind(&event.turn_id)
        .bind(&event.status)
        .bind(&event.payload_json)
        .execute(&mut *tx)
        .await
        .map_err(|error| anyhow!("insert postgres observability event: {error}"))?;
    }
    tx.commit()
        .await
        .map_err(|error| anyhow!("commit postgres observability event tx: {error}"))?;
    Ok(())
}

/// #2049 Finding 1 / Finding 5: When bulk insert rolls back the whole tx,
/// re-attempt each row in its own short tx so good rows still land. Returns
/// the rows that still failed (candidates for dead-letter JSONL or queue
/// push-back).
async fn insert_events_pg_row_isolated(
    pool: &PgPool,
    events: &[QueuedEvent],
) -> Vec<QueuedEvent> {
    let mut failed = Vec::new();
    for event in events {
        let result = sqlx::query(
            "INSERT INTO observability_events (
                event_type,
                provider,
                channel_id,
                dispatch_id,
                session_key,
                turn_id,
                status,
                payload_json
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, CAST($8 AS jsonb))",
        )
        .bind(&event.event_type)
        .bind(&event.provider)
        .bind(&event.channel_id)
        .bind(&event.dispatch_id)
        .bind(&event.session_key)
        .bind(&event.turn_id)
        .bind(&event.status)
        .bind(&event.payload_json)
        .execute(pool)
        .await;
        if let Err(error) = result {
            tracing::warn!(
                "[observability] per-row insert failed (event_type={}, dispatch_id={:?}): {error}",
                event.event_type,
                event.dispatch_id
            );
            failed.push(event.clone());
        }
    }
    failed
}

async fn insert_quality_events_pg(pool: &PgPool, events: &[QueuedQualityEvent]) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| anyhow!("begin postgres agent quality event tx: {error}"))?;
    for event in events {
        sqlx::query(
            "INSERT INTO agent_quality_event (
                source_event_id,
                correlation_id,
                agent_id,
                provider,
                channel_id,
                card_id,
                dispatch_id,
                event_type,
                payload
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8::agent_quality_event_type, CAST($9 AS jsonb))",
        )
        .bind(&event.source_event_id)
        .bind(&event.correlation_id)
        .bind(&event.agent_id)
        .bind(&event.provider)
        .bind(&event.channel_id)
        .bind(&event.card_id)
        .bind(&event.dispatch_id)
        .bind(&event.event_type)
        .bind(&event.payload_json)
        .execute(&mut *tx)
        .await
        .map_err(|error| anyhow!("insert postgres agent quality event: {error}"))?;
    }
    tx.commit()
        .await
        .map_err(|error| anyhow!("commit postgres agent quality event tx: {error}"))?;
    Ok(())
}

/// #2049 Finding 1 / Finding 5: Row-isolated retry for `agent_quality_event`
/// so one enum-cast failure cannot lose the whole 64-row batch.
async fn insert_quality_events_pg_row_isolated(
    pool: &PgPool,
    events: &[QueuedQualityEvent],
) -> Vec<QueuedQualityEvent> {
    let mut failed = Vec::new();
    for event in events {
        let result = sqlx::query(
            "INSERT INTO agent_quality_event (
                source_event_id,
                correlation_id,
                agent_id,
                provider,
                channel_id,
                card_id,
                dispatch_id,
                event_type,
                payload
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8::agent_quality_event_type, CAST($9 AS jsonb))",
        )
        .bind(&event.source_event_id)
        .bind(&event.correlation_id)
        .bind(&event.agent_id)
        .bind(&event.provider)
        .bind(&event.channel_id)
        .bind(&event.card_id)
        .bind(&event.dispatch_id)
        .bind(&event.event_type)
        .bind(&event.payload_json)
        .execute(pool)
        .await;
        if let Err(error) = result {
            tracing::warn!(
                "[quality] per-row insert failed (event_type={}, agent_id={:?}): {error}",
                event.event_type,
                event.agent_id
            );
            failed.push(event.clone());
        }
    }
    failed
}

async fn insert_snapshots_pg(pool: &PgPool, snapshots: &[SnapshotRow]) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| anyhow!("begin postgres observability snapshot tx: {error}"))?;
    for snapshot in snapshots {
        sqlx::query(
            "INSERT INTO observability_counter_snapshots (
                provider,
                channel_id,
                turn_attempts,
                guard_fires,
                watcher_replacements,
                recovery_fires,
                turn_successes,
                turn_failures
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&snapshot.provider)
        .bind(&snapshot.channel_id)
        .bind(saturating_i64(snapshot.values.turn_attempts))
        .bind(saturating_i64(snapshot.values.guard_fires))
        .bind(saturating_i64(snapshot.values.watcher_replacements))
        .bind(saturating_i64(snapshot.values.recovery_fires))
        .bind(saturating_i64(snapshot.values.turn_successes))
        .bind(saturating_i64(snapshot.values.turn_failures))
        .execute(&mut *tx)
        .await
        .map_err(|error| anyhow!("insert postgres observability snapshot: {error}"))?;
    }
    tx.commit()
        .await
        .map_err(|error| anyhow!("commit postgres observability snapshot tx: {error}"))?;
    Ok(())
}

fn counter_snapshot_from_values(
    provider: &str,
    channel_id: &str,
    values: CounterValues,
    source: &str,
    snapshot_at: String,
) -> AnalyticsCounterSnapshot {
    // #2049 Finding 20: divide by the *actual* observation count and elide the
    // ratio when no successes/failures observed instead of saturating the
    // denominator to 1. The previous formulation produced "100% of 0" when
    // emit_turn_started was lost but emit_turn_finished still fired (race),
    // which misled the dashboards. Using `successes + failures` as the
    // denominator keeps the rates self-consistent even when `turn_attempts`
    // is briefly skewed.
    let total = values
        .turn_successes
        .saturating_add(values.turn_failures);
    let (success_rate, failure_rate) = if total == 0 {
        (None, None)
    } else {
        let denom = total as f64;
        (
            Some(values.turn_successes as f64 / denom),
            Some(values.turn_failures as f64 / denom),
        )
    };
    AnalyticsCounterSnapshot {
        provider: provider.to_string(),
        channel_id: channel_id.to_string(),
        turn_attempts: values.turn_attempts,
        guard_fires: values.guard_fires,
        watcher_replacements: values.watcher_replacements,
        recovery_fires: values.recovery_fires,
        turn_successes: values.turn_successes,
        turn_failures: values.turn_failures,
        success_rate,
        failure_rate,
        snapshot_at,
        source: source.to_string(),
    }
}

fn matches_filters(
    filters: Option<&AnalyticsFilters>,
    provider: &str,
    channel_id: &str,
    event_type: Option<&str>,
) -> bool {
    let Some(filters) = filters else {
        return true;
    };
    if let Some(expected) = filters.provider.as_deref()
        && expected != provider
    {
        return false;
    }
    if let Some(expected) = filters.channel_id.as_deref()
        && expected != channel_id
    {
        return false;
    }
    if let Some(expected) = filters.event_type.as_deref()
        && event_type != Some(expected)
    {
        return false;
    }
    true
}

fn normalize_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn normalize_quality_event_type(value: &str) -> Option<String> {
    let normalized = value.trim().to_ascii_lowercase().replace('-', "_");
    AGENT_QUALITY_EVENT_TYPES
        .iter()
        .any(|candidate| *candidate == normalized)
        .then_some(normalized)
}

fn value_as_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
}

fn invariant_record_from_parts(
    id: i64,
    invariant: String,
    provider: Option<String>,
    channel_id: Option<String>,
    dispatch_id: Option<String>,
    session_key: Option<String>,
    turn_id: Option<String>,
    payload: Value,
    created_at: String,
) -> InvariantViolationRecord {
    let message = payload.get("message").and_then(value_as_string);
    let code_location = payload.get("code_location").and_then(value_as_string);
    let details = payload.get("details").cloned().unwrap_or_else(|| json!({}));
    InvariantViolationRecord {
        id,
        invariant,
        provider,
        channel_id,
        dispatch_id,
        session_key,
        turn_id,
        message,
        code_location,
        details,
        created_at,
    }
}

fn normalized_event_limit(limit: usize) -> usize {
    match limit {
        0 => DEFAULT_EVENT_LIMIT,
        value => value.min(MAX_EVENT_LIMIT),
    }
}

fn normalized_counter_limit(limit: usize) -> usize {
    match limit {
        0 => DEFAULT_COUNTER_LIMIT,
        value => value.min(MAX_COUNTER_LIMIT),
    }
}

fn normalized_invariant_limit(limit: usize) -> usize {
    match limit {
        0 => DEFAULT_INVARIANT_LIMIT,
        value => value.min(MAX_INVARIANT_LIMIT),
    }
}

fn normalized_quality_limit(limit: usize) -> usize {
    match limit {
        0 => DEFAULT_QUALITY_LIMIT,
        value => value.min(MAX_QUALITY_LIMIT),
    }
}

fn normalized_quality_daily_limit(limit: usize) -> usize {
    match limit {
        0 => DEFAULT_QUALITY_DAILY_LIMIT,
        value => value.min(MAX_QUALITY_DAILY_LIMIT),
    }
}

fn normalized_quality_ranking_limit(limit: usize) -> usize {
    match limit {
        0 => DEFAULT_QUALITY_RANKING_LIMIT,
        value => value.min(MAX_QUALITY_RANKING_LIMIT),
    }
}

fn normalized_quality_days(days: i64) -> i64 {
    match days {
        value if value <= 0 => DEFAULT_QUALITY_DAYS,
        value => value.min(MAX_QUALITY_DAYS),
    }
}

fn now_kst() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn saturating_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) async fn flush_for_tests() {
    let Some(sender) = worker_sender() else {
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
    let handles = storage_handles(&runtime());
    (false, handles.pg_pool.is_some())
}

#[cfg(test)]
mod cancellation_observability_tests {
    use super::*;

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
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
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
    async fn agent_quality_emit_and_query_round_trip() -> Result<()> {
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
    async fn agent_quality_query_without_pg_pool_is_unavailable() -> Result<()> {
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
    async fn agent_quality_unscoped_query_without_pg_pool_is_unavailable() -> Result<()> {
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
        let turn_rate: Option<f64> = row_a.try_get("turn_success_rate").unwrap();
        let review_rate: Option<f64> = row_a.try_get("review_pass_rate").unwrap();
        let sample_size: i64 = row_a.try_get("sample_size").unwrap();
        let rework: Option<f64> = row_a.try_get("avg_rework_count").unwrap();
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
        let sb: i64 = row_b.try_get("sample_size").unwrap();
        let unavail: bool = row_b.try_get("measurement_unavailable_7d").unwrap();
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
