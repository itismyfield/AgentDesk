//! Watcher first-relay latency metrics (#1134).
//!
//! Tracks the elapsed wall-clock time between the moment a tmux output watcher
//! is attached for a channel and the moment that watcher relays its first
//! payload to Discord. Captures three observables:
//!
//! - `watcher_first_relay_attach_total` (counter): number of attach events
//!   recorded.
//! - `watcher_first_relay_latency_seconds` (histogram): distribution of
//!   `attach -> first relay` durations in seconds.
//! - `watcher_first_relay_timeout_total` (counter): attach events for which no
//!   relay was observed within `ATTACH_TIMEOUT` (60 s).
//!
//! Implementation notes
//! --------------------
//! - Hot-path writes are lock-free where possible. Per-channel attach
//!   timestamps are stored in a `DashMap<u64, Instant>`; counter/histogram
//!   state lives behind a single `Mutex<HistogramState>` that is only touched
//!   on attach / first-relay / sweep boundaries (well below 1 op/s per
//!   channel under realistic traffic), so contention is negligible.
//! - The 60 s timeout sweep is lazy: it runs at most once per
//!   `SWEEP_MIN_INTERVAL` whenever `record_attach` or `snapshot` is invoked.
//!   No background task is spawned — keeps shutdown simple and avoids a
//!   long-lived runtime dependency.
//! - The histogram uses fixed-width buckets in seconds: 0.5, 1, 2, 5, 10,
//!   20, 30, 45, 60. Anything beyond 60 s is logically a timeout and is
//!   counted via the timeout counter rather than the histogram.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde::Serialize;

/// Upper bound (inclusive) for which an unmatched attach is treated as a
/// timeout. Keep in sync with the metric name suffix in the doc comment.
pub const ATTACH_TIMEOUT: Duration = Duration::from_secs(60);

/// Minimum wall-clock between two opportunistic timeout sweeps. Lazy sweeps
/// happen on `record_attach` and `snapshot`; this throttle prevents pathological
/// hot loops from sweeping on every call.
const SWEEP_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// Histogram bucket upper bounds in seconds. The final implicit bucket
/// (`+Inf`) is folded into the timeout counter rather than the histogram to
/// keep bucket semantics meaningful.
const BUCKET_UPPER_BOUNDS_SEC: [f64; 9] = [0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 45.0, 60.0];

/// Per-channel attach record. Stored in the DashMap keyed by `channel_id`.
#[derive(Debug, Clone, Copy)]
struct AttachRecord {
    attached_at: Instant,
}

/// Aggregated histogram + counter state. Cheap to lock — only touched on
/// attach / first-relay / sweep boundaries.
#[derive(Debug, Default)]
struct HistogramState {
    /// Cumulative bucket counts (NOT cumulative-from-zero like Prometheus
    /// expects on the wire; the public snapshot converts to cumulative form).
    bucket_counts: [u64; BUCKET_UPPER_BOUNDS_SEC.len()],
    sum_seconds: f64,
    count: u64,
    last_sweep_at: Option<Instant>,
}

impl HistogramState {
    fn observe(&mut self, latency: Duration) {
        let secs = latency.as_secs_f64();
        for (idx, upper) in BUCKET_UPPER_BOUNDS_SEC.iter().enumerate() {
            if secs <= *upper {
                self.bucket_counts[idx] = self.bucket_counts[idx].saturating_add(1);
                break;
            }
        }
        self.sum_seconds += secs;
        self.count = self.count.saturating_add(1);
    }
}

/// Public registry. One instance per process (see `global()`).
#[derive(Debug)]
pub struct WatcherLatencyMetrics {
    /// channel_id -> attach timestamp. Replaced (not stacked) on
    /// `record_attach` so a watcher that re-attaches after eviction starts a
    /// fresh latency measurement.
    pending: DashMap<u64, AttachRecord>,
    /// `watcher_first_relay_attach_total`.
    attach_total: AtomicU64,
    /// `watcher_first_relay_timeout_total`.
    timeout_total: AtomicU64,
    histogram: Mutex<HistogramState>,
}

impl Default for WatcherLatencyMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl WatcherLatencyMetrics {
    pub fn new() -> Self {
        Self {
            pending: DashMap::new(),
            attach_total: AtomicU64::new(0),
            timeout_total: AtomicU64::new(0),
            histogram: Mutex::new(HistogramState::default()),
        }
    }

    /// Record that a watcher has attached for `channel_id`. Replaces any
    /// pre-existing pending attach for the same channel (the previous one is
    /// counted as a timeout if it was already over the deadline; otherwise it
    /// is silently superseded — the issue is concerned with attach→relay
    /// latency, not duplicate-attach forensics).
    pub fn record_attach(&self, channel_id: u64) {
        self.record_attach_at(channel_id, Instant::now());
    }

    fn record_attach_at(&self, channel_id: u64, now: Instant) {
        self.attach_total.fetch_add(1, Ordering::Relaxed);
        self.pending
            .insert(channel_id, AttachRecord { attached_at: now });
        self.maybe_sweep(now);
    }

    /// Record that a watcher relayed its first payload for `channel_id`.
    /// Idempotent: subsequent calls for the same channel after the pending
    /// entry has been consumed are silent no-ops.
    pub fn record_first_relay(&self, channel_id: u64) {
        self.record_first_relay_at(channel_id, Instant::now());
    }

    fn record_first_relay_at(&self, channel_id: u64, now: Instant) {
        let Some((_, rec)) = self.pending.remove(&channel_id) else {
            return;
        };
        let elapsed = now.saturating_duration_since(rec.attached_at);
        if elapsed > ATTACH_TIMEOUT {
            // Late relay: still drain the pending entry but count it against
            // the timeout total so the histogram stays bounded.
            self.timeout_total.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if let Ok(mut h) = self.histogram.lock() {
            h.observe(elapsed);
        }
    }

    /// Sweep pending attach records older than `ATTACH_TIMEOUT`. Called
    /// opportunistically on `record_attach` / `snapshot`. Throttled to at most
    /// one full scan per `SWEEP_MIN_INTERVAL`.
    ///
    /// #2049 Finding 18: hold the histogram lock for the entire throttle
    /// check + sweep + last_sweep_at update so two concurrent callers can no
    /// longer both pass the throttle gate and double-count eviction stats.
    /// The pending map iteration is cheap (≈ active channels) so the
    /// critical section stays bounded.
    fn maybe_sweep(&self, now: Instant) {
        let Ok(mut h) = self.histogram.lock() else {
            return;
        };
        if let Some(last) = h.last_sweep_at {
            if now.saturating_duration_since(last) < SWEEP_MIN_INTERVAL {
                return;
            }
        }
        // Collect-then-remove to avoid holding DashMap shard locks while we
        // mutate. The pending map is expected to be small (≈ active channels).
        let stale: Vec<u64> = self
            .pending
            .iter()
            .filter_map(|entry| {
                let key = *entry.key();
                let age = now.saturating_duration_since(entry.value().attached_at);
                if age > ATTACH_TIMEOUT {
                    Some(key)
                } else {
                    None
                }
            })
            .collect();
        let mut evicted: u64 = 0;
        for key in stale {
            if self.pending.remove(&key).is_some() {
                evicted = evicted.saturating_add(1);
            }
        }
        if evicted > 0 {
            self.timeout_total.fetch_add(evicted, Ordering::Relaxed);
        }
        h.last_sweep_at = Some(now);
    }

    /// Force a sweep at `now`. Test-only escape hatch used by unit tests to
    /// deterministically advance the timeout window without sleeping.
    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    fn sweep_at(&self, now: Instant) {
        // Bypass the throttle for tests.
        if let Ok(mut h) = self.histogram.lock() {
            h.last_sweep_at = None;
        }
        self.maybe_sweep(now);
    }

    /// Serializable snapshot for `/api/analytics/observability` output.
    pub fn snapshot(&self) -> WatcherLatencySnapshot {
        let now = Instant::now();
        self.maybe_sweep(now);
        let attach_total = self.attach_total.load(Ordering::Relaxed);
        let timeout_total = self.timeout_total.load(Ordering::Relaxed);
        let (buckets, sum_seconds, count) = if let Ok(h) = self.histogram.lock() {
            // Convert per-bucket counts to cumulative form for downstream
            // Prometheus-style consumers.
            let mut cumulative = [0_u64; BUCKET_UPPER_BOUNDS_SEC.len()];
            let mut acc: u64 = 0;
            for (idx, c) in h.bucket_counts.iter().enumerate() {
                acc = acc.saturating_add(*c);
                cumulative[idx] = acc;
            }
            let mut buckets = Vec::with_capacity(BUCKET_UPPER_BOUNDS_SEC.len());
            for (idx, upper) in BUCKET_UPPER_BOUNDS_SEC.iter().enumerate() {
                buckets.push(WatcherLatencyBucket {
                    le_seconds: *upper,
                    cumulative_count: cumulative[idx],
                });
            }
            (buckets, h.sum_seconds, h.count)
        } else {
            (Vec::new(), 0.0, 0)
        };
        WatcherLatencySnapshot {
            attach_total,
            timeout_total,
            pending_attaches: self.pending.len() as u64,
            histogram: WatcherLatencyHistogram {
                buckets,
                sum_seconds,
                count,
            },
        }
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    pub fn reset(&self) {
        self.pending.clear();
        self.attach_total.store(0, Ordering::Relaxed);
        self.timeout_total.store(0, Ordering::Relaxed);
        if let Ok(mut h) = self.histogram.lock() {
            *h = HistogramState::default();
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WatcherLatencyBucket {
    /// Inclusive upper bound of the bucket in seconds (`le` in Prometheus
    /// parlance).
    pub le_seconds: f64,
    /// Cumulative count of observations whose latency was `<= le_seconds`.
    pub cumulative_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WatcherLatencyHistogram {
    pub buckets: Vec<WatcherLatencyBucket>,
    pub sum_seconds: f64,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WatcherLatencySnapshot {
    /// `watcher_first_relay_attach_total`.
    pub attach_total: u64,
    /// `watcher_first_relay_timeout_total`.
    pub timeout_total: u64,
    /// Currently outstanding attach records (attach observed, no relay yet).
    pub pending_attaches: u64,
    /// `watcher_first_relay_latency_seconds`.
    pub histogram: WatcherLatencyHistogram,
}

static GLOBAL: OnceLock<Arc<WatcherLatencyMetrics>> = OnceLock::new();

pub fn global() -> Arc<WatcherLatencyMetrics> {
    GLOBAL
        .get_or_init(|| Arc::new(WatcherLatencyMetrics::new()))
        .clone()
}

/// Convenience: record an attach in the process-wide registry.
pub fn record_attach(channel_id: u64) {
    global().record_attach(channel_id);
}

/// Convenience: record a first-relay event in the process-wide registry.
pub fn record_first_relay(channel_id: u64) {
    global().record_first_relay(channel_id);
}

/// Convenience: get the process-wide snapshot.
pub fn snapshot() -> WatcherLatencySnapshot {
    global().snapshot()
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn reset_for_tests() {
    global().reset();
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;

    #[test]
    fn attach_then_first_relay_records_histogram_observation() {
        let m = WatcherLatencyMetrics::new();
        let t0 = Instant::now();
        m.record_attach_at(42, t0);
        // 1.5 s elapsed → falls into the `<= 2.0` bucket.
        m.record_first_relay_at(42, t0 + Duration::from_millis(1_500));

        let snap = m.snapshot();
        assert_eq!(snap.attach_total, 1, "attach counter increments");
        assert_eq!(
            snap.timeout_total, 0,
            "no timeout when relay arrives in time"
        );
        assert_eq!(
            snap.histogram.count, 1,
            "histogram observed exactly one sample"
        );
        assert!((snap.histogram.sum_seconds - 1.5).abs() < 1e-6);
        assert_eq!(snap.pending_attaches, 0, "pending entry drained on relay");

        // Bucket assertions: cumulative counts at the 2 s bucket and beyond
        // should reflect the single 1.5 s observation.
        let two_sec = snap
            .histogram
            .buckets
            .iter()
            .find(|b| (b.le_seconds - 2.0).abs() < f64::EPSILON)
            .expect("2.0 s bucket exists");
        assert_eq!(two_sec.cumulative_count, 1);
        let one_sec = snap
            .histogram
            .buckets
            .iter()
            .find(|b| (b.le_seconds - 1.0).abs() < f64::EPSILON)
            .expect("1.0 s bucket exists");
        assert_eq!(
            one_sec.cumulative_count, 0,
            "1.5 s does not fall into <=1.0 bucket"
        );
    }

    #[test]
    fn first_relay_without_attach_is_silent_noop() {
        let m = WatcherLatencyMetrics::new();
        m.record_first_relay_at(7, Instant::now());
        let snap = m.snapshot();
        assert_eq!(snap.attach_total, 0);
        assert_eq!(snap.timeout_total, 0);
        assert_eq!(snap.histogram.count, 0);
    }

    #[test]
    fn second_relay_for_same_channel_is_ignored() {
        let m = WatcherLatencyMetrics::new();
        let t0 = Instant::now();
        m.record_attach_at(99, t0);
        m.record_first_relay_at(99, t0 + Duration::from_millis(200));
        m.record_first_relay_at(99, t0 + Duration::from_millis(900));
        let snap = m.snapshot();
        assert_eq!(snap.histogram.count, 1, "only the first relay counts");
    }

    #[test]
    fn pending_attach_older_than_timeout_is_swept_into_timeout_counter() {
        let m = WatcherLatencyMetrics::new();
        let t0 = Instant::now();
        m.record_attach_at(1, t0);
        m.record_attach_at(2, t0);
        // Advance virtual time past the 60 s window and force a sweep.
        let later = t0 + ATTACH_TIMEOUT + Duration::from_secs(1);
        m.sweep_at(later);
        let snap = m.snapshot();
        assert_eq!(snap.attach_total, 2);
        assert_eq!(snap.timeout_total, 2);
        assert_eq!(snap.pending_attaches, 0);
        assert_eq!(
            snap.histogram.count, 0,
            "timeouts do not feed the histogram"
        );
    }

    #[test]
    fn late_relay_after_timeout_is_counted_as_timeout_not_histogram() {
        let m = WatcherLatencyMetrics::new();
        let t0 = Instant::now();
        m.record_attach_at(5, t0);
        // Skip the sweep — exercise the late-relay branch directly.
        m.record_first_relay_at(5, t0 + ATTACH_TIMEOUT + Duration::from_secs(2));
        let snap = m.snapshot();
        assert_eq!(snap.timeout_total, 1);
        assert_eq!(snap.histogram.count, 0);
    }

    #[test]
    fn re_attach_replaces_previous_pending_entry() {
        let m = WatcherLatencyMetrics::new();
        let t0 = Instant::now();
        m.record_attach_at(11, t0);
        // Re-attach 5 s later. The latency should be measured from the second
        // attach, not the first.
        let t1 = t0 + Duration::from_secs(5);
        m.record_attach_at(11, t1);
        m.record_first_relay_at(11, t1 + Duration::from_millis(300));
        let snap = m.snapshot();
        assert_eq!(snap.attach_total, 2);
        assert_eq!(snap.histogram.count, 1);
        assert!((snap.histogram.sum_seconds - 0.3).abs() < 1e-6);
    }

    #[test]
    fn snapshot_round_trip_preserves_counters_and_histogram() {
        let m = WatcherLatencyMetrics::new();
        let t0 = Instant::now();
        m.record_attach_at(100, t0);
        m.record_first_relay_at(100, t0 + Duration::from_millis(750));
        m.record_attach_at(101, t0);
        m.record_first_relay_at(101, t0 + Duration::from_millis(1_200));

        let snap = m.snapshot();
        assert_eq!(snap.attach_total, 2);
        assert_eq!(snap.histogram.count, 2);
        assert!((snap.histogram.sum_seconds - (0.75 + 1.2)).abs() < 1e-6);

        // Snapshot is JSON-serializable end-to-end.
        let json = serde_json::to_value(&snap).expect("snapshot serializes");
        assert_eq!(json["attach_total"], 2);
        assert_eq!(json["timeout_total"], 0);
        assert_eq!(json["histogram"]["count"], 2);
        assert!(json["histogram"]["buckets"].is_array());
    }
}
