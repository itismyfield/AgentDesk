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

/// Closed reason categories for permanently unrecoverable relay emissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayPermanentLossReason {
    DriftStateTtlExpired,
    DeadPane,
}

impl RelayPermanentLossReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DriftStateTtlExpired => "drift_state_ttl_expired",
            Self::DeadPane => "dead_pane",
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
    /// #5175: a terminal frame ended with NO delivery owner — the session-bound
    /// sink did not acknowledge delivery AND the soft terminal failed the
    /// watcher's turn-authority contract, so neither actor posted the body. Any
    /// non-zero value is a silently dropped answer plus a frozen delivery
    /// frontier (redrive then re-publishes the previous answer). The failing
    /// conjunct is emitted alongside as a `relay_terminal_authority_denied_*`
    /// root-cause counter.
    pub relay_terminal_authority_denied: AtomicU64,
    /// #4794: observed prompt-notification emissions that hit an authoritative
    /// tmux-owner registry miss and were still pending when bounded three-state
    /// probing definitively reported `DeadOrAbsent`. Poll misses are excluded;
    /// `ProbeError` preserves the pending count; successful/already-effective
    /// promotion drains it as delayed rather than lost. This is process-local and
    /// cumulative per `(channel_id, provider)`.
    pub relay_permanent_loss: AtomicU64,
    /// #4794: subset of permanent loss caused by unresolved drift state TTL expiry.
    pub relay_permanent_loss_drift_state_ttl_expired: AtomicU64,
    /// #4794: subset of permanent loss confirmed by a dead/absent pane probe.
    pub relay_permanent_loss_dead_pane: AtomicU64,
    /// #4794: `/resume` channel transition critical sections that exceeded the
    /// observation threshold and continued without cancellation.
    pub resume_critical_section_overrun: AtomicU64,
    /// #4913: canonical Discord identity writes rejected with a typed conflict.
    pub session_identity_conflicts: AtomicU64,
    pub session_identity_conflict_ambiguous_canonical: AtomicU64,
    pub session_identity_conflict_ambiguous_legacy: AtomicU64,
    pub session_identity_conflict_evidence_divergence: AtomicU64,
    pub session_identity_conflict_locator_namespace: AtomicU64,
    pub session_identity_conflict_ownership_mismatch: AtomicU64,
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
            relay_terminal_authority_denied: self
                .relay_terminal_authority_denied
                .load(Ordering::Relaxed),
            relay_permanent_loss: self.relay_permanent_loss.load(Ordering::Relaxed),
            relay_permanent_loss_drift_state_ttl_expired: self
                .relay_permanent_loss_drift_state_ttl_expired
                .load(Ordering::Relaxed),
            relay_permanent_loss_dead_pane: self
                .relay_permanent_loss_dead_pane
                .load(Ordering::Relaxed),
            resume_critical_section_overrun: self
                .resume_critical_section_overrun
                .load(Ordering::Relaxed),
            session_identity_conflicts: self.session_identity_conflicts.load(Ordering::Relaxed),
            session_identity_conflict_ambiguous_canonical: self
                .session_identity_conflict_ambiguous_canonical
                .load(Ordering::Relaxed),
            session_identity_conflict_ambiguous_legacy: self
                .session_identity_conflict_ambiguous_legacy
                .load(Ordering::Relaxed),
            session_identity_conflict_evidence_divergence: self
                .session_identity_conflict_evidence_divergence
                .load(Ordering::Relaxed),
            session_identity_conflict_locator_namespace: self
                .session_identity_conflict_locator_namespace
                .load(Ordering::Relaxed),
            session_identity_conflict_ownership_mismatch: self
                .session_identity_conflict_ownership_mismatch
                .load(Ordering::Relaxed),
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
    /// #5175: see [`AtomicCounters::relay_terminal_authority_denied`].
    pub relay_terminal_authority_denied: u64,
    /// #4794: confirmed lost observed-prompt emissions; see
    /// [`AtomicCounters::relay_permanent_loss`] for exact inclusion rules.
    pub relay_permanent_loss: u64,
    pub relay_permanent_loss_drift_state_ttl_expired: u64,
    pub relay_permanent_loss_dead_pane: u64,
    pub resume_critical_section_overrun: u64,
    /// #4913: see [`AtomicCounters::session_identity_conflicts`].
    pub session_identity_conflicts: u64,
    pub session_identity_conflict_ambiguous_canonical: u64,
    pub session_identity_conflict_ambiguous_legacy: u64,
    pub session_identity_conflict_evidence_divergence: u64,
    pub session_identity_conflict_locator_namespace: u64,
    pub session_identity_conflict_ownership_mismatch: u64,
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
    /// #5175: terminal frames that ended with no delivery owner at all. See
    /// [`AtomicCounters::relay_terminal_authority_denied`].
    pub relay_terminal_authority_denied: u64,
    /// #4794: confirmed lost observed-prompt emissions; see
    /// [`AtomicCounters::relay_permanent_loss`] for exact inclusion rules.
    pub relay_permanent_loss: u64,
    pub relay_permanent_loss_drift_state_ttl_expired: u64,
    pub relay_permanent_loss_dead_pane: u64,
    /// #4794: `/resume` transition critical sections observed beyond the threshold.
    pub resume_critical_section_overrun: u64,
    /// #4913: canonical identity writes rejected with a typed conflict.
    pub session_identity_conflicts: u64,
    pub session_identity_conflict_ambiguous_canonical: u64,
    pub session_identity_conflict_ambiguous_legacy: u64,
    pub session_identity_conflict_evidence_divergence: u64,
    pub session_identity_conflict_locator_namespace: u64,
    pub session_identity_conflict_ownership_mismatch: u64,
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

    /// #5175: a terminal frame ended with no delivery owner (sink unacknowledged
    /// AND soft-terminal watcher authority denied).
    pub fn record_relay_terminal_authority_denied(&self, channel_id: u64, provider: &str) {
        self.slot(channel_id, provider)
            .relay_terminal_authority_denied
            .fetch_add(1, Ordering::Relaxed);
    }

    /// #2838: a turn began relay with `RelayOwnerKind::Unknown`.
    pub fn record_relay_owner_unknown(&self, channel_id: u64, provider: &str) {
        self.slot(channel_id, provider)
            .relay_owner_unknown
            .fetch_add(1, Ordering::Relaxed);
    }

    /// #4794: add a confirmed count of permanently unrecoverable relay emissions.
    pub fn record_relay_permanent_loss(
        &self,
        channel_id: u64,
        provider: &str,
        reason: RelayPermanentLossReason,
        count: u64,
    ) {
        let slot = self.slot(channel_id, provider);
        slot.relay_permanent_loss
            .fetch_add(count, Ordering::Relaxed);
        match reason {
            RelayPermanentLossReason::DriftStateTtlExpired => {
                &slot.relay_permanent_loss_drift_state_ttl_expired
            }
            RelayPermanentLossReason::DeadPane => &slot.relay_permanent_loss_dead_pane,
        }
        .fetch_add(count, Ordering::Relaxed);
    }

    /// #4794: a `/resume` transition remained in its non-cancellable critical
    /// section beyond the observation threshold.
    pub fn record_resume_critical_section_overrun(&self, channel_id: u64, provider: &str) {
        self.slot(channel_id, provider)
            .resume_critical_section_overrun
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_session_identity_conflict(
        &self,
        channel_id: u64,
        provider: &str,
        kind: crate::db::dispatched_session_canonical_identity::SessionIdentityConflictKind,
    ) {
        let slot = self.slot(channel_id, provider);
        slot.session_identity_conflicts
            .fetch_add(1, Ordering::Relaxed);
        match kind {
            crate::db::dispatched_session_canonical_identity::SessionIdentityConflictKind::AmbiguousCanonical => {
                &slot.session_identity_conflict_ambiguous_canonical
            }
            crate::db::dispatched_session_canonical_identity::SessionIdentityConflictKind::AmbiguousLegacy => {
                &slot.session_identity_conflict_ambiguous_legacy
            }
            crate::db::dispatched_session_canonical_identity::SessionIdentityConflictKind::EvidenceDivergence => {
                &slot.session_identity_conflict_evidence_divergence
            }
            crate::db::dispatched_session_canonical_identity::SessionIdentityConflictKind::LocatorNamespace => {
                &slot.session_identity_conflict_locator_namespace
            }
            crate::db::dispatched_session_canonical_identity::SessionIdentityConflictKind::OwnershipMismatch => {
                &slot.session_identity_conflict_ownership_mismatch
            }
        }
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
                    relay_terminal_authority_denied: snap.relay_terminal_authority_denied,
                    relay_permanent_loss: snap.relay_permanent_loss,
                    relay_permanent_loss_drift_state_ttl_expired: snap
                        .relay_permanent_loss_drift_state_ttl_expired,
                    relay_permanent_loss_dead_pane: snap.relay_permanent_loss_dead_pane,
                    resume_critical_section_overrun: snap.resume_critical_section_overrun,
                    session_identity_conflicts: snap.session_identity_conflicts,
                    session_identity_conflict_ambiguous_canonical: snap
                        .session_identity_conflict_ambiguous_canonical,
                    session_identity_conflict_ambiguous_legacy: snap
                        .session_identity_conflict_ambiguous_legacy,
                    session_identity_conflict_evidence_divergence: snap
                        .session_identity_conflict_evidence_divergence,
                    session_identity_conflict_locator_namespace: snap
                        .session_identity_conflict_locator_namespace,
                    session_identity_conflict_ownership_mismatch: snap
                        .session_identity_conflict_ownership_mismatch,
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

pub fn record_session_identity_conflict(
    channel_id: u64,
    provider: &str,
    kind: crate::db::dispatched_session_canonical_identity::SessionIdentityConflictKind,
) {
    global().record_session_identity_conflict(channel_id, provider, kind);
}

/// #2838: convenience wrapper for `ObservabilityCounters::record_relay_terminal_ack_timeout`.
pub fn record_relay_terminal_ack_timeout(channel_id: u64, provider: &str) {
    global().record_relay_terminal_ack_timeout(channel_id, provider);
    super::emit::emit_relay_root_cause_counter(provider, channel_id, "relay_terminal_ack_timeout");
}

/// #5175: convenience wrapper for
/// `ObservabilityCounters::record_relay_terminal_authority_denied`.
///
/// `denial_counter` names the failing authority conjunct
/// (`relay_terminal_authority_denied_*`) and is emitted as its own root-cause
/// counter so an alert can distinguish "the row vanished" from "a forged turn
/// was correctly refused".
pub fn record_relay_terminal_authority_denied(
    channel_id: u64,
    provider: &str,
    denial_counter: &str,
) {
    global().record_relay_terminal_authority_denied(channel_id, provider);
    super::emit::emit_relay_root_cause_counter(
        provider,
        channel_id,
        "relay_terminal_authority_denied",
    );
    super::emit::emit_relay_root_cause_counter(provider, channel_id, denial_counter);
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

/// #4794: record confirmed permanent relay loss as an additive emission count.
pub fn record_relay_permanent_loss(
    channel_id: u64,
    provider: &str,
    reason: RelayPermanentLossReason,
    count: u64,
) {
    if count == 0 {
        return;
    }
    global().record_relay_permanent_loss(channel_id, provider, reason, count);
    tracing::error!(
        channel_id,
        provider,
        permanent_loss_count = count,
        permanent_loss_reason = reason.as_str(),
        "relay emissions became permanently unrecoverable"
    );
}

/// #4794: record a non-cancelling `/resume` critical-section overrun.
pub fn record_resume_critical_section_overrun(channel_id: u64, provider: &str) {
    global().record_resume_critical_section_overrun(channel_id, provider);
}

pub fn record_relay_circuit_activate_unknown() {
    super::emit::emit_relay_root_cause_counter("unknown", 0, "relay_circuit_activate_unknown");
}

pub fn snapshot() -> Vec<CounterSnapshotRow> {
    global().snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::dispatched_session_canonical_identity::SessionIdentityConflictKind;

    #[test]
    fn permanent_relay_loss_is_additive_and_exposed() {
        let counters = ObservabilityCounters::new();
        counters.record_relay_permanent_loss(
            4794,
            "Claude",
            RelayPermanentLossReason::DriftStateTtlExpired,
            9,
        );
        counters.record_relay_permanent_loss(4794, "claude", RelayPermanentLossReason::DeadPane, 2);

        let rows = counters.snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].channel_id, 4794);
        assert_eq!(rows[0].provider, "claude");
        assert_eq!(rows[0].relay_permanent_loss, 11);
        assert_eq!(rows[0].relay_permanent_loss_drift_state_ttl_expired, 9);
        assert_eq!(rows[0].relay_permanent_loss_dead_pane, 2);
    }

    #[test]
    fn identity_conflict_snapshot_preserves_closed_categories() {
        let counters = ObservabilityCounters::new();
        counters.record_session_identity_conflict(
            4913,
            "Claude",
            SessionIdentityConflictKind::LocatorNamespace,
        );
        counters.record_session_identity_conflict(
            4913,
            "claude",
            SessionIdentityConflictKind::OwnershipMismatch,
        );

        let rows = counters.snapshot();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.provider, "claude");
        assert_eq!(row.session_identity_conflicts, 2);
        assert_eq!(row.session_identity_conflict_locator_namespace, 1);
        assert_eq!(row.session_identity_conflict_ownership_mismatch, 1);
        assert_eq!(row.session_identity_conflict_ambiguous_canonical, 0);
        assert_eq!(row.session_identity_conflict_ambiguous_legacy, 0);
        assert_eq!(row.session_identity_conflict_evidence_divergence, 0);
    }
}
