use chrono::{Duration as ChronoDuration, FixedOffset, TimeZone, Utc};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const RECALL_DEDUP_WINDOW: Duration = Duration::from_secs(60);
const REMEMBER_DEDUP_WINDOW: Duration = Duration::from_secs(5 * 60);
const MAX_METRIC_RETENTION_HOURS: i64 = 7 * 24;
const KST_OFFSET_SECONDS: i32 = 9 * 60 * 60;

#[derive(Clone, Debug)]
struct CachedRecallEntry {
    external_recall: Option<String>,
    expires_at: Instant,
}

#[derive(Clone, Debug)]
struct CachedRememberEntry {
    importance: Option<f64>,
    expires_at: Instant,
}

#[derive(Clone, Copy, Debug)]
enum MementoMetricAction {
    Request,
    RemoteCall,
    DedupHit,
}

#[derive(Clone, Debug)]
struct MementoMetricEvent {
    timestamp: chrono::DateTime<Utc>,
    tool_name: &'static str,
    action: MementoMetricAction,
}

#[derive(Clone, Debug)]
struct MementoFeedbackTriggerEvent {
    timestamp: chrono::DateTime<Utc>,
    trigger_type: String,
}

#[derive(Clone, Copy, Debug, Default)]
struct CallCounts {
    request_count: u64,
    remote_call_count: u64,
    dedup_hit_count: u64,
}

impl CallCounts {
    fn record(&mut self, action: MementoMetricAction) {
        match action {
            MementoMetricAction::Request => {
                self.request_count = self.request_count.saturating_add(1);
            }
            MementoMetricAction::RemoteCall => {
                self.remote_call_count = self.remote_call_count.saturating_add(1);
            }
            MementoMetricAction::DedupHit => {
                self.dedup_hit_count = self.dedup_hit_count.saturating_add(1);
            }
        }
    }

    fn as_json(self) -> Value {
        let dedup_rate = if self.request_count == 0 {
            0.0
        } else {
            self.dedup_hit_count as f64 / self.request_count as f64
        };

        json!({
            "request_count": self.request_count,
            "remote_call_count": self.remote_call_count,
            "dedup_hit_count": self.dedup_hit_count,
            "dedup_rate": dedup_rate,
        })
    }
}

#[derive(Clone, Debug, Default)]
struct HourBucket {
    total: CallCounts,
    tools: BTreeMap<String, CallCounts>,
}

#[derive(Default)]
struct MementoThrottleState {
    recall_cache: HashMap<String, CachedRecallEntry>,
    remember_cache: HashMap<String, CachedRememberEntry>,
    metrics: VecDeque<MementoMetricEvent>,
    feedback_triggers: VecDeque<MementoFeedbackTriggerEvent>,
}

impl MementoThrottleState {
    fn prune(&mut self) {
        let now = Instant::now();
        self.recall_cache.retain(|_, entry| entry.expires_at > now);
        self.remember_cache
            .retain(|_, entry| entry.expires_at > now);

        let cutoff = Utc::now() - ChronoDuration::hours(MAX_METRIC_RETENTION_HOURS);
        while self
            .metrics
            .front()
            .map(|event| event.timestamp < cutoff)
            .unwrap_or(false)
        {
            self.metrics.pop_front();
        }
        while self
            .feedback_triggers
            .front()
            .map(|event| event.timestamp < cutoff)
            .unwrap_or(false)
        {
            self.feedback_triggers.pop_front();
        }
    }
}

fn throttle_state() -> &'static Mutex<MementoThrottleState> {
    static STATE: OnceLock<Mutex<MementoThrottleState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(MementoThrottleState::default()))
}

fn with_state<R>(f: impl FnOnce(&mut MementoThrottleState) -> R) -> R {
    let mut guard = throttle_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.prune();
    f(&mut guard)
}

fn record_metric(tool_name: &'static str, action: MementoMetricAction) {
    with_state(|state| {
        state.metrics.push_back(MementoMetricEvent {
            timestamp: Utc::now(),
            tool_name,
            action,
        });
    });
}

pub(crate) fn note_memento_tool_request(tool_name: &'static str) {
    record_metric(tool_name, MementoMetricAction::Request);
}

pub(crate) fn note_memento_remote_call(tool_name: &'static str) {
    record_metric(tool_name, MementoMetricAction::RemoteCall);
}

pub(crate) fn note_memento_dedup_hit(tool_name: &'static str) {
    record_metric(tool_name, MementoMetricAction::DedupHit);
}

pub(crate) fn note_memento_tool_feedback_trigger(trigger_type: &str) {
    let trigger_type = normalize_feedback_trigger_type(trigger_type);
    with_state(|state| {
        state
            .feedback_triggers
            .push_back(MementoFeedbackTriggerEvent {
                timestamp: Utc::now(),
                trigger_type,
            });
    });
}

pub(crate) fn cached_recall_response(key: &str) -> Option<Option<String>> {
    with_state(|state| {
        state
            .recall_cache
            .get(key)
            .map(|entry| entry.external_recall.clone())
    })
}

pub(crate) fn store_recall_response(key: String, external_recall: Option<String>) {
    with_state(|state| {
        state.recall_cache.insert(
            key,
            CachedRecallEntry {
                external_recall,
                expires_at: Instant::now() + RECALL_DEDUP_WINDOW,
            },
        );
    });
}

pub(crate) fn should_dedup_remember(key: &str, importance: Option<f64>) -> bool {
    with_state(|state| {
        state
            .remember_cache
            .get(key)
            .map(|entry| match importance {
                Some(current) => entry
                    .importance
                    .map(|previous| current <= previous + f64::EPSILON)
                    .unwrap_or(false),
                None => true,
            })
            .unwrap_or(false)
    })
}

pub(crate) fn store_remember_fingerprint(key: String, importance: Option<f64>) {
    with_state(|state| {
        state.remember_cache.insert(
            key,
            CachedRememberEntry {
                importance,
                expires_at: Instant::now() + REMEMBER_DEDUP_WINDOW,
            },
        );
    });
}

pub(crate) fn memento_call_metrics_snapshot(window_hours: usize) -> Value {
    let window_hours = window_hours.clamp(1, 24 * 7);
    let now = Utc::now();
    let current_bucket_ts = now.timestamp() - now.timestamp().rem_euclid(3600);
    let first_bucket_ts =
        current_bucket_ts - (i64::try_from(window_hours).unwrap_or(1).saturating_sub(1)) * 3600;
    let kst = FixedOffset::east_opt(KST_OFFSET_SECONDS).expect("valid KST offset");

    let mut summary = CallCounts::default();
    let mut tools = BTreeMap::<String, CallCounts>::new();
    let mut hour_buckets = BTreeMap::<i64, HourBucket>::new();
    let mut feedback_trigger_counts = BTreeMap::<String, u64>::new();

    with_state(|state| {
        for event in state.metrics.iter() {
            if event.timestamp.timestamp() < first_bucket_ts {
                continue;
            }

            summary.record(event.action);
            tools
                .entry(event.tool_name.to_string())
                .or_default()
                .record(event.action);

            let bucket_ts =
                event.timestamp.timestamp() - event.timestamp.timestamp().rem_euclid(3600);
            let bucket = hour_buckets.entry(bucket_ts).or_default();
            bucket.total.record(event.action);
            bucket
                .tools
                .entry(event.tool_name.to_string())
                .or_default()
                .record(event.action);
        }

        for event in state.feedback_triggers.iter() {
            if event.timestamp.timestamp() < first_bucket_ts {
                continue;
            }
            let count = feedback_trigger_counts
                .entry(event.trigger_type.clone())
                .or_default();
            *count = count.saturating_add(1);
        }
    });

    let hours = (0..window_hours)
        .map(|offset| {
            let bucket_ts = first_bucket_ts + i64::try_from(offset).unwrap_or(0) * 3600;
            let bucket_start = Utc
                .timestamp_opt(bucket_ts, 0)
                .single()
                .expect("valid hourly timestamp")
                .with_timezone(&kst)
                .to_rfc3339();
            let bucket = hour_buckets.remove(&bucket_ts).unwrap_or_default();
            let tool_json = bucket
                .tools
                .into_iter()
                .map(|(tool_name, counts)| (tool_name, counts.as_json()))
                .collect::<serde_json::Map<String, Value>>();

            json!({
                "hour_start": bucket_start,
                "counts": bucket.total.as_json(),
                "tools": tool_json,
            })
        })
        .collect::<Vec<_>>();

    let tools_json = tools
        .into_iter()
        .map(|(tool_name, counts)| (tool_name, counts.as_json()))
        .collect::<serde_json::Map<String, Value>>();
    let feedback_trigger_json = feedback_trigger_counts
        .into_iter()
        .map(|(trigger_type, count)| (trigger_type, json!(count)))
        .collect::<serde_json::Map<String, Value>>();

    json!({
        "generated_at": now.with_timezone(&kst).to_rfc3339(),
        "timezone": "Asia/Seoul",
        "window_hours": window_hours,
        "summary": summary.as_json(),
        "tools": tools_json,
        "searchObservability": {
            "feedback_counts_by_trigger_type": feedback_trigger_json,
        },
        "hours": hours,
    })
}

fn normalize_feedback_trigger_type(trigger_type: &str) -> String {
    match trigger_type.trim().to_ascii_lowercase().as_str() {
        "automatic" => "automatic".to_string(),
        "manual" | "voluntary" => "voluntary".to_string(),
        _ => "voluntary".to_string(),
    }
}

// #1083: Track recall context size emitted per mode so #1083 can compare
// before/after average context bytes per channel without wiring a full A/B
// harness. Counters are global (process-wide); the call site logs the per-turn
// size and the average is computed by `recall_size_average_bytes`.
static FULL_CONTEXT_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
static FULL_CONTEXT_TURNS: AtomicU64 = AtomicU64::new(0);
static IDENTITY_CONTEXT_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
static IDENTITY_CONTEXT_TURNS: AtomicU64 = AtomicU64::new(0);
static SKIPPED_CONTEXT_TURNS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
pub(crate) enum RecallSizeBucket {
    Full,
    IdentityOnly,
    Skipped,
}

pub(crate) fn note_recall_context_size(bucket: RecallSizeBucket, bytes: usize) {
    match bucket {
        RecallSizeBucket::Full => {
            FULL_CONTEXT_BYTES_TOTAL.fetch_add(bytes as u64, Ordering::Relaxed);
            FULL_CONTEXT_TURNS.fetch_add(1, Ordering::Relaxed);
        }
        RecallSizeBucket::IdentityOnly => {
            IDENTITY_CONTEXT_BYTES_TOTAL.fetch_add(bytes as u64, Ordering::Relaxed);
            IDENTITY_CONTEXT_TURNS.fetch_add(1, Ordering::Relaxed);
        }
        RecallSizeBucket::Skipped => {
            SKIPPED_CONTEXT_TURNS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[allow(dead_code)] // exposed for ad-hoc inspection / future stats endpoint
pub(crate) fn recall_size_snapshot() -> Value {
    fn average(total: u64, count: u64) -> u64 {
        if count == 0 { 0 } else { total / count }
    }

    let full_total = FULL_CONTEXT_BYTES_TOTAL.load(Ordering::Relaxed);
    let full_turns = FULL_CONTEXT_TURNS.load(Ordering::Relaxed);
    let identity_total = IDENTITY_CONTEXT_BYTES_TOTAL.load(Ordering::Relaxed);
    let identity_turns = IDENTITY_CONTEXT_TURNS.load(Ordering::Relaxed);
    let skipped_turns = SKIPPED_CONTEXT_TURNS.load(Ordering::Relaxed);

    json!({
        "full": {
            "turns": full_turns,
            "bytes_total": full_total,
            "bytes_avg": average(full_total, full_turns),
        },
        "identity_only": {
            "turns": identity_turns,
            "bytes_total": identity_total,
            "bytes_avg": average(identity_total, identity_turns),
        },
        "skipped": {
            "turns": skipped_turns,
        },
    })
}

#[cfg(test)]
pub(crate) fn reset_memento_throttle_for_tests() {
    with_state(|state| {
        *state = MementoThrottleState::default();
    });
    FULL_CONTEXT_BYTES_TOTAL.store(0, Ordering::Relaxed);
    FULL_CONTEXT_TURNS.store(0, Ordering::Relaxed);
    IDENTITY_CONTEXT_BYTES_TOTAL.store(0, Ordering::Relaxed);
    IDENTITY_CONTEXT_TURNS.store(0, Ordering::Relaxed);
    SKIPPED_CONTEXT_TURNS.store(0, Ordering::Relaxed);
}
