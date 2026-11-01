use chrono::{Duration as ChronoDuration, FixedOffset, TimeZone, Utc};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const MAX_METRIC_RETENTION_HOURS: i64 = 7 * 24;
const KST_OFFSET_SECONDS: i32 = 9 * 60 * 60;
/// #2049 Finding 15: hard cap on the metric event ring + feedback ring so
/// they can't inflate to hundreds of MB on a high-traffic deployment.
const MAX_METRIC_EVENTS: usize = 100_000;
const MAX_FEEDBACK_EVENTS: usize = 10_000;
/// Run the retention scan at most once per this interval.
const HOT_PATH_PRUNE_INTERVAL: Duration = Duration::from_secs(60);

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

struct MementoThrottleState {
    metrics: VecDeque<MementoMetricEvent>,
    feedback_triggers: VecDeque<MementoFeedbackTriggerEvent>,
    /// Last prune wall-clock; keeps the retention scan off the hot path.
    last_prune_at: Instant,
}

impl Default for MementoThrottleState {
    fn default() -> Self {
        Self {
            metrics: VecDeque::new(),
            feedback_triggers: VecDeque::new(),
            last_prune_at: Instant::now(),
        }
    }
}

impl MementoThrottleState {
    /// Drop retention-aged metric events. Do *not* call from the hot path;
    /// use `prune_if_due`, which throttles to once per `HOT_PATH_PRUNE_INTERVAL`.
    fn prune(&mut self) {
        let now = Instant::now();
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
        self.last_prune_at = now;
    }

    /// #2049 Finding 15: skip the expensive full prune unless
    /// `HOT_PATH_PRUNE_INTERVAL` has elapsed since the last run.
    fn prune_if_due(&mut self) {
        if Instant::now().saturating_duration_since(self.last_prune_at) >= HOT_PATH_PRUNE_INTERVAL {
            self.prune();
        }
    }

    /// #2049 Finding 15: drop the oldest metric/feedback events when the ring
    /// exceeds its cap. Combined with retention pruning this keeps memory
    /// bounded even under heavy traffic.
    fn enforce_metric_ring_cap(&mut self) {
        while self.metrics.len() > MAX_METRIC_EVENTS {
            self.metrics.pop_front();
        }
        while self.feedback_triggers.len() > MAX_FEEDBACK_EVENTS {
            self.feedback_triggers.pop_front();
        }
    }
}

fn throttle_state() -> &'static Mutex<MementoThrottleState> {
    static STATE: OnceLock<Mutex<MementoThrottleState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(MementoThrottleState::default()))
}

/// #2049 Finding 15: hot-path entry point. Runs the cheap amortized prune
/// (no-op unless `HOT_PATH_PRUNE_INTERVAL` has elapsed) rather than the
/// per-call O(N) retain scan that was previously here.
fn with_state<R>(f: impl FnOnce(&mut MementoThrottleState) -> R) -> R {
    let mut guard = throttle_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.prune_if_due();
    f(&mut guard)
}

fn record_metric(tool_name: &'static str, action: MementoMetricAction) {
    with_state(|state| {
        state.metrics.push_back(MementoMetricEvent {
            timestamp: Utc::now(),
            tool_name,
            action,
        });
        state.enforce_metric_ring_cap();
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
        state.enforce_metric_ring_cap();
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
    // #5168: the `recall_context` block is gone. Every one of its counters was
    // fed only by `note_recall_context_size`, called from the two server-side
    // recall sites in `message_handler/{intake,headless}_turn.rs` that slice 2
    // (`abb72d261`) deleted. Under path B the model calls memento itself, so
    // AgentDesk cannot measure a context payload it never fetches — the block
    // could only have reported a structural zero. Removed rather than
    // documented as permanently-zero: no consumer was found (no dashboard,
    // script or doc reads it beyond the `/api/stats/memento` example, which is
    // updated in the same change).
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
        "automatic" | "sampled" => "automatic".to_string(),
        "manual" | "voluntary" => "voluntary".to_string(),
        // #4308: terminal count after the model ignored both the first Stop
        // reminder and its one permitted retry. This is an observation, not a
        // successful manual/automatic tool_feedback call, so keep it distinct.
        "unsubmitted_stop_flush" => "unsubmitted_stop_flush".to_string(),
        _ => "voluntary".to_string(),
    }
}

#[cfg(test)]
// Covers the tool_feedback-trigger metric counter; the module keeps its historical name.
mod forget_ratio_tests {
    use super::*;

    #[test]
    fn unsubmitted_stop_flush_is_exposed_in_the_seven_day_process_snapshot() {
        let before = memento_call_metrics_snapshot(24 * 7)["searchObservability"]
            ["feedback_counts_by_trigger_type"]["unsubmitted_stop_flush"]
            .as_u64()
            .unwrap_or(0);

        note_memento_tool_feedback_trigger("unsubmitted_stop_flush");

        let after = memento_call_metrics_snapshot(24 * 7)["searchObservability"]
            ["feedback_counts_by_trigger_type"]["unsubmitted_stop_flush"]
            .as_u64()
            .unwrap_or(0);
        assert!(after >= before.saturating_add(1));
    }
}
