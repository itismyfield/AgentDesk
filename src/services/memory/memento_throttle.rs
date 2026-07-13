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
/// #2660 — TTL for the static-slice tracker that records when the
/// `Ranked context from Memento` + `Core memory from Memento` blob was last
/// emitted to a given (workspace, agent, session, mode) lane. Within this
/// window, follow-up turns reuse the prior emission (via a one-line pointer)
/// instead of re-dumping multi-KB literals each turn. The 10-minute value is
/// a balance: long enough to dedupe a multi-turn user task, short enough that
/// fresh ranked-context revisions land within the same coffee break.
const STATIC_SLICE_WINDOW: Duration = Duration::from_secs(10 * 60);
const MAX_STATIC_SLICE_ENTRIES: usize = 4_096;
const MAX_METRIC_RETENTION_HOURS: i64 = 7 * 24;
const KST_OFFSET_SECONDS: i32 = 9 * 60 * 60;
/// #2655: window for the forget:recall flood monitor. Sliding-window counts of
/// `forget` and `recall` invocations are evaluated whenever a fresh
/// observation is recorded; outside this window observations are pruned.
const FORGET_RATIO_WINDOW: Duration = Duration::from_secs(30 * 60);
/// #2655: threshold ratio (`forget / recall`) above which a single warn is
/// emitted per dedupe key. A value of `5.0` mirrors the audit signal — the
/// chunk-05 §3 S7 observation that flagged `forget × 84 vs recall × 13`.
const FORGET_RATIO_ALARM_THRESHOLD: f64 = 5.0;
/// #2655: minimum number of forget observations inside the window before we
/// even consider firing the alarm. Avoids screaming on `forget × 2` against a
/// single recall.
const FORGET_RATIO_MIN_FORGET_COUNT: u64 = 10;
/// #2655: minimum cool-down before we can re-emit the alarm for the same
/// dedupe key. Keeps logs readable during sustained over-forget storms.
const FORGET_RATIO_ALARM_COOLDOWN: Duration = Duration::from_secs(15 * 60);
/// #2049 Finding 15: cap each cache so the hashmap can't grow without bound
/// between prune ticks. Combined with the amortized `prune_if_due` (which
/// replaces the per-call O(N) retain scan), the hot path stays O(1).
const MAX_RECALL_CACHE_ENTRIES: usize = 4_096;
const MAX_REMEMBER_CACHE_ENTRIES: usize = 4_096;
/// #2049 Finding 15: hard cap on the metric event ring + feedback ring so
/// they can't inflate to hundreds of MB on a high-traffic deployment.
const MAX_METRIC_EVENTS: usize = 100_000;
const MAX_FEEDBACK_EVENTS: usize = 10_000;
/// #2049 Finding 15: amortize prune cost. Run the full retain scan at most
/// once per `HOT_PATH_PRUNE_INTERVAL`. Lookups still check `expires_at`, so
/// we never serve an expired entry — this only affects when memory is
/// reclaimed.
const HOT_PATH_PRUNE_INTERVAL: Duration = Duration::from_secs(60);

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

/// #2660 — tracker entry for the static-slice cache. `digest` is a stable
/// 64-bit fingerprint of the emitted static-section text, used to detect
/// content drift so a *changed* ranked-context revision is still re-emitted.
#[derive(Clone, Debug)]
struct StaticSliceEntry {
    digest: u64,
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

/// #2655: observation kinds tracked by the forget-flood monitor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ForgetRatioObservationKind {
    Forget,
    Recall,
}

#[derive(Clone, Debug)]
struct ForgetRatioObservation {
    at: Instant,
    kind: ForgetRatioObservationKind,
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
    recall_cache: HashMap<String, CachedRecallEntry>,
    remember_cache: HashMap<String, CachedRememberEntry>,
    /// #2660 — per-(workspace,agent,session,mode) tracker that records when
    /// the static Memento dump (Ranked context + Core memory) was last
    /// emitted. Keyed independently from `recall_cache` so per-turn `user_text`
    /// variation no longer forces a multi-KB re-emit.
    static_slice_cache: HashMap<String, StaticSliceEntry>,
    metrics: VecDeque<MementoMetricEvent>,
    feedback_triggers: VecDeque<MementoFeedbackTriggerEvent>,
    /// #2049 Finding 15: last prune wall-clock. Used to throttle the O(N)
    /// hashmap retain scans so hot-path callers don't pay them on every turn.
    last_prune_at: Instant,
    /// #2655: forget/recall observations bucketed by dedupe scope key. Each
    /// scope (`provider:session`) gets its own rolling window so a noisy
    /// channel does not silence the alarm for another agent.
    forget_ratio_observations: HashMap<String, VecDeque<ForgetRatioObservation>>,
    /// #2655: last wall-clock when the alarm was emitted per scope, used for
    /// cool-down after the first warn.
    forget_ratio_last_alarm: HashMap<String, Instant>,
}

impl Default for MementoThrottleState {
    fn default() -> Self {
        Self {
            recall_cache: HashMap::new(),
            remember_cache: HashMap::new(),
            static_slice_cache: HashMap::new(),
            metrics: VecDeque::new(),
            feedback_triggers: VecDeque::new(),
            last_prune_at: Instant::now(),
            forget_ratio_observations: HashMap::new(),
            forget_ratio_last_alarm: HashMap::new(),
        }
    }
}

impl MementoThrottleState {
    /// Drop expired hashmap entries + retention-aged metric events.
    /// O(N) on the hashmaps; do *not* call from the hot path. Use
    /// `prune_if_due` instead, which throttles to once per
    /// `HOT_PATH_PRUNE_INTERVAL`.
    fn prune(&mut self) {
        let now = Instant::now();
        self.recall_cache.retain(|_, entry| entry.expires_at > now);
        self.remember_cache
            .retain(|_, entry| entry.expires_at > now);
        self.static_slice_cache
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
        // #2655: drop forget-ratio observations whose Instant is older than
        // the sliding window. Empty per-scope deques are removed so the map
        // does not grow unboundedly with one-off scopes.
        let ratio_cutoff = now.checked_sub(FORGET_RATIO_WINDOW);
        self.forget_ratio_observations.retain(|_, deque| {
            if let Some(cutoff) = ratio_cutoff {
                while deque.front().map(|obs| obs.at < cutoff).unwrap_or(false) {
                    deque.pop_front();
                }
            }
            !deque.is_empty()
        });
        // #2655: prune stale cool-down entries so the map can't grow with
        // one-off scopes either. A cool-down whose deadline expired more than
        // one window ago is irrelevant.
        if let Some(cutoff) = ratio_cutoff {
            self.forget_ratio_last_alarm
                .retain(|_, fired_at| *fired_at >= cutoff);
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

    /// #2049 Finding 15: enforce the recall cache size cap. Called after each
    /// insert so an unbounded sequence of distinct keys cannot blow past the
    /// hard limit even when the periodic prune hasn't fired.
    fn enforce_recall_cache_cap(&mut self) {
        if self.recall_cache.len() <= MAX_RECALL_CACHE_ENTRIES {
            return;
        }
        // Drop the oldest-expiring entries first; this approximates LRU well
        // enough for short TTL caches without paying for an LRU data structure.
        let mut by_expiry: Vec<(String, Instant)> = self
            .recall_cache
            .iter()
            .map(|(k, v)| (k.clone(), v.expires_at))
            .collect();
        by_expiry.sort_by_key(|(_, t)| *t);
        let drop = self.recall_cache.len() - MAX_RECALL_CACHE_ENTRIES;
        for (key, _) in by_expiry.into_iter().take(drop) {
            self.recall_cache.remove(&key);
        }
    }

    /// #2049 Finding 15: same cap-enforcement story for the remember cache.
    fn enforce_remember_cache_cap(&mut self) {
        if self.remember_cache.len() <= MAX_REMEMBER_CACHE_ENTRIES {
            return;
        }
        let mut by_expiry: Vec<(String, Instant)> = self
            .remember_cache
            .iter()
            .map(|(k, v)| (k.clone(), v.expires_at))
            .collect();
        by_expiry.sort_by_key(|(_, t)| *t);
        let drop = self.remember_cache.len() - MAX_REMEMBER_CACHE_ENTRIES;
        for (key, _) in by_expiry.into_iter().take(drop) {
            self.remember_cache.remove(&key);
        }
    }

    /// #2660 — cap enforcement for the static-slice tracker. Mirrors the
    /// recall/remember LRU-ish drop strategy: when over the cap, evict the
    /// oldest-expiring entries first.
    fn enforce_static_slice_cap(&mut self) {
        if self.static_slice_cache.len() <= MAX_STATIC_SLICE_ENTRIES {
            return;
        }
        let mut by_expiry: Vec<(String, Instant)> = self
            .static_slice_cache
            .iter()
            .map(|(k, v)| (k.clone(), v.expires_at))
            .collect();
        by_expiry.sort_by_key(|(_, t)| *t);
        let drop = self.static_slice_cache.len() - MAX_STATIC_SLICE_ENTRIES;
        for (key, _) in by_expiry.into_iter().take(drop) {
            self.static_slice_cache.remove(&key);
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

/// #2655: outcome reported by [`observe_memento_forget_recall`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ForgetRatioAlarmDecision {
    /// Below threshold or insufficient evidence — no alarm.
    NoAlarm,
    /// Threshold exceeded and cool-down expired — fresh alarm emitted.
    Alarm,
    /// Threshold still exceeded but cool-down has not expired — alarm
    /// suppressed to avoid log spam.
    AlarmSuppressedByCooldown,
}

/// #2655: snapshot of a single forget:recall evaluation. Returned to the
/// caller so the hook receiver (or any other observer) can decide how loudly
/// to surface the signal.
#[derive(Clone, Debug)]
pub(crate) struct ForgetRatioSnapshot {
    pub forget_count: u64,
    pub recall_count: u64,
    pub ratio: f64,
    pub decision: ForgetRatioAlarmDecision,
}

/// #2655: observe a single memento `forget` or `recall` invocation, scoped by
/// caller-provided dedupe key (typically `"<provider>:<session_id>"`). Returns
/// a snapshot describing whether the threshold is breached, with an explicit
/// decision the caller can switch on for logging.
///
/// The alarm threshold and minimum-evidence floor live in the module
/// constants. The cool-down is per scope so concurrent agent sessions do not
/// silence each other.
pub(crate) fn observe_memento_forget_recall(
    scope_key: &str,
    kind: ForgetRatioObservationKind,
) -> ForgetRatioSnapshot {
    let scope_key = if scope_key.trim().is_empty() {
        "__unscoped__".to_string()
    } else {
        scope_key.trim().to_string()
    };
    with_state(|state| {
        let now = Instant::now();
        let cutoff = now.checked_sub(FORGET_RATIO_WINDOW);
        let entry = state
            .forget_ratio_observations
            .entry(scope_key.clone())
            .or_default();
        if let Some(cutoff) = cutoff {
            while entry.front().map(|obs| obs.at < cutoff).unwrap_or(false) {
                entry.pop_front();
            }
        }
        entry.push_back(ForgetRatioObservation { at: now, kind });
        // Hard cap: if the deque ever grows past 4× alarm threshold of evidence
        // we trim the oldest, since the ratio of interest is over a window of
        // recent events and unbounded growth helps no one.
        let hard_cap = (FORGET_RATIO_MIN_FORGET_COUNT.saturating_mul(40)) as usize;
        while entry.len() > hard_cap {
            entry.pop_front();
        }

        let (forget_count, recall_count) = entry.iter().fold((0u64, 0u64), |(f, r), obs| match obs
            .kind
        {
            ForgetRatioObservationKind::Forget => (f.saturating_add(1), r),
            ForgetRatioObservationKind::Recall => (f, r.saturating_add(1)),
        });
        let ratio = if recall_count == 0 {
            // Treat zero recalls as forget_count itself when forget_count
            // crosses the minimum. This is the worst-case ratio.
            forget_count as f64
        } else {
            forget_count as f64 / recall_count as f64
        };
        let exceeds_threshold =
            ratio >= FORGET_RATIO_ALARM_THRESHOLD && forget_count >= FORGET_RATIO_MIN_FORGET_COUNT;
        let decision = if !exceeds_threshold {
            ForgetRatioAlarmDecision::NoAlarm
        } else {
            match state.forget_ratio_last_alarm.get(&scope_key).copied() {
                Some(fired_at)
                    if now.saturating_duration_since(fired_at) < FORGET_RATIO_ALARM_COOLDOWN =>
                {
                    ForgetRatioAlarmDecision::AlarmSuppressedByCooldown
                }
                _ => {
                    state.forget_ratio_last_alarm.insert(scope_key.clone(), now);
                    ForgetRatioAlarmDecision::Alarm
                }
            }
        };
        ForgetRatioSnapshot {
            forget_count,
            recall_count,
            ratio,
            decision,
        }
    })
}

/// #2655: convenience wrapper that observes a forget call and emits a
/// `tracing::warn!` when the decision is `Alarm`. Returns the snapshot so
/// callers can take additional action (e.g. publish to telemetry).
pub(crate) fn note_memento_forget_call(scope_key: &str) -> ForgetRatioSnapshot {
    let snapshot = observe_memento_forget_recall(scope_key, ForgetRatioObservationKind::Forget);
    if matches!(snapshot.decision, ForgetRatioAlarmDecision::Alarm) {
        tracing::warn!(
            scope = scope_key,
            forget_count = snapshot.forget_count,
            recall_count = snapshot.recall_count,
            ratio = snapshot.ratio,
            threshold = FORGET_RATIO_ALARM_THRESHOLD,
            "memento forget:recall flood detected (#2655); recall precision likely insufficient — investigate why agent is over-forgetting"
        );
    }
    snapshot
}

/// #2655: companion to [`note_memento_forget_call`]. Observes a recall and
/// returns the resulting snapshot. Recalls themselves do not directly trigger
/// the alarm; they reset the ratio downward.
pub(crate) fn note_memento_recall_call(scope_key: &str) -> ForgetRatioSnapshot {
    observe_memento_forget_recall(scope_key, ForgetRatioObservationKind::Recall)
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

pub(crate) fn cached_recall_response(key: &str) -> Option<Option<String>> {
    with_state(|state| {
        // #2049 Finding 15: explicit expiry check here so the lookup is strict
        // even when the amortized prune hasn't fired yet.
        let now = Instant::now();
        state
            .recall_cache
            .get(key)
            .filter(|entry| entry.expires_at > now)
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
        state.enforce_recall_cache_cap();
    });
}

pub(crate) fn should_dedup_remember(key: &str, importance: Option<f64>) -> bool {
    with_state(|state| {
        // #2049 Finding 15: strict expiry check; the amortized prune may not
        // have run yet.
        let now = Instant::now();
        state
            .remember_cache
            .get(key)
            .filter(|entry| entry.expires_at > now)
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
        state.enforce_remember_cache_cap();
    });
}

/// #2660 — record/lookup helper for the static-slice tracker. Returns:
/// - `Some(true)`  → the same digest was emitted within the TTL window; the
///   caller SHOULD elide the static dump and emit a one-line pointer instead.
/// - `Some(false)` → a different digest is on file (content drifted) and the
///   caller SHOULD emit the fresh dump. The tracker is updated.
/// - `None`        → no record on file; the caller SHOULD emit the dump and
///   the tracker is updated.
///
/// Either way the entry is refreshed on call so an active multi-turn task
/// keeps benefiting from the cache.
pub(crate) fn record_static_slice_emission(key: &str, digest: u64) -> Option<bool> {
    with_state(|state| {
        let now = Instant::now();
        let result = state
            .static_slice_cache
            .get(key)
            .filter(|entry| entry.expires_at > now)
            .map(|entry| entry.digest == digest);
        state.static_slice_cache.insert(
            key.to_string(),
            StaticSliceEntry {
                digest,
                expires_at: now + STATIC_SLICE_WINDOW,
            },
        );
        state.enforce_static_slice_cap();
        result
    })
}

/// #2660 — test-only helper to inspect the static-slice cache without
/// touching the private state. Returns the cached digest if present and
/// not expired.
#[cfg(test)]
pub(crate) fn peek_static_slice_digest(key: &str) -> Option<u64> {
    with_state(|state| {
        let now = Instant::now();
        state
            .static_slice_cache
            .get(key)
            .filter(|entry| entry.expires_at > now)
            .map(|entry| entry.digest)
    })
}

/// #2660 — test-only helper to clear the tracker (so unit tests don't
/// leak state across runs in the same process).
#[cfg(test)]
pub(crate) fn clear_static_slice_cache() {
    with_state(|state| state.static_slice_cache.clear());
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
    let full_turns = FULL_CONTEXT_TURNS.load(Ordering::Relaxed);
    let full_bytes = FULL_CONTEXT_BYTES_TOTAL.load(Ordering::Relaxed);
    let identity_turns = IDENTITY_CONTEXT_TURNS.load(Ordering::Relaxed);
    let identity_bytes = IDENTITY_CONTEXT_BYTES_TOTAL.load(Ordering::Relaxed);
    let identity_empty_turns = IDENTITY_EMPTY_CONTEXT_TURNS.load(Ordering::Relaxed);
    let skipped_turns = SKIPPED_CONTEXT_TURNS.load(Ordering::Relaxed);

    json!({
        "generated_at": now.with_timezone(&kst).to_rfc3339(),
        "timezone": "Asia/Seoul",
        "window_hours": window_hours,
        "summary": summary.as_json(),
        "tools": tools_json,
        "searchObservability": {
            "feedback_counts_by_trigger_type": feedback_trigger_json,
        },
        "recall_context": {
            "full_turns": full_turns,
            "full_bytes": full_bytes,
            "full_average_bytes": average_bytes(full_bytes, full_turns),
            "identity_only_turns": identity_turns,
            "identity_only_bytes": identity_bytes,
            "identity_only_average_bytes": average_bytes(identity_bytes, identity_turns),
            "identity_only_empty_turns": identity_empty_turns,
            "skipped_turns": skipped_turns,
        },
        "hours": hours,
    })
}

fn average_bytes(total: u64, turns: u64) -> u64 {
    if turns == 0 { 0 } else { total / turns }
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

// #1083: Track recall context size emitted per mode so #1083 can compare
// before/after average context bytes per channel without wiring a full A/B
// harness. Counters are global (process-wide); the call site logs the per-turn
// size and the average is computed by `recall_size_average_bytes`.
static FULL_CONTEXT_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
static FULL_CONTEXT_TURNS: AtomicU64 = AtomicU64::new(0);
static IDENTITY_CONTEXT_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
static IDENTITY_CONTEXT_TURNS: AtomicU64 = AtomicU64::new(0);
static IDENTITY_EMPTY_CONTEXT_TURNS: AtomicU64 = AtomicU64::new(0);
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
            if bytes == 0 {
                IDENTITY_EMPTY_CONTEXT_TURNS.fetch_add(1, Ordering::Relaxed);
            }
        }
        RecallSizeBucket::Skipped => {
            SKIPPED_CONTEXT_TURNS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod forget_ratio_tests {
    use super::*;

    #[test]
    fn no_alarm_below_min_evidence() {
        let snapshot = note_memento_forget_call("claude:test-low-evidence");
        // single forget against zero recalls => ratio inflated, but evidence
        // count below floor, so no alarm yet.
        assert!(snapshot.forget_count >= 1);
        assert_eq!(snapshot.recall_count, 0);
        assert_eq!(snapshot.decision, ForgetRatioAlarmDecision::NoAlarm);
    }

    #[test]
    fn alarm_fires_once_then_cools_down() {
        let scope = "claude:test-flood";
        let mut alarm_count = 0;
        let mut suppressed_count = 0;
        for _ in 0..(FORGET_RATIO_MIN_FORGET_COUNT + 5) {
            let snapshot = note_memento_forget_call(scope);
            match snapshot.decision {
                ForgetRatioAlarmDecision::Alarm => alarm_count += 1,
                ForgetRatioAlarmDecision::AlarmSuppressedByCooldown => suppressed_count += 1,
                ForgetRatioAlarmDecision::NoAlarm => {}
            }
        }
        assert_eq!(
            alarm_count, 1,
            "only one fresh alarm should fire within the cool-down window"
        );
        assert!(
            suppressed_count >= 1,
            "subsequent over-threshold observations must be suppressed: got {suppressed_count}"
        );
    }

    #[test]
    fn recalls_reduce_ratio_below_threshold() {
        let scope = "claude:test-recalls-mix";
        // 5 recalls + 8 forgets => ratio 1.6 (below 5.0).
        for _ in 0..5 {
            note_memento_recall_call(scope);
        }
        let snapshot = (0..8)
            .map(|_| note_memento_forget_call(scope))
            .last()
            .expect("at least one observation");
        assert!(snapshot.ratio < FORGET_RATIO_ALARM_THRESHOLD);
        assert_eq!(snapshot.decision, ForgetRatioAlarmDecision::NoAlarm);
    }

    #[test]
    fn scope_isolation_does_not_silence_other_agents() {
        // Scope A floods => should alarm.
        for _ in 0..(FORGET_RATIO_MIN_FORGET_COUNT + 2) {
            note_memento_forget_call("codex:agent-a");
        }
        // Scope B alone is below evidence floor.
        let snapshot_b = note_memento_forget_call("codex:agent-b");
        assert_eq!(
            snapshot_b.decision,
            ForgetRatioAlarmDecision::NoAlarm,
            "another scope's flood must not pre-trigger an unrelated scope"
        );
    }

    #[test]
    fn empty_scope_falls_back_to_default_bucket() {
        let snapshot = note_memento_forget_call("");
        // The empty key is normalised to a sentinel; observation still
        // increments forget_count so downstream callers see the signal.
        assert!(snapshot.forget_count >= 1);
    }

    #[test]
    fn identity_only_empty_recall_is_exposed_in_stats() {
        let before =
            memento_call_metrics_snapshot(1)["recall_context"]["identity_only_empty_turns"]
                .as_u64()
                .unwrap_or(0);

        note_recall_context_size(RecallSizeBucket::IdentityOnly, 0);

        let recall_context = memento_call_metrics_snapshot(1)["recall_context"].clone();
        assert!(
            recall_context["identity_only_empty_turns"]
                .as_u64()
                .unwrap_or(0)
                >= before.saturating_add(1)
        );
        assert!(
            recall_context["identity_only_turns"].as_u64().unwrap_or(0)
                >= recall_context["identity_only_empty_turns"]
                    .as_u64()
                    .unwrap_or(0)
        );
    }

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
