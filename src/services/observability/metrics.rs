//! Lightweight atomic observability counters for channel × provider.
//!
//! Introduced by issue #1070 (Epic #905 Phase 1) as a foundation layer on top
//! of the existing, heavier `observability` module. The existing module writes
//! structured events into SQLite/Postgres; this module maintains a pure
//! in-memory atomic counter table for O(1) hot-path updates and cheap
//! snapshotting by the `/api/analytics/observability` endpoint.
//!
//! Design goals:
//! - Hot-path writes must be lock-free (DashMap + AtomicU64).
//! - Counter keys are `(channel_id, provider)` tuples; `channel_id = 0` means
//!   "aggregate / unknown channel" (so callers that only know the provider can
//!   still record).
//! - Snapshot returns a `Vec<CounterSnapshotRow>` suitable for serde
//!   serialization without holding any lock.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;
use serde::Serialize;

/// Key for the counters table. `provider` is lowercased for stability.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CounterKey {
    pub channel_id: u64,
    pub provider: String,
}

impl CounterKey {
    pub fn new(channel_id: u64, provider: &str) -> Self {
        Self {
            channel_id,
            provider: provider.trim().to_ascii_lowercase(),
        }
    }
}

/// Atomic counters per `(channel_id, provider)`. All fields use `AtomicU64`.
#[derive(Debug, Default)]
pub struct AtomicCounters {
    pub attempts: AtomicU64,
    pub guard_fires: AtomicU64,
    pub watcher_replacements: AtomicU64,
    pub success: AtomicU64,
    pub fail: AtomicU64,
    /// #1085: turn entered with `session_id.is_some()` — provider session reused.
    pub session_reused: AtomicU64,
    /// #1085: turn entered with `session_id.is_none()` — provider session created fresh.
    pub session_new: AtomicU64,
    /// #1136: watcher hit the "inflight missing → DB dispatch fallback" path AND
    /// the DB fallback failed to resolve a `dispatch_id`. Each increment marks
    /// one occurrence where the legacy code would have silently dropped the
    /// watcher; the runtime now keeps the live watcher attached and observable.
    pub watcher_db_fallback_resolve_failed: AtomicU64,
    /// #2838 (relay-stability P0-1): the watcher's 10s session-bound terminal
    /// delivery ACK timed out AND the watcher proceeded to direct-send anyway.
    /// This is the primary duplicate-emit vector (root cause #1a): the
    /// StreamRelay sink may have actually posted but lagged the committed
    /// sequence metric, so the watcher re-sends the same answer. Rising counts
    /// here mean the dual-authority terminal-delivery lease is overdue.
    pub relay_terminal_ack_timeout: AtomicU64,
    /// #2838: finalization cleared inflight while `full_response` was non-empty
    /// and terminal delivery was NOT committed — i.e. a generated answer was
    /// destroyed with no retry path (root causes #1b / #4, the missing-answer
    /// vector). Any non-zero value is a leaked answer.
    pub relay_uncommitted_inflight_cleared: AtomicU64,
    /// #2838: a turn started relay with `RelayOwnerKind::Unknown` (ownership not
    /// cleanly assigned across the three relay-launch paths, root cause #3). A
    /// phantom/unknown owner can make the bridge skip its own delivery.
    pub relay_owner_unknown: AtomicU64,
}

impl AtomicCounters {
    fn snapshot(&self) -> AtomicCountersSnapshot {
        AtomicCountersSnapshot {
            attempts: self.attempts.load(Ordering::Relaxed),
            guard_fires: self.guard_fires.load(Ordering::Relaxed),
            watcher_replacements: self.watcher_replacements.load(Ordering::Relaxed),
            success: self.success.load(Ordering::Relaxed),
            fail: self.fail.load(Ordering::Relaxed),
            session_reused: self.session_reused.load(Ordering::Relaxed),
            session_new: self.session_new.load(Ordering::Relaxed),
            watcher_db_fallback_resolve_failed: self
                .watcher_db_fallback_resolve_failed
                .load(Ordering::Relaxed),
            relay_terminal_ack_timeout: self.relay_terminal_ack_timeout.load(Ordering::Relaxed),
            relay_uncommitted_inflight_cleared: self
                .relay_uncommitted_inflight_cleared
                .load(Ordering::Relaxed),
            relay_owner_unknown: self.relay_owner_unknown.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct AtomicCountersSnapshot {
    pub attempts: u64,
    pub guard_fires: u64,
    pub watcher_replacements: u64,
    pub success: u64,
    pub fail: u64,
    pub session_reused: u64,
    pub session_new: u64,
    /// #1136: see [`AtomicCounters::watcher_db_fallback_resolve_failed`].
    pub watcher_db_fallback_resolve_failed: u64,
    /// #2838: see [`AtomicCounters::relay_terminal_ack_timeout`].
    pub relay_terminal_ack_timeout: u64,
    /// #2838: see [`AtomicCounters::relay_uncommitted_inflight_cleared`].
    pub relay_uncommitted_inflight_cleared: u64,
    /// #2838: see [`AtomicCounters::relay_owner_unknown`].
    pub relay_owner_unknown: u64,
}

/// One row emitted by `ObservabilityCounters::snapshot()`.
#[derive(Debug, Clone, Serialize)]
pub struct CounterSnapshotRow {
    pub channel_id: u64,
    pub provider: String,
    pub attempts: u64,
    pub guard_fires: u64,
    pub watcher_replacements: u64,
    pub success: u64,
    pub fail: u64,
    pub success_rate: f64,
    /// #1085: cumulative count of turns that entered with an existing provider session_id.
    pub session_reused: u64,
    /// #1085: cumulative count of turns that started without an existing provider session_id.
    pub session_new: u64,
    /// #1085: ratio `session_reused / (session_reused + session_new)`; 0.0 when both zero.
    pub session_reuse_rate: f64,
    /// #1136: cumulative count of watcher DB-dispatch-fallback resolve failures
    /// for which the live watcher was kept attached instead of silently dropping.
    pub watcher_db_fallback_resolve_failed: u64,
    /// #2838: watcher 10s terminal-delivery ACK timed out then direct-sent (the
    /// duplicate-emit vector). See [`AtomicCounters::relay_terminal_ack_timeout`].
    pub relay_terminal_ack_timeout: u64,
    /// #2838: inflight cleared with a non-empty undelivered `full_response` (the
    /// missing-answer vector). See [`AtomicCounters::relay_uncommitted_inflight_cleared`].
    pub relay_uncommitted_inflight_cleared: u64,
    /// #2838: turns that began relay with an Unknown owner kind. See
    /// [`AtomicCounters::relay_owner_unknown`].
    pub relay_owner_unknown: u64,
}

/// In-process registry of `(channel_id, provider) -> AtomicCounters`.
#[derive(Debug, Default)]
pub struct ObservabilityCounters {
    table: DashMap<CounterKey, Arc<AtomicCounters>>,
}

impl ObservabilityCounters {
    pub fn new() -> Self {
        Self {
            table: DashMap::new(),
        }
    }

    fn slot(&self, channel_id: u64, provider: &str) -> Arc<AtomicCounters> {
        let key = CounterKey::new(channel_id, provider);
        if let Some(existing) = self.table.get(&key) {
            return existing.clone();
        }
        let fresh = Arc::new(AtomicCounters::default());
        self.table
            .entry(key)
            .or_insert_with(|| fresh.clone())
            .clone()
    }

    pub fn record_attempt(&self, channel_id: u64, provider: &str) {
        self.slot(channel_id, provider)
            .attempts
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_guard_fire(&self, channel_id: u64, provider: &str) {
        self.slot(channel_id, provider)
            .guard_fires
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_watcher_replacement(&self, channel_id: u64, provider: &str) {
        self.slot(channel_id, provider)
            .watcher_replacements
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_success(&self, channel_id: u64, provider: &str) {
        self.slot(channel_id, provider)
            .success
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_fail(&self, channel_id: u64, provider: &str) {
        self.slot(channel_id, provider)
            .fail
            .fetch_add(1, Ordering::Relaxed);
    }

    /// #1136: increment the watcher DB-fallback resolve-failure counter for
    /// `(channel_id, provider)`. Called whenever the watcher detects that the
    /// `inflight` state is missing AND the DB-side `dispatch_id` resolve also
    /// failed, in which case the runtime keeps the live watcher attached and
    /// marks the observation instead of silently dropping it.
    pub fn record_watcher_db_fallback_resolve_failed(&self, channel_id: u64, provider: &str) {
        self.slot(channel_id, provider)
            .watcher_db_fallback_resolve_failed
            .fetch_add(1, Ordering::Relaxed);
    }

    /// #2838: the watcher's 10s session-bound terminal-delivery ACK timed out
    /// and it proceeded to direct-send (the duplicate-emit vector).
    pub fn record_relay_terminal_ack_timeout(&self, channel_id: u64, provider: &str) {
        self.slot(channel_id, provider)
            .relay_terminal_ack_timeout
            .fetch_add(1, Ordering::Relaxed);
    }

    /// #2838: finalization cleared inflight while a non-empty `full_response`
    /// had not been committed to Discord (the missing-answer vector).
    pub fn record_relay_uncommitted_inflight_cleared(&self, channel_id: u64, provider: &str) {
        self.slot(channel_id, provider)
            .relay_uncommitted_inflight_cleared
            .fetch_add(1, Ordering::Relaxed);
    }

    /// #2838: a turn began relay with `RelayOwnerKind::Unknown`.
    pub fn record_relay_owner_unknown(&self, channel_id: u64, provider: &str) {
        self.slot(channel_id, provider)
            .relay_owner_unknown
            .fetch_add(1, Ordering::Relaxed);
    }

    /// #1085: record whether the turn entered with an existing provider session.
    /// `session_id_present == true` increments `session_reused`, else `session_new`.
    pub fn record_session_entry(&self, channel_id: u64, provider: &str, session_id_present: bool) {
        let slot = self.slot(channel_id, provider);
        if session_id_present {
            slot.session_reused.fetch_add(1, Ordering::Relaxed);
        } else {
            slot.session_new.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Serde-friendly snapshot. Does not clear counters.
    pub fn snapshot(&self) -> Vec<CounterSnapshotRow> {
        let mut rows: Vec<CounterSnapshotRow> = self
            .table
            .iter()
            .map(|entry| {
                let key = entry.key().clone();
                let snap = entry.value().snapshot();
                let denom = snap.success + snap.fail;
                let rate = if denom == 0 {
                    0.0
                } else {
                    snap.success as f64 / denom as f64
                };
                let session_denom = snap.session_reused + snap.session_new;
                let session_reuse_rate = if session_denom == 0 {
                    0.0
                } else {
                    snap.session_reused as f64 / session_denom as f64
                };
                CounterSnapshotRow {
                    channel_id: key.channel_id,
                    provider: key.provider,
                    attempts: snap.attempts,
                    guard_fires: snap.guard_fires,
                    watcher_replacements: snap.watcher_replacements,
                    success: snap.success,
                    fail: snap.fail,
                    success_rate: rate,
                    session_reused: snap.session_reused,
                    session_new: snap.session_new,
                    session_reuse_rate,
                    watcher_db_fallback_resolve_failed: snap.watcher_db_fallback_resolve_failed,
                    relay_terminal_ack_timeout: snap.relay_terminal_ack_timeout,
                    relay_uncommitted_inflight_cleared: snap.relay_uncommitted_inflight_cleared,
                    relay_owner_unknown: snap.relay_owner_unknown,
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            b.attempts
                .cmp(&a.attempts)
                .then_with(|| a.provider.cmp(&b.provider))
                .then_with(|| a.channel_id.cmp(&b.channel_id))
        });
        rows
    }
}

static GLOBAL_COUNTERS: OnceLock<Arc<ObservabilityCounters>> = OnceLock::new();

pub fn global() -> Arc<ObservabilityCounters> {
    GLOBAL_COUNTERS
        .get_or_init(|| Arc::new(ObservabilityCounters::new()))
        .clone()
}

/// Convenience wrappers so call-sites don't have to pull `global()` each time.
pub fn record_attempt(channel_id: u64, provider: &str) {
    global().record_attempt(channel_id, provider);
}

pub fn record_guard_fire(channel_id: u64, provider: &str) {
    global().record_guard_fire(channel_id, provider);
}

pub fn record_watcher_replacement(channel_id: u64, provider: &str) {
    global().record_watcher_replacement(channel_id, provider);
}

pub fn record_success(channel_id: u64, provider: &str) {
    global().record_success(channel_id, provider);
}

pub fn record_fail(channel_id: u64, provider: &str) {
    global().record_fail(channel_id, provider);
}

/// #1136: convenience wrapper for `ObservabilityCounters::record_watcher_db_fallback_resolve_failed`.
pub fn record_watcher_db_fallback_resolve_failed(channel_id: u64, provider: &str) {
    global().record_watcher_db_fallback_resolve_failed(channel_id, provider);
}

/// #1085: convenience wrapper for `ObservabilityCounters::record_session_entry`.
pub fn record_session_entry(channel_id: u64, provider: &str, session_id_present: bool) {
    global().record_session_entry(channel_id, provider, session_id_present);
}

/// #2838: convenience wrapper for `ObservabilityCounters::record_relay_terminal_ack_timeout`.
pub fn record_relay_terminal_ack_timeout(channel_id: u64, provider: &str) {
    global().record_relay_terminal_ack_timeout(channel_id, provider);
    super::emit::emit_relay_root_cause_counter(provider, channel_id, "relay_terminal_ack_timeout");
}

/// #2838: convenience wrapper for `ObservabilityCounters::record_relay_uncommitted_inflight_cleared`.
pub fn record_relay_uncommitted_inflight_cleared(channel_id: u64, provider: &str) {
    global().record_relay_uncommitted_inflight_cleared(channel_id, provider);
    super::emit::emit_relay_root_cause_counter(
        provider,
        channel_id,
        "relay_uncommitted_inflight_cleared",
    );
}

/// #2838: convenience wrapper for `ObservabilityCounters::record_relay_owner_unknown`.
pub fn record_relay_owner_unknown(channel_id: u64, provider: &str) {
    global().record_relay_owner_unknown(channel_id, provider);
    super::emit::emit_relay_root_cause_counter(provider, channel_id, "relay_owner_unknown");
}

pub fn record_relay_circuit_activate_unknown() {
    super::emit::emit_relay_root_cause_counter("unknown", 0, "relay_circuit_activate_unknown");
}

pub fn snapshot() -> Vec<CounterSnapshotRow> {
    global().snapshot()
}
