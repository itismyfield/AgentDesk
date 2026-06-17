//! In-memory hook registry with TTL buffering and claim/replay semantics.
//!
//! Ported from Jinn's `gateway/hook-registry.ts` race pattern. The existing
//! `hook_server` broadcast + `prompt_ready_notify()` are **edge-triggered**: a
//! Stop / SessionStart hook that lands before a consumer subscribes is dropped
//! and a turn resolver that times out cannot recover the early evidence. That
//! produces prompt-readiness / finalization races that are hard to tell apart
//! from a real TUI hang.
//!
//! This module adds a **focused, additive** layer on top of the existing
//! broadcast (it does NOT replace it): every accepted hook is *also* buffered
//! here, keyed by `(provider, session_key)`, where `session_key` prefers the
//! provider session id and falls back to the tmux session name. A late
//! consumer can `claim` its key and replay the fresh buffered events exactly
//! once; expired or consumed events are dropped so a stale Stop from a previous
//! turn can never wake a fresh turn.
//!
//! ## Invariants (load-bearing — see PRD REQ-001..REQ-006)
//!
//! 1. **Composite key includes provider.** `(provider="claude", id="ABC")` and
//!    `(provider="codex", id="ABC")` are *different* keys and never leak into
//!    each other's claim. Channel id alone is never a valid key.
//! 2. **Single consumption.** Claiming a key drains its fresh buffer exactly
//!    once. A second claim of the same key (without new deliveries) replays
//!    nothing. Unregister drops listener state so a future turn cannot replay
//!    consumed hooks.
//! 3. **TTL.** An event older than the configured TTL is never replayed; it is
//!    swept lazily on the next `deliver` / `claim` / `snapshot` touch for that
//!    key, so there is no background task and no blocking I/O on the hook
//!    receiver path. The sweep is O(buffer length for that key).
//! 4. **Broadcast is untouched.** The `hook_server` broadcast and Codex
//!    `try_recv()` observer path keep firing for every event exactly as before.
//!    The registry is a parallel buffer; it does not gate, drop, or reorder the
//!    broadcast.
//! 5. **Unclaimed Stop is diagnostic-only in P0.** A Stop that arrives with no
//!    claimed listener increments a counter and (for a qualifying Stop) is
//!    retained as the newest unclaimed Stop. An empty Stop / one missing
//!    `last_assistant_message` is *counted* but flagged non-qualifying so P1 can
//!    decide whether to trigger a transcript sync; P0 never syncs or finalizes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::services::claude_tui::hook_server::{HookEvent, HookEventKind};

/// Compiled-in fallback TTL for buffered hooks when the runtime override is
/// unset. Matches the PRD default (`tui_hook_buffer_ttl_secs=30`).
pub const DEFAULT_HOOK_BUFFER_TTL: Duration = Duration::from_secs(30);

/// Compiled-in fallback diagnostic delay for unclaimed Stop handling when the
/// runtime override is unset. Matches the PRD default
/// (`tui_unclaimed_stop_delay_ms=2000`).
pub const DEFAULT_UNCLAIMED_STOP_DELAY: Duration = Duration::from_millis(2000);

/// Hard cap on the number of buffered events retained per `(provider, session)`
/// key. The hook receiver accepts bodies up to 8 MiB and the per-key buffer is
/// otherwise unbounded within the TTL window, so a busy or noisy session could
/// accumulate arbitrary memory before its events expire. When the cap is hit we
/// drop the OLDEST buffered event (front of the Vec) to make room for the new
/// one: replay/claim only cares about the freshest Stop / token hit (callers
/// scan newest-first), so evicting the stale front never loses the signal a
/// fresh wait is looking for. A dropped-for-capacity event is counted into
/// `expired_total` (it is, semantically, dropped before its TTL the same way an
/// expired event is). Keeping this bounded also bounds total registry memory at
/// `keys * MAX_BUFFERED_EVENTS_PER_KEY` buffered events.
pub const MAX_BUFFERED_EVENTS_PER_KEY: usize = 64;

/// Composite registry key. Provider is always part of the key so the same id
/// (a shared tmux session name) under two providers never cross-leaks. The
/// `session_key` is the provider session id when known, else the tmux session
/// name. Channel id alone is intentionally NOT representable here — callers map
/// channel -> tmux session before keying.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RegistryKey {
    pub provider: String,
    pub session_key: String,
}

impl RegistryKey {
    /// Build a key from a provider plus the preferred provider session id and
    /// an optional tmux session fallback. Returns `None` when neither a
    /// provider session id nor a tmux session name is available (channel alone
    /// is never a valid key — REQ-001).
    pub fn new(
        provider: &str,
        provider_session_id: Option<&str>,
        tmux_session_name: Option<&str>,
    ) -> Option<Self> {
        let provider = provider.trim().to_ascii_lowercase();
        if provider.is_empty() {
            return None;
        }
        let session_key = provider_session_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or_else(|| tmux_session_name.map(str::trim).filter(|v| !v.is_empty()))?
            .to_string();
        Some(Self {
            provider,
            session_key,
        })
    }
}

/// A buffered hook plus the monotonic instant it was buffered, used for TTL
/// sweeps. We keep the original `HookEvent` (which already carries a wall-clock
/// `received_at`) and add a monotonic `buffered_at` so TTL math is immune to
/// wall-clock jumps.
#[derive(Clone, Debug)]
struct BufferedEvent {
    event: HookEvent,
    buffered_at: Instant,
}

/// The newest unclaimed qualifying Stop retained for diagnostics (REQ-004).
#[derive(Clone, Debug)]
struct UnclaimedStop {
    event: HookEvent,
    armed_at: Instant,
    /// `false` for an empty Stop / one missing `last_assistant_message`: still
    /// counted, but flagged so P1 can refuse to sync on it.
    qualifying: bool,
}

/// Per-key state: the fresh (unconsumed) buffer plus diagnostic counters.
#[derive(Default)]
struct KeyState {
    buffer: Vec<BufferedEvent>,
    claimed: bool,
    /// Newest unclaimed Stop retained for diagnostics, if any.
    unclaimed_stop: Option<UnclaimedStop>,
    // ---- monotonically-increasing diagnostic counters (additive) ----
    buffered_total: u64,
    replayed_total: u64,
    expired_total: u64,
    unclaimed_stop_total: u64,
    empty_stop_total: u64,
}

/// Outcome of delivering a single event into the registry. Returned so the
/// caller (and tests) can assert what happened without reaching into private
/// state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliverOutcome {
    /// The event was buffered for a not-yet-claimed key.
    Buffered,
    /// A claimed listener exists; the event is delivered live (the buffer stays
    /// drained) and not retained for replay.
    DeliveredLive,
    /// An unclaimed Stop armed/refreshed the diagnostic timer.
    UnclaimedStopArmed,
}

/// Serializable, additive per-key diagnostic for a retained unclaimed Stop.
/// Surfaced on the per-session `/diag` payload so operators can tell whether a
/// terminal Stop landed with no consumer claiming the key (the early-Stop race
/// this registry exists to make observable). Diagnostic-only in P0 — nothing
/// finalizes or syncs on it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UnclaimedStopDiagnostic {
    /// Hook kind of the retained unclaimed Stop (`stop` / `subagent_stop`).
    pub kind: String,
    /// `true` when the Stop carried a non-empty `last_assistant_message`.
    /// `false` for an empty Stop that is counted but must not drive a sync.
    pub qualifying: bool,
    /// `true` once the configured `tui_unclaimed_stop_delay_ms` has elapsed with
    /// the Stop still unclaimed.
    pub delay_elapsed: bool,
}

/// Serializable, additive observability snapshot of the registry. Exposed for
/// `/diag`-style telemetry; every field is additive (new), so existing API
/// consumers are unaffected.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct HookRegistrySnapshot {
    /// Number of distinct live keys currently tracked.
    pub keys_tracked: usize,
    /// Sum of currently-buffered (unconsumed, unexpired) events across keys.
    pub buffered_event_count: usize,
    /// Number of keys that currently have a claimed listener.
    pub claimed_keys: usize,
    /// Cumulative events ever buffered.
    pub buffered_total: u64,
    /// Cumulative events ever replayed to a claiming listener.
    pub replayed_total: u64,
    /// Cumulative events dropped because they exceeded the TTL.
    pub expired_total: u64,
    /// Cumulative unclaimed Stop events observed (qualifying + non-qualifying).
    pub unclaimed_stop_total: u64,
    /// Cumulative empty / non-qualifying Stop events observed.
    pub empty_stop_total: u64,
    /// Keys that currently hold a retained unclaimed Stop for diagnostics.
    pub keys_with_unclaimed_stop: usize,
}

/// The registry itself. Cheap to construct; the process uses one global
/// instance via [`global`], but tests construct isolated instances.
pub struct HookRegistry {
    keys: Mutex<HashMap<RegistryKey, KeyState>>,
    ttl: Duration,
    unclaimed_stop_delay: Duration,
    /// Cumulative events replayed by `claim_once`, accumulated at the registry
    /// level because `claim_once` drops the key (and its per-key counters) on
    /// every call. Without this the advertised `replayed_total` would always be
    /// 0 in production, since `snapshot` only sums per-key counters for *live*
    /// keys and the transient one-shot consumer leaves none behind.
    replayed_once_total: AtomicU64,
    /// Cumulative diagnostic counters absorbed from keys that have been EVICTED
    /// from the live map (an empty unclaimed key dropped by `snapshot`, or a key
    /// removed by `claim_once`). Without these, evicting an empty key to bound
    /// map growth (REQ — "drop expired empty registry keys") would silently reset
    /// the advertised cumulative totals. `snapshot` folds these in alongside the
    /// surviving per-key counters so the reported totals stay monotonic.
    evicted_buffered_total: AtomicU64,
    evicted_expired_total: AtomicU64,
    evicted_unclaimed_stop_total: AtomicU64,
    evicted_empty_stop_total: AtomicU64,
}

impl HookRegistry {
    pub fn new(ttl: Duration, unclaimed_stop_delay: Duration) -> Self {
        Self {
            keys: Mutex::new(HashMap::new()),
            ttl,
            unclaimed_stop_delay,
            replayed_once_total: AtomicU64::new(0),
            evicted_buffered_total: AtomicU64::new(0),
            evicted_expired_total: AtomicU64::new(0),
            evicted_unclaimed_stop_total: AtomicU64::new(0),
            evicted_empty_stop_total: AtomicU64::new(0),
        }
    }

    /// Absorb a to-be-evicted key's cumulative diagnostic counters into the
    /// registry-level totals so dropping the key (to bound map growth) does not
    /// reset the advertised monotonic totals. `replayed_total` is handled
    /// separately by the callers that actually replay.
    fn absorb_evicted_counters(&self, state: &KeyState) {
        self.evicted_buffered_total
            .fetch_add(state.buffered_total, Ordering::Relaxed);
        self.evicted_expired_total
            .fetch_add(state.expired_total, Ordering::Relaxed);
        self.evicted_unclaimed_stop_total
            .fetch_add(state.unclaimed_stop_total, Ordering::Relaxed);
        self.evicted_empty_stop_total
            .fetch_add(state.empty_stop_total, Ordering::Relaxed);
    }

    /// Deliver an event into the registry. If a listener has claimed the key
    /// the event is delivered live (buffer stays drained); otherwise it is
    /// buffered for later replay, and a Stop additionally arms the diagnostic
    /// timer. Always sweeps expired entries for the touched key first so a
    /// stale Stop can never linger past its TTL. O(buffer length for the key).
    pub fn deliver(&self, key: RegistryKey, event: HookEvent) -> DeliverOutcome {
        let now = Instant::now();
        let mut keys = self.lock();
        let state = keys.entry(key).or_default();
        Self::sweep_state(state, self.ttl, now);

        let is_stop = matches!(
            event.kind,
            HookEventKind::Stop | HookEventKind::SubagentStop
        );

        if state.claimed {
            // A live listener already consumed this key's history; do not retain
            // for replay (single-consumption). The live path observes the event
            // through the broadcast; we just record diagnostics.
            state.buffered_total += 1;
            if is_stop {
                // A claimed key's Stop must NOT count as an unclaimed Stop — the
                // listener owns it (REQ-004: claimed event cannot trigger
                // fallback).
            }
            return DeliverOutcome::DeliveredLive;
        }

        if is_stop {
            let qualifying = stop_is_qualifying(&event);
            state.unclaimed_stop_total += 1;
            if !qualifying {
                state.empty_stop_total += 1;
            }
            state.unclaimed_stop = Some(UnclaimedStop {
                event: event.clone(),
                armed_at: now,
                qualifying,
            });
        }

        // Bound the per-key buffer: if at capacity, drop the OLDEST event to make
        // room. Claim/replay scans newest-first, so the freshest Stop / token hit
        // is preserved; only stale front entries are evicted. Count the eviction
        // as an expiry (it is dropped-before-TTL the same way).
        if state.buffer.len() >= MAX_BUFFERED_EVENTS_PER_KEY {
            let overflow = state.buffer.len() + 1 - MAX_BUFFERED_EVENTS_PER_KEY;
            state.buffer.drain(0..overflow);
            state.expired_total += overflow as u64;
        }
        state.buffer.push(BufferedEvent {
            event,
            buffered_at: now,
        });
        state.buffered_total += 1;
        if is_stop {
            DeliverOutcome::UnclaimedStopArmed
        } else {
            DeliverOutcome::Buffered
        }
    }

    /// Claim a key: mark it claimed, cancel any unclaimed Stop diagnostic timer,
    /// and return the fresh (unexpired) buffered events exactly once. A second
    /// claim with no new deliveries returns an empty vec (single-consumption).
    ///
    /// This long-lived claim variant (mark claimed + keep listener state) is the
    /// API P1 consumers (Codex rollout tail, transcript finalization — see
    /// TSK-P1-001) will migrate onto. P0 production consumers use `claim_once`
    /// (claim + immediate unregister). The lower-level claim/unregister pair is
    /// exercised by the unit tests that pin the REQ-002/REQ-003 semantics, so it
    /// is compiled under `#[cfg(test)]` until the P1 wiring lands.
    #[cfg(test)]
    pub fn claim(&self, key: RegistryKey) -> Vec<HookEvent> {
        let now = Instant::now();
        let mut keys = self.lock();
        let state = keys.entry(key).or_default();
        Self::sweep_state(state, self.ttl, now);

        state.claimed = true;
        // Claiming cancels the unclaimed Stop timer for this key (REQ-002).
        state.unclaimed_stop = None;

        let drained: Vec<HookEvent> = std::mem::take(&mut state.buffer)
            .into_iter()
            .map(|buffered| buffered.event)
            .collect();
        state.replayed_total += drained.len() as u64;
        drained
    }

    /// Unregister a claim: drop the listener state so future stale events are
    /// not replayed to a later turn (REQ-003). Counters are reset along with the
    /// key so a new turn starts clean; the key is removed entirely when empty.
    /// Pairs with the long-lived `claim`; test-only until the P1 wiring lands
    /// (P0 uses `claim_once`).
    #[cfg(test)]
    pub fn unregister(&self, key: &RegistryKey) {
        let mut keys = self.lock();
        keys.remove(key);
    }

    /// One-shot claim used by short-lived consumers: drain the fresh buffer
    /// exactly once AND immediately drop the listener state so the key returns to
    /// buffering mode for the next turn. This is the claim+unregister pair as a
    /// single atomic-ish operation; it gives single-consumption of the current
    /// turn's early events while keeping the registry useful for the next turn's
    /// early-Stop race (REQ-002 + REQ-003). Dropping the key resets its per-key
    /// counters (absorbed at the registry level so the totals stay monotonic).
    ///
    /// Test-only: production consumers (`/tui/wait`, the readiness wait) migrated
    /// to [`claim_matching_once`], which consumes only the matching event and
    /// re-buffers the rest so an unrelated waiter's buffered events are not
    /// discarded. The unconditional drain remains exercised by the unit tests
    /// that pin the REQ-002/REQ-003 single-consumption + replay-counting
    /// semantics.
    #[cfg(test)]
    pub fn claim_once(&self, key: RegistryKey) -> Vec<HookEvent> {
        let now = Instant::now();
        let mut keys = self.lock();
        let Some(mut state) = keys.remove(&key) else {
            return Vec::new();
        };
        Self::sweep_state(&mut state, self.ttl, now);
        let drained: Vec<HookEvent> = std::mem::take(&mut state.buffer)
            .into_iter()
            .map(|buffered| buffered.event)
            .collect();
        // The key (and its per-key counters) is dropped here, so record the
        // replay and absorb the cumulative diagnostics at the registry level —
        // otherwise the advertised totals would lose this key's history.
        if !drained.is_empty() {
            self.replayed_once_total
                .fetch_add(drained.len() as u64, Ordering::Relaxed);
        }
        self.absorb_evicted_counters(&state);
        drained
    }

    /// One-shot claim that consumes ONLY the events matching `wants`, leaving
    /// every other fresh (unexpired) buffered event in place for a later waiter
    /// to replay. Returns the newest matching event, or `None` when nothing
    /// matches.
    ///
    /// This is the selective variant of `claim_once` used by `/tui/wait`. The
    /// plain `claim_once` drains and drops the whole key, which discards buffered
    /// Stops / token payloads that a *different* waiter (e.g. one waiting on a
    /// different `until=token`) should still be able to replay. Here we:
    ///   1. sweep expired events,
    ///   2. remove and return the freshest event satisfying `wants`,
    ///   3. re-buffer every non-matching fresh event under the key (preserving
    ///      arrival order and the unclaimed-Stop diagnostic),
    /// so a non-matching buffered event is never lost to an unrelated wait.
    /// Unlike `claim_once` this does NOT mark the key claimed: the registry stays
    /// in buffering mode for the next turn's early-Stop race.
    pub fn claim_matching_once<F>(&self, key: RegistryKey, wants: F) -> Option<HookEvent>
    where
        F: Fn(&HookEvent) -> bool,
    {
        let now = Instant::now();
        let mut keys = self.lock();
        let state = keys.get_mut(&key)?;
        Self::sweep_state(state, self.ttl, now);

        // Find the newest matching event (scan from the back; arrival order is
        // preserved in the Vec). Remove exactly that one and keep the rest.
        let matched_idx = state
            .buffer
            .iter()
            .rposition(|buffered| wants(&buffered.event));
        let matched = match matched_idx {
            Some(idx) => {
                let removed = state.buffer.remove(idx);
                self.replayed_once_total.fetch_add(1, Ordering::Relaxed);
                // Consuming a Stop claims it: cancel the unclaimed-Stop diagnostic
                // timer so a later turn's diagnostic does not report this
                // now-claimed Stop as still pending.
                if matches!(
                    removed.event.kind,
                    HookEventKind::Stop | HookEventKind::SubagentStop
                ) {
                    state.unclaimed_stop = None;
                }
                Some(removed.event)
            }
            None => None,
        };

        // If the buffer is now empty and the key has nothing else to retain, drop
        // it to bound map growth (mirrors the snapshot eviction policy); its
        // cumulative counters are absorbed at the registry level.
        if state.buffer.is_empty() && !state.claimed && state.unclaimed_stop.is_none() {
            if let Some(removed) = keys.remove(&key) {
                self.absorb_evicted_counters(&removed);
            }
        }
        matched
    }

    /// Per-key diagnostic for the retained unclaimed Stop, or `None` when none is
    /// retained (no Stop seen, or it was claimed/expired). Reports the Stop kind,
    /// whether it qualified (non-empty `last_assistant_message`), and whether the
    /// configured `tui_unclaimed_stop_delay_ms` has elapsed with the Stop still
    /// unclaimed. Diagnostic only in P0 — surfaced on `/diag`; nothing finalizes
    /// or syncs on it. Sweeps the key first so an expired Stop reports `None`.
    pub fn unclaimed_stop_diagnostic(&self, key: &RegistryKey) -> Option<UnclaimedStopDiagnostic> {
        let now = Instant::now();
        let mut keys = self.lock();
        let state = keys.get_mut(key)?;
        Self::sweep_state(state, self.ttl, now);
        let stop = state.unclaimed_stop.as_ref()?;
        Some(UnclaimedStopDiagnostic {
            kind: stop.event.kind.as_str().to_string(),
            qualifying: stop.qualifying,
            delay_elapsed: now.duration_since(stop.armed_at) >= self.unclaimed_stop_delay,
        })
    }

    /// Number of currently-buffered (unconsumed, unexpired) events for `key`,
    /// after sweeping expired entries. Test-only contract pin (production reads
    /// the aggregate via `snapshot`).
    #[cfg(test)]
    pub fn buffered_len(&self, key: &RegistryKey) -> usize {
        let now = Instant::now();
        let mut keys = self.lock();
        match keys.get_mut(key) {
            Some(state) => {
                Self::sweep_state(state, self.ttl, now);
                state.buffer.len()
            }
            None => 0,
        }
    }

    /// Build an additive observability snapshot. Sweeps every key first so the
    /// reported buffered count excludes expired events, then EVICTS keys that
    /// swept clean and carry no live listener / retained Stop — without this an
    /// unclaimed session that fired one hook and was never claimed would leave an
    /// empty `KeyState` (and be counted by `keys_tracked`) forever, so a
    /// long-running dcserver would accumulate one map entry per unique session
    /// (REQ — "drop expired empty registry keys"). The evicted keys' cumulative
    /// counters are absorbed at the registry level first so the advertised totals
    /// stay monotonic. O(total buffered).
    pub fn snapshot(&self) -> HookRegistrySnapshot {
        let now = Instant::now();
        let mut keys = self.lock();
        let mut snap = HookRegistrySnapshot::default();
        keys.retain(|_, state| {
            Self::sweep_state(state, self.ttl, now);
            // A key is droppable once it has nothing left to replay: no buffered
            // events, no claimed listener, and no retained unclaimed Stop. Such a
            // key can only ever be re-created lazily on the next deliver/claim, so
            // dropping it here is safe and bounds map growth.
            let droppable =
                state.buffer.is_empty() && !state.claimed && state.unclaimed_stop.is_none();
            if droppable {
                self.absorb_evicted_counters(state);
                return false;
            }
            snap.buffered_event_count += state.buffer.len();
            if state.claimed {
                snap.claimed_keys += 1;
            }
            if state.unclaimed_stop.is_some() {
                snap.keys_with_unclaimed_stop += 1;
            }
            snap.buffered_total += state.buffered_total;
            snap.replayed_total += state.replayed_total;
            snap.expired_total += state.expired_total;
            snap.unclaimed_stop_total += state.unclaimed_stop_total;
            snap.empty_stop_total += state.empty_stop_total;
            true
        });
        snap.keys_tracked = keys.len();
        // Fold in replays delivered by `claim_once`, plus the cumulative counters
        // absorbed from every evicted key (claim_once removals + empty-key
        // sweeps), whose per-key counters are already gone from the live map.
        snap.replayed_total += self.replayed_once_total.load(Ordering::Relaxed);
        snap.buffered_total += self.evicted_buffered_total.load(Ordering::Relaxed);
        snap.expired_total += self.evicted_expired_total.load(Ordering::Relaxed);
        snap.unclaimed_stop_total += self.evicted_unclaimed_stop_total.load(Ordering::Relaxed);
        snap.empty_stop_total += self.evicted_empty_stop_total.load(Ordering::Relaxed);
        snap
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<RegistryKey, KeyState>> {
        self.keys.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Drop expired entries from a key's buffer and clear an expired unclaimed
    /// Stop. Counts expirations into the diagnostic counter. Pure on `state`.
    fn sweep_state(state: &mut KeyState, ttl: Duration, now: Instant) {
        let before = state.buffer.len();
        state
            .buffer
            .retain(|buffered| now.duration_since(buffered.buffered_at) < ttl);
        let expired = before - state.buffer.len();
        state.expired_total += expired as u64;

        if let Some(stop) = state.unclaimed_stop.as_ref() {
            if now.duration_since(stop.armed_at) >= ttl {
                state.unclaimed_stop = None;
            }
        }
    }
}

/// Whether a Stop event "qualifies" for diagnostic retention: it must carry a
/// non-empty `last_assistant_message`. An empty Stop body (the common case for
/// the Claude TUI) or a missing `last_assistant_message` is still *counted* but
/// flagged non-qualifying so P0 never syncs / finalizes on it (REQ-004).
fn stop_is_qualifying(event: &HookEvent) -> bool {
    let Some(message) = event
        .payload
        .get("last_assistant_message")
        .or_else(|| event.payload.get("lastAssistantMessage"))
    else {
        return false;
    };
    message
        .as_str()
        .map(|text| !text.trim().is_empty())
        .unwrap_or(false)
}

/// Process-global registry, lazily built from the live config TTL / delay
/// overrides at first access. Subsequent calls reuse the same instance — the
/// TTL is captured at construction; a hot-reload of the TTL takes effect for
/// the next process (a restart) OR can be observed by reading the live config
/// in the caller before invoking diagnostic helpers. For P0 the registry is
/// additive and disabled by a config flag, so capturing-at-first-access keeps
/// the lock-free hot path simple.
static GLOBAL: LazyLock<HookRegistry> = LazyLock::new(|| {
    let (ttl, delay) = current_registry_durations();
    HookRegistry::new(ttl, delay)
});

/// Read the TTL / unclaimed-Stop-delay overrides from the runtime config,
/// falling back to the compiled defaults. A configured `0` is treated as unset
/// to avoid an immediate-expiry footgun. Clamped so a bad value cannot overflow
/// `Instant + Duration`.
///
/// NOTE: although this reads from the live config, it is called exactly once —
/// when `GLOBAL` is first initialised — so the resulting durations are frozen on
/// the immutable `HookRegistry` for the life of the process. Editing
/// `tui_hook_buffer_ttl_secs` / `tui_unclaimed_stop_delay_ms` therefore requires
/// a restart; only `registry_enabled()` is re-read per hook.
pub fn current_registry_durations() -> (Duration, Duration) {
    let cfg = crate::config_live_reload::current();
    let ttl = cfg
        .as_ref()
        .and_then(|c| c.runtime.tui_hook_buffer_ttl_secs)
        .filter(|secs| *secs > 0)
        .map(|secs| Duration::from_secs(secs.min(86_400)))
        .unwrap_or(DEFAULT_HOOK_BUFFER_TTL);
    let delay = cfg
        .as_ref()
        .and_then(|c| c.runtime.tui_unclaimed_stop_delay_ms)
        .filter(|ms| *ms > 0)
        .map(|ms| Duration::from_millis(ms.min(600_000)))
        .unwrap_or(DEFAULT_UNCLAIMED_STOP_DELAY);
    (ttl, delay)
}

/// Whether the registry buffering layer is enabled. Reads the hot-reloadable
/// `tui_hook_registry_enabled` runtime flag; defaults to ON. This is the
/// documented rollback switch (REQ rollback contract): set it to `false` in
/// `agentdesk.yaml` and the hook receiver stops feeding the registry, leaving
/// the legacy broadcast + polling path exactly as before — no restart needed.
pub fn registry_enabled() -> bool {
    crate::config_live_reload::current()
        .and_then(|cfg| cfg.runtime.tui_hook_registry_enabled)
        .unwrap_or(true)
}

/// The process-global registry instance.
pub fn global() -> &'static HookRegistry {
    &GLOBAL
}

/// Convenience for `/diag`-style telemetry: a snapshot of the global registry.
pub fn global_snapshot() -> HookRegistrySnapshot {
    global().snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn registry() -> HookRegistry {
        HookRegistry::new(Duration::from_secs(30), Duration::from_millis(2000))
    }

    fn event(
        provider: &str,
        sid: &str,
        kind: HookEventKind,
        payload: serde_json::Value,
    ) -> HookEvent {
        HookEvent {
            provider: provider.to_string(),
            session_id: sid.to_string(),
            kind,
            received_at: Utc::now(),
            payload,
        }
    }

    fn key(provider: &str, sid: &str) -> RegistryKey {
        RegistryKey::new(provider, Some(sid), None).unwrap()
    }

    // -------- REQ-001: keying --------

    #[test]
    fn key_prefers_provider_session_then_tmux_then_none() {
        // Provider session id preferred.
        let k = RegistryKey::new("claude", Some("sid-1"), Some("tmux-1")).unwrap();
        assert_eq!(k.session_key, "sid-1");
        // Falls back to tmux when provider session id absent/blank.
        let k = RegistryKey::new("claude", None, Some("tmux-1")).unwrap();
        assert_eq!(k.session_key, "tmux-1");
        let k = RegistryKey::new("claude", Some("  "), Some("tmux-1")).unwrap();
        assert_eq!(k.session_key, "tmux-1");
        // Channel alone (no session id, no tmux) is never a valid key.
        assert!(RegistryKey::new("claude", None, None).is_none());
        assert!(RegistryKey::new("claude", Some(""), Some("")).is_none());
        // Empty provider is rejected.
        assert!(RegistryKey::new("", Some("sid"), None).is_none());
    }

    #[test]
    fn cross_provider_same_session_id_is_isolated() {
        // TEST: send Stop(claude, ABC) and Stop(codex, ABC); confirm separate
        // keys and no cross-leak into the other's claim (mustFix #5).
        let reg = registry();
        let claude_key = key("claude", "ABC");
        let codex_key = key("codex", "ABC");

        reg.deliver(
            claude_key.clone(),
            event("claude", "ABC", HookEventKind::Stop, json!({})),
        );
        reg.deliver(
            codex_key.clone(),
            event("codex", "ABC", HookEventKind::Stop, json!({})),
        );

        let claude_events = reg.claim(claude_key.clone());
        assert_eq!(claude_events.len(), 1);
        assert_eq!(claude_events[0].provider, "claude");

        let codex_events = reg.claim(codex_key);
        assert_eq!(codex_events.len(), 1);
        assert_eq!(codex_events[0].provider, "codex");
        // Re-claiming claude returns nothing — the codex deliver did not leak.
        assert!(reg.claim(claude_key).is_empty());
    }

    // -------- REQ-002: claim replays fresh events once --------

    #[test]
    fn deliver_before_claim_replays_once() {
        let reg = registry();
        let k = key("claude", "sid");
        reg.deliver(
            k.clone(),
            event("claude", "sid", HookEventKind::SessionStart, json!({})),
        );
        reg.deliver(
            k.clone(),
            event("claude", "sid", HookEventKind::Stop, json!({})),
        );

        let first = reg.claim(k.clone());
        assert_eq!(first.len(), 2, "claim replays both fresh buffered events");
        // Single-consumption: a second claim with no new deliveries replays nothing.
        let second = reg.claim(k);
        assert!(second.is_empty());
    }

    #[test]
    fn claim_cancels_unclaimed_stop_timer() {
        let reg = registry();
        let k = key("claude", "sid");
        reg.deliver(
            k.clone(),
            event("claude", "sid", HookEventKind::Stop, json!({})),
        );
        assert!(reg.unclaimed_stop_diagnostic(&k).is_some());
        let _ = reg.claim(k.clone());
        // After claim, the unclaimed Stop timer is cancelled (REQ-002).
        assert!(reg.unclaimed_stop_diagnostic(&k).is_none());
    }

    #[test]
    fn claim_before_deliver_then_live_delivery_is_not_retained() {
        let reg = registry();
        let k = key("claude", "sid");
        // Claim first (empty buffer).
        assert!(reg.claim(k.clone()).is_empty());
        // A subsequent deliver to a claimed key is live, not retained for replay.
        let outcome = reg.deliver(
            k.clone(),
            event("claude", "sid", HookEventKind::Stop, json!({})),
        );
        assert_eq!(outcome, DeliverOutcome::DeliveredLive);
        // Re-claim replays nothing — the live event was not buffered.
        assert!(reg.claim(k).is_empty());
    }

    // -------- REQ-003: unregister prevents future stale replay --------

    #[test]
    fn unregister_prevents_future_stale_replay() {
        let reg = registry();
        let k = key("claude", "sid");
        reg.deliver(
            k.clone(),
            event("claude", "sid", HookEventKind::Stop, json!({})),
        );
        reg.unregister(&k);
        // A fresh turn (same key) must not replay the dropped Stop.
        assert!(reg.claim(k).is_empty());
    }

    // -------- REQ-001 / stale: TTL sweep --------

    #[test]
    fn expired_events_are_swept_and_not_replayed() {
        // TTL of 0 makes every buffered event immediately stale.
        let reg = HookRegistry::new(Duration::from_millis(0), Duration::from_millis(2000));
        let k = key("claude", "sid");
        reg.deliver(
            k.clone(),
            event("claude", "sid", HookEventKind::Stop, json!({})),
        );
        // With a zero TTL the buffered event is already expired on the next touch.
        assert_eq!(reg.buffered_len(&k), 0);
        assert!(
            reg.claim(k.clone()).is_empty(),
            "stale Stop must not be replayed"
        );
        let snap = reg.snapshot();
        assert!(snap.expired_total >= 1);
    }

    // -------- REQ-004: unclaimed Stop diagnostics --------

    #[test]
    fn empty_stop_is_counted_but_non_qualifying() {
        let reg = registry();
        let k = key("claude", "sid");
        // Empty Stop body -> counted, but non-qualifying (no last_assistant_message).
        reg.deliver(
            k.clone(),
            event("claude", "sid", HookEventKind::Stop, json!({})),
        );
        let diag = reg
            .unclaimed_stop_diagnostic(&k)
            .expect("unclaimed Stop retained");
        assert!(!diag.qualifying);
        assert_eq!(diag.kind, "stop");
        let snap = reg.snapshot();
        assert_eq!(snap.unclaimed_stop_total, 1);
        assert_eq!(snap.empty_stop_total, 1);
    }

    #[test]
    fn qualifying_stop_carries_last_assistant_message() {
        let reg = registry();
        let k = key("claude", "sid");
        reg.deliver(
            k.clone(),
            event(
                "claude",
                "sid",
                HookEventKind::Stop,
                json!({ "last_assistant_message": "done with the task" }),
            ),
        );
        let diag = reg
            .unclaimed_stop_diagnostic(&k)
            .expect("unclaimed Stop retained");
        assert!(diag.qualifying);
        let snap = reg.snapshot();
        assert_eq!(snap.unclaimed_stop_total, 1);
        assert_eq!(
            snap.empty_stop_total, 0,
            "qualifying Stop is not an empty Stop"
        );
    }

    #[test]
    fn newest_unclaimed_stop_replaces_older() {
        let reg = registry();
        let k = key("claude", "sid");
        reg.deliver(
            k.clone(),
            event(
                "claude",
                "sid",
                HookEventKind::Stop,
                json!({ "last_assistant_message": "first" }),
            ),
        );
        reg.deliver(
            k.clone(),
            event("claude", "sid", HookEventKind::Stop, json!({})),
        );
        // The newest unclaimed Stop (empty) is retained — diagnostic-only.
        let diag = reg
            .unclaimed_stop_diagnostic(&k)
            .expect("unclaimed Stop retained");
        assert!(
            !diag.qualifying,
            "newest (empty) Stop replaced the qualifying one"
        );
    }

    #[test]
    fn claimed_key_stop_does_not_count_as_unclaimed() {
        // REQ-004: a claimed event cannot trigger the unclaimed fallback.
        let reg = registry();
        let k = key("claude", "sid");
        assert!(reg.claim(k.clone()).is_empty());
        let outcome = reg.deliver(
            k.clone(),
            event("claude", "sid", HookEventKind::Stop, json!({})),
        );
        assert_eq!(outcome, DeliverOutcome::DeliveredLive);
        let snap = reg.snapshot();
        assert_eq!(
            snap.unclaimed_stop_total, 0,
            "claimed Stop is not unclaimed"
        );
        assert!(reg.unclaimed_stop_diagnostic(&k).is_none());
    }

    #[test]
    fn unclaimed_stop_elapsed_after_delay() {
        let reg = HookRegistry::new(Duration::from_secs(30), Duration::from_millis(0));
        let k = key("claude", "sid");
        reg.deliver(
            k.clone(),
            event("claude", "sid", HookEventKind::Stop, json!({})),
        );
        // Zero delay => the diagnostic reports the timer as already elapsed.
        let diag = reg
            .unclaimed_stop_diagnostic(&k)
            .expect("unclaimed Stop retained");
        assert!(diag.delay_elapsed);
    }

    // -------- snapshot shape (additive observability) --------

    #[test]
    fn snapshot_aggregates_counts() {
        let reg = registry();
        let k1 = key("claude", "s1");
        let k2 = key("codex", "s2");
        reg.deliver(
            k1.clone(),
            event("claude", "s1", HookEventKind::Stop, json!({})),
        );
        reg.deliver(
            k2.clone(),
            event("codex", "s2", HookEventKind::SessionStart, json!({})),
        );
        let _ = reg.claim(k1);
        let snap = reg.snapshot();
        assert_eq!(snap.keys_tracked, 2);
        assert_eq!(snap.claimed_keys, 1);
        assert_eq!(snap.buffered_total, 2);
        assert_eq!(snap.replayed_total, 1);
        // Serializable to JSON with stable additive field names.
        let value = serde_json::to_value(&snap).unwrap();
        assert!(value.get("buffered_event_count").is_some());
        assert!(value.get("unclaimed_stop_total").is_some());
        assert!(value.get("replayed_total").is_some());
    }

    #[test]
    fn claim_once_replay_is_counted_in_replayed_total() {
        // Regression: the production one-shot consumer (`claim_once`) removes the
        // key — and with it the per-key `replayed_total` — so the advertised
        // aggregate must be tracked at the registry level. Before this fix the
        // diagnostic always reported 0 in production.
        let reg = registry();
        let k = key("claude", "sid");
        reg.deliver(
            k.clone(),
            event("claude", "sid", HookEventKind::SessionStart, json!({})),
        );
        reg.deliver(
            k.clone(),
            event("claude", "sid", HookEventKind::Stop, json!({})),
        );

        let replayed = reg.claim_once(k.clone());
        assert_eq!(replayed.len(), 2, "claim_once replays both buffered events");

        let snap = reg.snapshot();
        assert_eq!(
            snap.replayed_total, 2,
            "claim_once replays must be reflected in the advertised replayed_total"
        );
        // The key is gone after the one-shot claim, but the cumulative counter
        // survives; a second claim_once replays nothing and does not double-count.
        assert!(reg.claim_once(k).is_empty());
        assert_eq!(reg.snapshot().replayed_total, 2);
    }

    // -------- bounded buffers (per-key cap, drop-oldest) --------

    #[test]
    fn buffer_is_bounded_per_key_and_drops_oldest() {
        let reg = registry();
        let k = key("claude", "sid");
        // Push well past the cap with token-bearing payloads so each is distinct.
        let total = MAX_BUFFERED_EVENTS_PER_KEY + 10;
        for i in 0..total {
            reg.deliver(
                k.clone(),
                event(
                    "claude",
                    "sid",
                    HookEventKind::Notification,
                    json!({ "seq": i }),
                ),
            );
        }
        // The buffer never exceeds the cap.
        assert_eq!(reg.buffered_len(&k), MAX_BUFFERED_EVENTS_PER_KEY);
        let drained = reg.claim_once(k);
        assert_eq!(drained.len(), MAX_BUFFERED_EVENTS_PER_KEY);
        // The OLDEST entries were dropped: the freshest event survives, the
        // earliest does not.
        let seqs: Vec<i64> = drained
            .iter()
            .filter_map(|e| e.payload.get("seq").and_then(|v| v.as_i64()))
            .collect();
        assert_eq!(*seqs.last().unwrap(), (total - 1) as i64);
        assert_eq!(
            *seqs.first().unwrap(),
            (total - MAX_BUFFERED_EVENTS_PER_KEY) as i64
        );
        // Capacity drops are counted as expirations (dropped-before-TTL).
        let snap = reg.snapshot();
        assert_eq!(snap.expired_total, 10);
        assert_eq!(snap.buffered_total, total as u64);
    }

    // -------- empty expired key eviction (snapshot) --------

    #[test]
    fn snapshot_evicts_empty_unclaimed_keys_but_keeps_totals() {
        // TTL 0 => the single buffered event expires immediately, so after the
        // sweep the key is empty/unclaimed and must be dropped from the map.
        let reg = HookRegistry::new(Duration::from_millis(0), Duration::from_millis(2000));
        let k = key("claude", "sid");
        reg.deliver(
            k.clone(),
            event("claude", "sid", HookEventKind::Stop, json!({})),
        );
        let snap = reg.snapshot();
        assert_eq!(
            snap.keys_tracked, 0,
            "an empty, swept-clean, unclaimed key must be evicted"
        );
        // Cumulative counters survive the eviction (absorbed at registry level).
        assert_eq!(snap.buffered_total, 1);
        assert!(snap.expired_total >= 1);
        assert_eq!(snap.unclaimed_stop_total, 1);
        assert_eq!(snap.empty_stop_total, 1);
        // A second snapshot does not double-count the absorbed totals.
        let snap2 = reg.snapshot();
        assert_eq!(snap2.buffered_total, 1);
        assert_eq!(snap2.unclaimed_stop_total, 1);
        assert_eq!(snap2.keys_tracked, 0);
    }

    #[test]
    fn snapshot_keeps_keys_with_live_buffer_or_retained_stop() {
        let reg = registry();
        let k = key("claude", "sid");
        // A live (unexpired) Stop must NOT be evicted — it has a retained
        // unclaimed Stop and a buffered event.
        reg.deliver(
            k.clone(),
            event("claude", "sid", HookEventKind::Stop, json!({})),
        );
        let snap = reg.snapshot();
        assert_eq!(snap.keys_tracked, 1);
        assert_eq!(snap.keys_with_unclaimed_stop, 1);
    }

    // -------- claim_matching_once: re-buffer non-matching events --------

    #[test]
    fn claim_matching_once_consumes_only_match_and_rebuffers_rest() {
        let reg = registry();
        let k = key("claude", "sid");
        // A non-Stop token payload plus a Stop are buffered.
        reg.deliver(
            k.clone(),
            event(
                "claude",
                "sid",
                HookEventKind::Notification,
                json!({ "text": "TOKEN-XYZ here" }),
            ),
        );
        reg.deliver(
            k.clone(),
            event("claude", "sid", HookEventKind::Stop, json!({})),
        );
        // A Stop waiter consumes ONLY the Stop.
        let matched = reg.claim_matching_once(k.clone(), |e| {
            matches!(e.kind, HookEventKind::Stop | HookEventKind::SubagentStop)
        });
        assert!(matched.is_some());
        assert_eq!(matched.unwrap().kind, HookEventKind::Stop);
        // The non-matching token payload is still buffered for a later token wait.
        assert_eq!(reg.buffered_len(&k), 1);
        assert_eq!(
            reg.snapshot().replayed_total,
            1,
            "selective replay must be counted exactly once while the key stays live"
        );
        let token_match = reg.claim_matching_once(k.clone(), |e| {
            e.payload
                .get("text")
                .and_then(|v| v.as_str())
                .is_some_and(|t| t.contains("TOKEN-XYZ"))
        });
        assert!(
            token_match.is_some(),
            "the re-buffered token payload must remain replayable"
        );
        // Now the buffer is drained and the empty key is evicted.
        assert_eq!(reg.buffered_len(&k), 0);
    }

    #[test]
    fn claim_matching_once_no_match_keeps_buffer_intact() {
        let reg = registry();
        let k = key("claude", "sid");
        reg.deliver(
            k.clone(),
            event("claude", "sid", HookEventKind::Stop, json!({})),
        );
        // A token waiter that does not match must not discard the buffered Stop.
        let matched =
            reg.claim_matching_once(k.clone(), |e| matches!(e.kind, HookEventKind::SessionStart));
        assert!(matched.is_none());
        assert_eq!(reg.buffered_len(&k), 1, "unmatched claim must not discard");
        // A subsequent Stop waiter can still replay it.
        let stop = reg.claim_matching_once(k, |e| {
            matches!(e.kind, HookEventKind::Stop | HookEventKind::SubagentStop)
        });
        assert!(stop.is_some());
    }

    #[test]
    fn claim_matching_once_cancels_unclaimed_stop_diagnostic() {
        let reg = registry();
        let k = key("claude", "sid");
        reg.deliver(
            k.clone(),
            event("claude", "sid", HookEventKind::Stop, json!({})),
        );
        assert!(reg.unclaimed_stop_diagnostic(&k).is_some());
        let _ = reg.claim_matching_once(k.clone(), |e| {
            matches!(e.kind, HookEventKind::Stop | HookEventKind::SubagentStop)
        });
        // Consuming the Stop cancels its unclaimed diagnostic (and the now-empty
        // key is evicted, so the diagnostic reports None).
        assert!(reg.unclaimed_stop_diagnostic(&k).is_none());
    }

    // -------- config defaults / clamping / rollback flag --------

    /// A configured `0` (or absent) TTL / delay falls back to the compiled-in
    /// default; a huge value is clamped so `Instant + Duration` cannot overflow.
    #[test]
    fn registry_durations_apply_defaults_and_clamp() {
        // Pure default path (config not installed in unit tests => None).
        let (ttl, delay) = current_registry_durations();
        assert_eq!(ttl, DEFAULT_HOOK_BUFFER_TTL);
        assert_eq!(delay, DEFAULT_UNCLAIMED_STOP_DELAY);
    }

    /// The rollback flag defaults to ON when the config is not installed (the
    /// unit-test case). This pins the "feature defaults ON" contract — operators
    /// must explicitly set `tui_hook_registry_enabled: false` to disable it.
    #[test]
    fn registry_enabled_defaults_on() {
        assert!(registry_enabled());
    }
}
