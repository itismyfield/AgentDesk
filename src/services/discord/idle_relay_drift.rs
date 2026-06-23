//! #3306/#3656: registry/dedupe-mirror drift self-heal for TUI relay owner lookup.
//!
//! ## Problem
//!
//! TUI relay paths resolve a tmux-session→channel owner from the authoritative
//! `tmux_watchers` registry. #3018 made the registry the SINGLE authority: a
//! registry miss while the `tui_prompt_dedupe` mirror still holds a mapping is
//! observable DRIFT, and the resolver drops (never routes from the mirror, which
//! is not a reverse authority). That decision is preserved.
//!
//! For ROUTINE tmux sessions (e.g. `…claude-routine-token-daily-report-…`) the
//! drift becomes PERMANENT: the watcher slot is freed when the routine turn ends
//! (`remove_tmux_session_if_current`), the registry entry disappears, but the
//! mirror (24h in-memory TTL) survives. The existing #3105 self-heal only
//! promotes a SETTINGS-derived channel (`resolve_rehydrated_claude_tmux_channel_id`),
//! and a routine tmux name matches NO settings channel binding — so its repair
//! gate never opens. The idle loop then re-emits the per-poll drift WARN every
//! ~500ms forever (52k+ WARN over two days) and the routine's relay output is
//! dropped (functional defect: routine results never reach Discord) until a
//! dcserver restart wipes the in-memory mirror and boot recovery re-claims the
//! watcher.
//!
//! ## Fix (this module)
//!
//! 1. **Burst suppression**: rate-limit the per-session drift WARN (first
//!    occurrence emits immediately for incident visibility; subsequent ones are
//!    suppressed for a 5-minute window and re-emitted with `suppressed_count` and
//!    `drift_age_secs`). The drop branch still does NOT advance the scan offset,
//!    so a successful repair lets the next ~500ms poll re-scan the preserved tail
//!    and relay it (re-relay, NOT loss).
//!
//! 2. **Drift self-heal** (Claude only — the settings resolver is Claude-specific
//!    and the durable DB row column is written by the Claude/routine hook flow):
//!    on drift, once per session (60s cooldown, single-flight) trigger a one-shot
//!    async repair from a DURABLE source, in order:
//!      (a) the existing settings binding (`resolve_rehydrated_claude_tmux_channel_id`)
//!          — same trust level as #3105;
//!      (b) `sessions.channel_id` (NEW), promoted ONLY behind three guards:
//!          - the tmux session has a LIVE pane,
//!          - the row's `instance_id` (if present) is THIS instance,
//!          - the dedupe mirror's channel AGREES with the DB channel.
//!    On a passing source the value is promoted via the existing
//!    `restore_owner_channel_for_tmux_session` (the authoritative #3105 path) —
//!    the single-authority registry model is unchanged.
//!
//! ## Why this is NOT a reverse-authority promotion of the mirror (#3018 intact)
//!
//! The promoted VALUE never comes from the mirror. It comes from settings or from
//! `sessions.channel_id` (a durable value the session's own dispatch/hook flow
//! recorded under a host+token-hash-namespaced key). The mirror is used only as
//! (i) the drift detector (existing) and (ii) a BLOCK-only gate: if it disagrees
//! with the DB channel, the repair is blocked (the drop is kept). A validator
//! that can only block can never be the routing authority, so #3018's
//! "never route from the mirror" invariant holds literally — the resolver's
//! drift branch still returns `None`. Mis-delivery (relaying to the wrong
//! channel) is strictly worse than a drop, so the decision core defaults to
//! `Blocked` whenever the sources are ambiguous.

use std::collections::HashMap;
#[cfg(unix)]
use std::sync::Arc;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

#[cfg(unix)]
use poise::serenity_prelude::ChannelId;

#[cfg(unix)]
use super::SharedData;
#[cfg(unix)]
use crate::services::provider::ProviderKind;

/// Re-emit the per-session drift WARN at most once per this window after the
/// first (immediate) occurrence. The first hit is never suppressed so a fresh
/// incident is visible without delay.
const DRIFT_WARN_COOLDOWN: Duration = Duration::from_secs(300);

/// Minimum spacing between repair attempts for the same session, so a permanent
/// no-source session does not re-spawn a repair task on every ~500ms poll.
#[cfg(unix)]
const DRIFT_REPAIR_COOLDOWN: Duration = Duration::from_secs(60);

/// Purge per-session drift state entries untouched for longer than this, so the
/// map cannot grow unbounded across the process lifetime.
const DRIFT_STATE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug)]
struct DriftState {
    first_seen_at: Instant,
    last_touched_at: Instant,
    last_warn_at: Option<Instant>,
    suppressed_count: u64,
    #[cfg_attr(not(unix), allow(dead_code))]
    last_repair_attempt_at: Option<Instant>,
    repair_inflight: bool,
}

impl DriftState {
    fn new(now: Instant) -> Self {
        Self {
            first_seen_at: now,
            last_touched_at: now,
            last_warn_at: None,
            suppressed_count: 0,
            last_repair_attempt_at: None,
            repair_inflight: false,
        }
    }
}

static DRIFT_STATE: LazyLock<Mutex<HashMap<String, DriftState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Outcome of the rate-limiter for a single drift observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WarnDecision {
    /// Whether the caller should emit a WARN now.
    pub(super) emit: bool,
    /// Number of WARNs suppressed since the last emitted one (0 on the first
    /// emit). Carried in the re-emitted WARN so the burst is quantified.
    pub(super) suppressed_count: u64,
    /// Seconds since the drift was first observed for this session.
    pub(super) drift_age_secs: u64,
}

/// Pure rate-limiter core (time injected) so the suppression policy is unit
/// testable without wall-clock dependence. Mutates `state` in place.
fn decide_drift_warn(state: &mut DriftState, now: Instant) -> WarnDecision {
    state.last_touched_at = now;
    let drift_age_secs = now.saturating_duration_since(state.first_seen_at).as_secs();
    let due = match state.last_warn_at {
        None => true,
        Some(last) => now.saturating_duration_since(last) >= DRIFT_WARN_COOLDOWN,
    };
    if due {
        let suppressed = state.suppressed_count;
        state.last_warn_at = Some(now);
        state.suppressed_count = 0;
        WarnDecision {
            emit: true,
            suppressed_count: suppressed,
            drift_age_secs,
        }
    } else {
        state.suppressed_count = state.suppressed_count.saturating_add(1);
        WarnDecision {
            emit: false,
            suppressed_count: state.suppressed_count,
            drift_age_secs,
        }
    }
}

fn purge_expired_locked(map: &mut HashMap<String, DriftState>, now: Instant) {
    map.retain(|_, state| {
        state.repair_inflight
            || now.saturating_duration_since(state.last_touched_at) < DRIFT_STATE_TTL
    });
}

/// Rate-limit decision for the resolver-internal drift WARN (the
/// `resolve_owner_channel_authoritatively` "drift alert"). The resolver is the
/// single WARN owner; the idle drift hook only triggers repair. Returns the
/// decision the caller folds into its log fields.
pub(super) fn should_emit_drift_warn(tmux_session_name: &str) -> WarnDecision {
    let now = Instant::now();
    let mut map = DRIFT_STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    purge_expired_locked(&mut map, now);
    let state = map
        .entry(tmux_session_name.to_string())
        .or_insert_with(|| DriftState::new(now));
    decide_drift_warn(state, now)
}

/// Source a repair value was promoted from (for the success log) or the reason a
/// promotion was blocked / had no source.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RepairSource {
    /// Settings-derived channel binding (#3105 trust level).
    Settings,
    /// `sessions.channel_id` durable column (#3306), passed all guards.
    SessionsTable,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BlockReason {
    /// DB row's `instance_id` belongs to another instance.
    ForeignInstance,
    /// The tmux pane is no longer live.
    DeadPane,
    /// The dedupe mirror channel disagrees with the DB channel (possible session
    /// name reuse / rebind — dropping is safer than mis-delivery).
    MirrorMismatch,
    /// A DB channel exists but no dedupe mirror witness is present to corroborate
    /// it, so it must not be promoted.
    NoMirrorWitness,
}

/// Decision of the repair core for a single session.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RepairDecision {
    /// Promote `channel` from `source` into the authoritative registry.
    Promote { source: RepairSource, channel: u64 },
    /// A candidate value existed but a guard refused promotion (keep the drop).
    Blocked(BlockReason),
    /// No durable source resolved a channel (keep the drop, #3018 semantics).
    NoSource,
}

/// Resolved facts the IO layer feeds into the pure repair decision core.
#[cfg(unix)]
#[derive(Debug, Clone, Default)]
pub(super) struct RepairInputs {
    /// Settings-derived channel (`resolve_rehydrated_claude_tmux_channel_id`).
    pub(super) settings_channel: Option<u64>,
    /// `sessions.channel_id` durable column value.
    pub(super) db_channel: Option<u64>,
    /// Owning `instance_id` from the DB row, if recorded.
    pub(super) db_instance: Option<String>,
    /// This process's instance id.
    pub(super) local_instance: String,
    /// Dedupe mirror's last-seen channel (drift witness / agreement check).
    pub(super) mirror_channel: Option<u64>,
    /// Whether the tmux session currently has a live pane.
    pub(super) pane_live: bool,
}

/// Pure, IO-free repair decision (#3306). Mis-delivery is strictly worse than a
/// drop, so ambiguity resolves to `Blocked`/`NoSource`, never a guess.
///
/// Precedence:
/// 1. Settings binding (same trust as #3105) wins when the pane is live.
/// 2. Otherwise `sessions.channel_id` may be promoted ONLY behind all three
///    guards: live pane, matching instance, AND mirror-agrees.
#[cfg(unix)]
pub(super) fn evaluate_drift_repair(inputs: &RepairInputs) -> RepairDecision {
    if !inputs.pane_live {
        // A dead pane can never resolve (mirrors the #3105 dead-pane branch). If
        // there is no candidate at all, report NoSource so callers don't log a
        // spurious block.
        if inputs.settings_channel.is_none() && inputs.db_channel.is_none() {
            return RepairDecision::NoSource;
        }
        return RepairDecision::Blocked(BlockReason::DeadPane);
    }

    if let Some(channel) = inputs.settings_channel {
        return RepairDecision::Promote {
            source: RepairSource::Settings,
            channel,
        };
    }

    let Some(db_channel) = inputs.db_channel else {
        return RepairDecision::NoSource;
    };

    // Guard (b): the DB row must belong to THIS instance (cross-host is already
    // excluded by the hostname-embedded session_key, this also blocks a
    // dev/release co-tenant row that slipped the token-hash namespace).
    if let Some(db_instance) = inputs
        .db_instance
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if db_instance != inputs.local_instance.trim() {
            return RepairDecision::Blocked(BlockReason::ForeignInstance);
        }
    }

    // Guard (c): require a mirror witness AND agreement. A missing mirror means
    // no corroboration (the drift condition itself would not hold without it);
    // a disagreeing mirror signals a name-reuse / rebind window where the new
    // dispatch already recorded a different channel — dropping is safer.
    match inputs.mirror_channel {
        None => RepairDecision::Blocked(BlockReason::NoMirrorWitness),
        Some(mirror_channel) if mirror_channel != db_channel => {
            RepairDecision::Blocked(BlockReason::MirrorMismatch)
        }
        Some(_) => RepairDecision::Promote {
            source: RepairSource::SessionsTable,
            channel: db_channel,
        },
    }
}

/// RAII guard that clears the `repair_inflight` flag on drop, so a panicking
/// repair task can never leak the single-flight lock for a session.
#[cfg(unix)]
struct RepairInflightGuard {
    tmux_session_name: String,
}

#[cfg(unix)]
impl Drop for RepairInflightGuard {
    fn drop(&mut self) {
        let mut map = DRIFT_STATE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(state) = map.get_mut(&self.tmux_session_name) {
            state.repair_inflight = false;
        }
    }
}

/// Try to claim the single-flight repair slot for this session. Returns a guard
/// (held until the repair task ends) when the caller may proceed, or `None` when
/// a repair is already inflight or the 60s cooldown has not elapsed.
#[cfg(unix)]
fn try_begin_repair(tmux_session_name: &str, now: Instant) -> Option<RepairInflightGuard> {
    let mut map = DRIFT_STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    purge_expired_locked(&mut map, now);
    let state = map
        .entry(tmux_session_name.to_string())
        .or_insert_with(|| DriftState::new(now));
    if state.repair_inflight {
        return None;
    }
    if let Some(last) = state.last_repair_attempt_at {
        if now.saturating_duration_since(last) < DRIFT_REPAIR_COOLDOWN {
            return None;
        }
    }
    state.last_repair_attempt_at = Some(now);
    state.repair_inflight = true;
    Some(RepairInflightGuard {
        tmux_session_name: tmux_session_name.to_string(),
    })
}

/// Cross-OS no-op: tmux-based relay drift self-heal is unix-only, but the
/// owner-resolution chokepoint that invokes this lives in non-`cfg(unix)` code,
/// so a stub is required for the workspace to compile on non-unix targets (CI
/// `Fast check cross OS`). The relay machinery never runs on those targets.
#[cfg(not(unix))]
pub(super) fn on_idle_relay_drift(
    _shared: &std::sync::Arc<super::SharedData>,
    _provider: crate::services::provider::ProviderKind,
    _tmux_session_name: &str,
) {
}

/// Entry point invoked from the owner-resolution chokepoint's drift (drop)
/// branch. The resolver owns the single rate-limited drift WARN; this path only
/// fires the Claude one-shot async repair (cooldown + single-flight gated).
#[cfg(unix)]
pub(super) fn on_idle_relay_drift(
    shared: &Arc<SharedData>,
    provider: ProviderKind,
    tmux_session_name: &str,
) {
    // Repair is Claude-only: the settings resolver is Claude-specific and the
    // durable DB column is written by the Claude/routine hook flow. Codex drift
    // is WARN-only at the resolver.
    if provider != ProviderKind::Claude {
        return;
    }
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }

    let now = Instant::now();
    let Some(guard) = try_begin_repair(tmux_session_name, now) else {
        return;
    };

    let shared = shared.clone();
    let tmux_session_name = tmux_session_name.to_string();
    super::task_supervisor::spawn_observed("idle_relay_drift_repair", async move {
        // The guard is moved into the task so the single-flight slot stays held
        // for the whole repair and is released (even on panic) on drop.
        attempt_drift_repair(&shared, &tmux_session_name, guard).await;
    });
}

#[cfg(unix)]
async fn attempt_drift_repair(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    _guard: RepairInflightGuard,
) {
    // (a) settings source — synchronous, no lock held.
    let settings_channel =
        super::tui_prompt_relay::resolve_rehydrated_claude_tmux_channel_id(tmux_session_name);

    // (b) durable DB source — `sessions.channel_id` for either the namespaced or
    // the legacy session key. Only consulted when storage is configured.
    let (db_channel, db_instance) = load_db_channel(shared, tmux_session_name).await;

    // Drift witness / agreement check value (read-only mirror use).
    let mirror_channel =
        crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(tmux_session_name);

    // Live-pane check on the blocking pool (synchronous tmux subprocess call).
    let pane_live = {
        let probe_name = tmux_session_name.to_string();
        tokio::task::spawn_blocking(move || {
            crate::services::tmux_diagnostics::tmux_session_has_live_pane(&probe_name)
        })
        .await
        .unwrap_or(false)
    };

    let inputs = RepairInputs {
        settings_channel,
        db_channel,
        db_instance,
        local_instance:
            crate::services::cluster::node_registry::resolve_self_instance_id_without_config(),
        mirror_channel,
        pane_live,
    };

    match evaluate_drift_repair(&inputs) {
        RepairDecision::Promote { source, channel } => {
            // All awaits are complete; `restore_owner_channel_for_tmux_session`
            // briefly takes the registry lock synchronously (no await held).
            let repaired = shared
                .tmux_watchers
                .restore_owner_channel_for_tmux_session(tmux_session_name, ChannelId::new(channel));
            if repaired {
                tracing::warn!(
                    tmux_session_name = %tmux_session_name,
                    channel_id = channel,
                    repair_source = source.label(),
                    provider = "claude",
                    "repaired authoritative tmux-session→channel registry (drift-triggered); \
                     idle relay can route again"
                );
            }
        }
        RepairDecision::Blocked(reason) => {
            tracing::warn!(
                tmux_session_name = %tmux_session_name,
                block_reason = reason.label(),
                db_channel_id = db_channel,
                mirror_channel_id = mirror_channel,
                provider = "claude",
                "idle relay drift repair blocked; keeping the drop (no mis-delivery)"
            );
        }
        RepairDecision::NoSource => {
            tracing::debug!(
                tmux_session_name = %tmux_session_name,
                provider = "claude",
                "idle relay drift repair found no durable source; keeping the drop"
            );
        }
    }
}

#[cfg(unix)]
async fn load_db_channel(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
) -> (Option<u64>, Option<String>) {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return (None, None);
    };
    let candidates = super::adk_session::build_session_key_candidates(
        &shared.token_hash,
        &ProviderKind::Claude,
        tmux_session_name,
    );
    for session_key in candidates {
        match crate::db::dispatched_sessions::load_session_channel_id_pg(pool, &session_key).await {
            Ok(Some((channel_id, instance_id))) => return (Some(channel_id), instance_id),
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(
                    session_key = %session_key,
                    error = %error,
                    "idle relay drift repair: session channel_id lookup failed"
                );
            }
        }
    }
    (None, None)
}

#[cfg(unix)]
impl RepairSource {
    fn label(self) -> &'static str {
        match self {
            RepairSource::Settings => "settings",
            RepairSource::SessionsTable => "sessions_table",
        }
    }
}

#[cfg(unix)]
impl BlockReason {
    fn label(self) -> &'static str {
        match self {
            BlockReason::ForeignInstance => "foreign_instance",
            BlockReason::DeadPane => "dead_pane",
            BlockReason::MirrorMismatch => "mirror_mismatch",
            BlockReason::NoMirrorWitness => "no_mirror_witness",
        }
    }
}

/// Test-only: clear all per-session drift state so rate-limiter / single-flight
/// tests do not leak across cases (the static map is process-global).
#[cfg(test)]
pub(super) fn reset_drift_state_for_tests() {
    let mut map = DRIFT_STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    map.clear();
}

#[cfg(test)]
pub(super) fn repair_attempt_recorded_for_tests(tmux_session_name: &str) -> bool {
    let map = DRIFT_STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    map.get(tmux_session_name)
        .is_some_and(|state| state.last_repair_attempt_at.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Serializes tests that touch the process-global `DRIFT_STATE` so parallel
    // execution can't clear another test's state mid-assertion (#3306 codex r1:
    // `repair_cooldown_and_single_flight_gate` raced a peer's
    // `reset_drift_state_for_tests`). Tests that only use a LOCAL `DriftState`
    // or the IO-free `evaluate_drift_repair` core do not need it.
    static DRIFT_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(unix)]
    fn inputs() -> RepairInputs {
        RepairInputs {
            settings_channel: None,
            db_channel: None,
            db_instance: None,
            local_instance: "host-1234".to_string(),
            mirror_channel: None,
            pane_live: true,
        }
    }

    // --- rate-limiter ---------------------------------------------------

    #[test]
    fn drift_warn_first_emits_then_suppresses_then_reemits() {
        let start = Instant::now();
        let mut state = DriftState::new(start);

        // First occurrence emits immediately, zero suppressed.
        let d0 = decide_drift_warn(&mut state, start);
        assert!(d0.emit, "first drift WARN must emit immediately");
        assert_eq!(d0.suppressed_count, 0);

        // Within the cooldown: suppressed, count accumulates.
        let t1 = start + Duration::from_secs(1);
        let d1 = decide_drift_warn(&mut state, t1);
        assert!(!d1.emit);
        assert_eq!(d1.suppressed_count, 1);
        let t2 = start + Duration::from_secs(2);
        let d2 = decide_drift_warn(&mut state, t2);
        assert!(!d2.emit);
        assert_eq!(d2.suppressed_count, 2);

        // After the cooldown: re-emit, carrying the accumulated suppressed count,
        // then reset.
        let t3 = start + DRIFT_WARN_COOLDOWN + Duration::from_secs(1);
        let d3 = decide_drift_warn(&mut state, t3);
        assert!(d3.emit, "WARN must re-emit after the cooldown window");
        assert_eq!(
            d3.suppressed_count, 2,
            "re-emit carries the suppressed count"
        );
        assert!(d3.drift_age_secs >= DRIFT_WARN_COOLDOWN.as_secs());

        // The next suppression window starts fresh.
        let t4 = t3 + Duration::from_secs(1);
        let d4 = decide_drift_warn(&mut state, t4);
        assert!(!d4.emit);
        assert_eq!(d4.suppressed_count, 1);
    }

    #[test]
    fn drift_warn_sessions_are_independent() {
        let _serial = DRIFT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_drift_state_for_tests();
        // Two distinct sessions each get their own immediate first emit.
        assert!(should_emit_drift_warn("tmux-a").emit);
        assert!(should_emit_drift_warn("tmux-b").emit);
        // Second hit on A within the window is suppressed (independent of B).
        assert!(!should_emit_drift_warn("tmux-a").emit);
        reset_drift_state_for_tests();
    }

    // --- repair decision core ------------------------------------------

    #[cfg(unix)]
    #[test]
    fn settings_hit_live_pane_promotes_settings() {
        let i = RepairInputs {
            settings_channel: Some(111),
            pane_live: true,
            ..inputs()
        };
        assert_eq!(
            evaluate_drift_repair(&i),
            RepairDecision::Promote {
                source: RepairSource::Settings,
                channel: 111
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn settings_miss_db_hit_mirror_agrees_instance_match_promotes_db() {
        let i = RepairInputs {
            db_channel: Some(222),
            mirror_channel: Some(222),
            db_instance: Some("host-1234".to_string()),
            local_instance: "host-1234".to_string(),
            pane_live: true,
            ..inputs()
        };
        assert_eq!(
            evaluate_drift_repair(&i),
            RepairDecision::Promote {
                source: RepairSource::SessionsTable,
                channel: 222
            }
        );
    }

    // #3306 KEY regression: session-name REUSE. The DB row still points at the
    // OLD channel (C1) while the new dispatch's mirror already recorded the NEW
    // channel (C2). Promotion MUST be blocked — mis-delivery is worse than a drop.
    #[cfg(unix)]
    #[test]
    fn db_channel_disagrees_with_mirror_blocks_misdelivery() {
        let i = RepairInputs {
            db_channel: Some(1),     // C1 (stale)
            mirror_channel: Some(2), // C2 (new dispatch witness)
            pane_live: true,
            ..inputs()
        };
        assert_eq!(
            evaluate_drift_repair(&i),
            RepairDecision::Blocked(BlockReason::MirrorMismatch)
        );
    }

    #[cfg(unix)]
    #[test]
    fn db_channel_without_mirror_witness_is_blocked() {
        let i = RepairInputs {
            db_channel: Some(222),
            mirror_channel: None,
            pane_live: true,
            ..inputs()
        };
        assert_eq!(
            evaluate_drift_repair(&i),
            RepairDecision::Blocked(BlockReason::NoMirrorWitness)
        );
    }

    #[cfg(unix)]
    #[test]
    fn db_channel_foreign_instance_is_blocked() {
        let i = RepairInputs {
            db_channel: Some(222),
            mirror_channel: Some(222),
            db_instance: Some("other-host-9999".to_string()),
            local_instance: "host-1234".to_string(),
            pane_live: true,
            ..inputs()
        };
        assert_eq!(
            evaluate_drift_repair(&i),
            RepairDecision::Blocked(BlockReason::ForeignInstance)
        );
    }

    #[cfg(unix)]
    #[test]
    fn dead_pane_blocks_any_candidate() {
        let i = RepairInputs {
            settings_channel: Some(111),
            pane_live: false,
            ..inputs()
        };
        assert_eq!(
            evaluate_drift_repair(&i),
            RepairDecision::Blocked(BlockReason::DeadPane)
        );
    }

    #[cfg(unix)]
    #[test]
    fn dead_pane_no_candidate_is_no_source() {
        let i = RepairInputs {
            pane_live: false,
            ..inputs()
        };
        assert_eq!(evaluate_drift_repair(&i), RepairDecision::NoSource);
    }

    #[cfg(unix)]
    #[test]
    fn all_sources_miss_is_no_source() {
        // #3018 semantics preserved: no durable source ⇒ keep the drop.
        assert_eq!(evaluate_drift_repair(&inputs()), RepairDecision::NoSource);
    }

    // --- cooldown / single-flight state machine ------------------------

    #[cfg(unix)]
    #[test]
    fn repair_cooldown_and_single_flight_gate() {
        let _serial = DRIFT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_drift_state_for_tests();
        let start = Instant::now();

        // First attempt claims the slot.
        let guard = try_begin_repair("tmux-x", start);
        assert!(guard.is_some(), "first repair attempt must be allowed");

        // While inflight, a concurrent attempt is refused.
        assert!(
            try_begin_repair("tmux-x", start + Duration::from_secs(1)).is_none(),
            "single-flight: no second inflight repair"
        );

        // Dropping the guard clears the inflight flag (even on panic).
        drop(guard);

        // Still within the cooldown ⇒ refused even though no longer inflight.
        assert!(
            try_begin_repair("tmux-x", start + Duration::from_secs(2)).is_none(),
            "60s cooldown blocks rapid re-attempts"
        );

        // After the cooldown ⇒ allowed again.
        assert!(
            try_begin_repair(
                "tmux-x",
                start + DRIFT_REPAIR_COOLDOWN + Duration::from_secs(1)
            )
            .is_some(),
            "cooldown elapsed ⇒ a new repair attempt is allowed"
        );
        reset_drift_state_for_tests();
    }
}
