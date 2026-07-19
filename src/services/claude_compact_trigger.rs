//! Model-aware Claude `/compact` triggering and busy-turn steering.
//!
//! This trigger keeps main's proven *stateless observable-usage* shape: a single
//! per-pane `armed` bool, consumed once per fill cycle at a turn-completion
//! boundary and re-armed only after observable occupancy drops back below a
//! hysteresis floor (which is exactly what a real compaction does). It never owns
//! an identity/epoch/ticket lifecycle authority; the model-aware context window is
//! resolved fresh each turn by the caller (`claude_compact_context`) and only the
//! numeric threshold changes when the model/window changes.
//!
//! What #4591 keeps on top of main's shape:
//!   * a *token* threshold (`compact_threshold`) instead of a percentage, so the
//!     model-aware CTW resolution drives an exact absolute trigger, and
//!   * the steering primitive (`send_compact_while_busy` under the narrow
//!     per-pane composer lock) so an auto `/compact` steers a busy pane without
//!     waiting behind a normal turn's readiness phase.
//!
//! Lock discipline (the freeze-bug fix): the per-pane armed state lock is a
//! LEAF. It is taken only for a brief flip/read in [`observe_and_decide`],
//! [`pane_still_disarmed_for_send`], [`rearm_for_retry`], and [`clear_for_tmux`],
//! and is NEVER held across tmux I/O. Only the per-pane composer lock is held
//! across the send. This removes the old global-latch-held-across-submit
//! linearization that froze every pane's observation for the duration of a send.
//!
//! What #4591 rework adds on top of the stateless shape:
//!   * F2 — a per-pane fill-cycle GENERATION (a single process-global monotonic
//!     `u64`, owned in exactly one place under the leaf lock). Every crossing
//!     that consumes the armed flag stamps the pane with a fresh generation; a
//!     queued worker carries the generation it was spawned for and proceeds only
//!     on an exact match. Because the counter is globally monotonic and never
//!     reused, a same-name pane recreated after [`clear_for_tmux`] gets a
//!     strictly greater generation, so a stale worker can never match a NEW
//!     pane's `Some(false)` (closes the armed-bool ABA), and a late
//!     [`rearm_for_retry`] cannot clobber a newer crossing's consumed flag.
//!   * F3 — the pre-send recheck re-reads the freshest observed OCCUPANCY (also
//!     leaf-lock state) under the composer lock and bails when it has fallen
//!     below the trigger threshold, atomizing "is it still valid to send" with
//!     the send inside one composer critical section.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::services::claude_compact_context::{
    CLAUDE_AUTO_COMPACT_MAX_TOKENS, CompactThreshold, TurnWindowResolution, compact_threshold,
};
use crate::services::claude_tui::input::CompactSubmitOutcome;
use crate::services::discord::{ManagedCompactTurnIdentity, live_managed_turn_matches};
use crate::services::provider::ProviderKind;

/// Per-pane key for the once-per-fill-cycle armed flag. Keyed by both the Discord
/// channel and the physical tmux pane so [`clear_for_tmux`] can forget every
/// channel's flag for a recreated pane name (launch/teardown hygiene).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CompactPaneKey {
    channel_id: u64,
    tmux_session_name: String,
}

/// Per-pane once-per-fill-cycle state.
///
/// `armed == true` (the default, [`PaneArmState::fresh`]) means the next
/// threshold crossing may inject; injecting flips it to `false` until observable
/// occupancy drops below the re-arm floor. `generation` is the fill-cycle id
/// stamped by the crossing that last consumed `armed` (F2); `last_occupied` is
/// the freshest observed usage occupancy (F3). All three are read/written only
/// under [`COMPACT_TRIGGER_STATE`]'s leaf lock.
#[derive(Clone, Copy, Debug)]
struct PaneArmState {
    armed: bool,
    generation: u64,
    last_occupied: u64,
    last_window_tokens: u64,
    last_window_source: Option<CompactWindowSource>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services) enum CompactWindowSource {
    Proven,
    FallbackMax,
}

impl CompactWindowSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Proven => "proven",
            Self::FallbackMax => "fallback_max",
        }
    }
}

impl PaneArmState {
    fn fresh() -> Self {
        Self {
            armed: true,
            generation: 0,
            last_occupied: 0,
            last_window_tokens: 0,
            last_window_source: None,
        }
    }
}

/// The entire persistent trigger state: per-pane arm state plus the single
/// process-global monotonic fill-cycle counter (F2). One `Mutex` is the sole
/// authority for both, so a generation is never read or written anywhere else.
struct CompactTriggerState {
    panes: HashMap<CompactPaneKey, PaneArmState>,
    /// Monotonic fill-cycle counter. Only ever increases (one bump per consume),
    /// so no two crossings — across any pane, or a recreated same-name pane —
    /// ever share a generation. Generation `0` is reserved for a never-consumed
    /// pane, which no spawned worker ever carries.
    next_generation: u64,
}

static COMPACT_TRIGGER_STATE: LazyLock<Mutex<CompactTriggerState>> = LazyLock::new(|| {
    Mutex::new(CompactTriggerState {
        panes: HashMap::new(),
        next_generation: 0,
    })
});

/// Update the per-pane armed flag from the latest turn-completion occupancy and
/// report the fill-cycle generation to inject for (`Some(generation)`), or `None`
/// when THIS observation must not inject.
///
/// The re-arm check runs FIRST and is edge-triggered on an observable occupancy
/// drop (`occupied <= rearm_floor_tokens`): a real context reset — a landed
/// compaction — drops occupancy far below the threshold and re-arms; a small
/// jitter around the threshold does not. The inject check consumes the flag
/// optimistically the moment we decide to fire and stamps the pane with a fresh
/// generation, so two near-simultaneous completion observations cannot both
/// inject and a queued worker is bound to exactly its crossing. A non-confirmed
/// send restores the flag via [`rearm_for_retry`] so a later turn retries while
/// usage stays high.
///
/// `occupied` is the observable USAGE occupancy (`context_occupancy_input_tokens`
/// = input + cache_create + cache_read). Idempotency is keyed on this reliable
/// signal, never on a cosmetic `auto_compacted` string heuristic. Every call
/// records `occupied` as the pane's `last_occupied` for the F3 pre-send re-read.
fn observe_and_decide(
    pane: &CompactPaneKey,
    occupied: u64,
    threshold: CompactThreshold,
) -> Option<u64> {
    observe_and_decide_with_source(pane, occupied, threshold, CompactWindowSource::Proven)
}

fn observe_and_decide_with_source(
    pane: &CompactPaneKey,
    occupied: u64,
    threshold: CompactThreshold,
    window_source: CompactWindowSource,
) -> Option<u64> {
    let mut guard = COMPACT_TRIGGER_STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let state = &mut *guard;
    // Record the freshest occupancy and read the armed bit; the entry borrow ends
    // with this block so the generation counter can be mutated below.
    let armed = {
        let entry = state
            .panes
            .entry(pane.clone())
            .or_insert_with(PaneArmState::fresh);
        if entry.last_window_tokens != 0
            && (entry.last_window_tokens != threshold.actual_window_tokens
                || entry.last_window_source != Some(window_source))
        {
            // A model/window or proof-source switch starts a distinct fill cycle.
            // Invalidate the prior worker before this observation can consume a
            // fresh generation, including fallback-to-proven transitions at 1M.
            entry.armed = true;
            entry.generation = 0;
        }
        entry.last_occupied = occupied;
        entry.last_window_tokens = threshold.actual_window_tokens;
        entry.last_window_source = Some(window_source);
        entry.armed
    };
    // Re-arm first, independent of the inject check: a post-compact occupancy
    // drop (possibly to ~0) must re-arm a disarmed pane. It never injects and it
    // leaves the generation untouched (no new crossing was consumed).
    if !armed && occupied <= threshold.rearm_floor_tokens {
        if let Some(entry) = state.panes.get_mut(pane) {
            entry.armed = true;
        }
        return None;
    }
    if armed && occupied >= threshold.effective_tokens {
        // Optimistically consume the flag and stamp a fresh generation so
        // concurrent completion observations do not double-inject and the queued
        // worker is bound to THIS crossing. Restored by `rearm_for_retry` on a
        // non-confirmed send; re-armed naturally by the drop branch after a real
        // compact.
        let generation = state.next_generation.wrapping_add(1);
        state.next_generation = generation;
        if let Some(entry) = state.panes.get_mut(pane) {
            entry.armed = false;
            entry.generation = generation;
        }
        return Some(generation);
    }
    None
}

/// Observable pre-send revalidation, performed under the composer lock right
/// before the tmux mutation. A queued worker proceeds only when the pane is:
///   * still present AND still disarmed for `generation` — no observable
///     occupancy drop re-armed it and no NEW crossing (a different generation)
///     superseded it, and no teardown/policy-clear removed it (F2 ABA close),
///     AND
///   * still at/above the trigger threshold — the freshest observed occupancy
///     has not fallen below `effective_tokens`, so a compact is still warranted
///     (F3 occupancy re-read; a bare `Some(false)` bool cannot express a drop
///     into the hysteresis band).
/// The leaf lock is read and released here; it is NEVER held across the send.
fn pane_still_disarmed_for_send(
    pane: &CompactPaneKey,
    generation: u64,
    threshold: CompactThreshold,
) -> bool {
    let guard = COMPACT_TRIGGER_STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    guard.panes.get(pane).is_some_and(|entry| {
        !entry.armed
            && entry.generation == generation
            && entry.last_occupied >= threshold.effective_tokens
    })
}

/// Restore the armed flag after a non-confirmed send so a later turn-completion
/// retries `/compact` while usage stays high — observable retry. Generation-gated
/// (F2): the restore only lands while the pane is still on the worker's crossing
/// `generation`, so a stale worker cannot clobber a NEWER crossing's consumed
/// flag. Idempotent and resurrection-safe: a pane the teardown path already
/// removed stays removed, so a late worker cannot revive a stale entry.
fn rearm_for_retry(pane: &CompactPaneKey, generation: u64) {
    let mut guard = COMPACT_TRIGGER_STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(entry) = guard.panes.get_mut(pane) {
        if entry.generation == generation {
            entry.armed = true;
        }
    }
}

/// Invalidate a consumed cycle when its captured Managed authority was revoked
/// before mutation. The generation and context-window match make this a
/// compare-and-set: a stale worker cannot re-arm or clear a newer cycle. Resetting
/// the matched generation lets the next eligible Managed observation at already
/// high occupancy consume a fresh globally unique generation.
fn invalidate_after_authority_rejection(
    pane: &CompactPaneKey,
    generation: u64,
    threshold: CompactThreshold,
) {
    let mut guard = COMPACT_TRIGGER_STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(entry) = guard.panes.get_mut(pane) {
        if !entry.armed
            && entry.generation == generation
            && entry.last_window_tokens == threshold.actual_window_tokens
        {
            entry.armed = true;
            entry.generation = 0;
        }
    }
}

/// Run the observable pre-send recheck and, if the world is unchanged, the
/// compact submit — both inside a single per-pane composer critical section. The
/// composer lock (not the leaf state lock) is the only lock held across the tmux
/// mutation, so a queued worker may wait behind another composer mutation but
/// never carries the leaf state lock into `submit`. `None` means the pre-send
/// recheck bailed (stale/torn-down/below-threshold) and no mutation was
/// attempted.
fn submit_under_composer_lock(
    pane: &CompactPaneKey,
    generation: u64,
    threshold: CompactThreshold,
    expected_turn: &ManagedCompactTurnIdentity,
    live_turn_matches: impl FnOnce(&ManagedCompactTurnIdentity) -> bool,
    submit: impl FnOnce() -> CompactSubmitOutcome,
) -> Option<CompactSubmitOutcome> {
    crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
        &pane.tmux_session_name,
        || {
            if !pane_still_disarmed_for_send(pane, generation, threshold) {
                return None;
            }
            if !live_turn_matches(expected_turn) {
                invalidate_after_authority_rejection(pane, generation, threshold);
                return None;
            }
            Some(submit())
        },
    )
}

fn trigger_window_for_resolution(
    resolution: Option<TurnWindowResolution>,
) -> Option<(u64, CompactWindowSource)> {
    match resolution {
        None => None,
        Some(TurnWindowResolution::Proven(window)) => Some((window, CompactWindowSource::Proven)),
        Some(TurnWindowResolution::UnprovenLaunchBound) => Some((
            CLAUDE_AUTO_COMPACT_MAX_TOKENS,
            CompactWindowSource::FallbackMax,
        )),
    }
}

/// Validate and forward one complete active Claude usage snapshot. Missing model,
/// launch provenance, or any usage component fails closed before pane state exists.
pub(in crate::services) fn observe_active_usage(
    turn_identity: ManagedCompactTurnIdentity,
    provider: &ProviderKind,
    model: Option<&str>,
    input_tokens: Option<u64>,
    cache_create_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    compact_percent: u64,
    lower_bound_tokens: u64,
) -> bool {
    if !matches!(provider, ProviderKind::Claude) {
        return false;
    }
    let tmux_session_name = turn_identity.tmux_session_name();
    let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) else {
        return false;
    };
    let (Some(input_tokens), Some(cache_create_tokens), Some(cache_read_tokens)) =
        (input_tokens, cache_create_tokens, cache_read_tokens)
    else {
        return false;
    };
    let occupied = input_tokens
        .saturating_add(cache_create_tokens)
        .saturating_add(cache_read_tokens);
    let resolution = crate::services::claude_compact_context::context_window_for_turn(
        tmux_session_name,
        Some(model),
    );
    let Some((actual_window_tokens, window_source)) = trigger_window_for_resolution(resolution)
    else {
        tracing::debug!(
            tmux_session_name,
            model,
            occupied,
            "skipping Claude auto compact without launch provenance or model evidence"
        );
        return false;
    };
    if window_source == CompactWindowSource::FallbackMax {
        tracing::debug!(
            tmux_session_name,
            model,
            occupied,
            fallback_window_tokens = CLAUDE_AUTO_COMPACT_MAX_TOKENS,
            "using conservative maximum-window fallback for Claude auto compact"
        );
    }
    maybe_inject_compact_with_source(
        turn_identity,
        provider,
        occupied,
        Some(actual_window_tokens),
        compact_percent,
        lower_bound_tokens,
        window_source,
    );
    true
}

/// Claude-only: at a watcher-completed (pane-idle) turn boundary, inject
/// `/compact` into the live TUI when observable context occupancy first crosses
/// the model-aware token threshold this fill cycle.
///
/// `usage_tokens` is the observable occupancy (`context_occupancy_input_tokens`);
/// `actual_window_tokens` is the launch-provenance-resolved trigger window for
/// this turn: either an exact proven window or the conservative maximum-window
/// bound selected by the caller. The percentage/lower-bound are combined into an
/// absolute token threshold each turn, so a model/window change simply changes
/// the number on the next turn.
///
/// Degrade-safe no-ops that never touch the armed flag: a non-Claude provider
/// (Codex compacts natively), an unresolvable window, or a `0`/disabled percent.
/// A `0`/low `usage_tokens` is deliberately NOT short-circuited: it is the
/// post-compact re-arm signal handled inside [`observe_and_decide`].
pub(in crate::services) fn maybe_inject_compact(
    turn_identity: ManagedCompactTurnIdentity,
    provider: &ProviderKind,
    usage_tokens: u64,
    actual_window_tokens: Option<u64>,
    compact_percent: u64,
    lower_bound_tokens: u64,
) {
    maybe_inject_compact_with_source(
        turn_identity,
        provider,
        usage_tokens,
        actual_window_tokens,
        compact_percent,
        lower_bound_tokens,
        CompactWindowSource::Proven,
    );
}

pub(in crate::services) fn maybe_inject_compact_with_source(
    turn_identity: ManagedCompactTurnIdentity,
    provider: &ProviderKind,
    usage_tokens: u64,
    actual_window_tokens: Option<u64>,
    compact_percent: u64,
    lower_bound_tokens: u64,
    window_source: CompactWindowSource,
) {
    if !matches!(provider, ProviderKind::Claude) {
        return;
    }
    let channel_id = turn_identity.channel_id();
    let tmux_session_name = turn_identity.tmux_session_name();
    // Callers map launch-bound but unproven windows to the conservative maximum.
    // Missing provenance/model and zero-percent still degrade safely WITHOUT
    // touching the armed flag, like main's `threshold_pct == 0` short-circuit.
    let Some(actual_window_tokens) = actual_window_tokens else {
        tracing::debug!(
            tmux_session_name,
            "skipping Claude auto compact without a resolved trigger window"
        );
        return;
    };
    let Some(threshold) =
        compact_threshold(actual_window_tokens, compact_percent, lower_bound_tokens)
    else {
        return;
    };
    let pane = CompactPaneKey {
        channel_id,
        tmux_session_name: tmux_session_name.to_string(),
    };
    let Some(generation) =
        observe_and_decide_with_source(&pane, usage_tokens, threshold, window_source)
    else {
        return;
    };

    // The flag was just consumed, so at most one blocking worker exists per
    // crossing. This worker holds ONLY the per-pane composer lock across the tmux
    // mutation (never the leaf state lock) and performs no turn-readiness wait. It
    // carries `generation` so the pre-send recheck binds it to THIS crossing.
    tokio::task::spawn_blocking(move || {
        let outcome = submit_under_composer_lock(
            &pane,
            generation,
            threshold,
            &turn_identity,
            live_managed_turn_matches,
            || crate::services::claude_tui::input::send_compact_while_busy(&pane.tmux_session_name),
        );
        match outcome {
            None => tracing::debug!(
                tmux_session_name = %pane.tmux_session_name,
                "skipping stale Claude auto compact before tmux mutation (occupancy drop re-armed, pane torn down, or fell below threshold)"
            ),
            Some(CompactSubmitOutcome::AcceptedOrQueued) => tracing::info!(
                tmux_session_name = %pane.tmux_session_name,
                usage_tokens,
                threshold_tokens = threshold.effective_tokens,
                window_source = window_source.as_str(),
                "Claude auto compact accepted or queued"
            ),
            Some(CompactSubmitOutcome::PreMutationRefused) => {
                // No mutation happened (pane was not in an empty-composer state).
                // Re-arm so a later idle turn retries while usage stays high.
                rearm_for_retry(&pane, generation);
                tracing::debug!(
                    tmux_session_name = %pane.tmux_session_name,
                    "Claude auto compact refused before mutation; armed flag restored for retry"
                );
            }
            Some(CompactSubmitOutcome::AmbiguousAfterMutation) => {
                // Observable retry (replaces the old never-re-arm rule that could
                // permanently disable auto-compact and let context grow without
                // bound): re-arm, then `observe_and_decide` next turn only
                // re-injects when usage is still high AND no compaction was
                // observed. If the ambiguous send actually landed, occupancy
                // drops and the re-arm branch keeps it armed without re-injecting.
                rearm_for_retry(&pane, generation);
                tracing::warn!(
                    tmux_session_name = %pane.tmux_session_name,
                    "Claude auto compact outcome ambiguous after tmux mutation; armed flag restored for observable retry next turn"
                );
            }
        }
    });
}

/// Forget every compact armed flag tied to a physical tmux pane. This runs when a
/// new hosted Claude pane is prepared and when runtime bindings are torn down, so
/// a recreated pane cannot inherit an old disarmed flag under a reused name.
pub(crate) fn clear_for_tmux(tmux_session_name: &str) {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return;
    }
    // Removal alone invalidates every stale worker: because `next_generation` is
    // global-monotonic and never reset here, a same-name pane recreated after
    // this clear re-inserts as `PaneArmState::fresh` (generation 0) and its first
    // crossing consumes a strictly greater generation than any prior worker holds.
    COMPACT_TRIGGER_STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .panes
        .retain(|pane, _| pane.tmux_session_name != tmux_session_name);
}

#[cfg(test)]
fn reset_for_test() {
    let mut guard = COMPACT_TRIGGER_STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    guard.panes.clear();
    guard.next_generation = 0;
}

/// The armed flag is process-global test state. Keep every stateful test
/// serialized so one fixture cannot clear another under normal parallel runs.
#[cfg(test)]
pub(crate) static STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Acquire the shared state guard and reset all compact-trigger test state while
/// the guard is held. The caller retains the guard for its entire test.
#[cfg(test)]
fn state_test_guard() -> std::sync::MutexGuard<'static, ()> {
    let guard = STATE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    reset_for_test();
    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane() -> CompactPaneKey {
        CompactPaneKey {
            channel_id: 42,
            tmux_session_name: "tmux-4591".to_string(),
        }
    }

    // window 1_000_000, percent 50, lower 300_000 → effective 500_000,
    // rearm_floor = 500_000 - 5% * 1_000_000 = 450_000.
    fn threshold_for(window: u64) -> CompactThreshold {
        compact_threshold(window, 50, 300_000).expect("valid threshold fixture")
    }

    fn armed_state(pane: &CompactPaneKey) -> Option<bool> {
        COMPACT_TRIGGER_STATE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .panes
            .get(pane)
            .map(|entry| entry.armed)
    }

    fn panes_is_empty() -> bool {
        COMPACT_TRIGGER_STATE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .panes
            .is_empty()
    }

    fn last_window_tokens(pane: &CompactPaneKey) -> Option<u64> {
        COMPACT_TRIGGER_STATE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .panes
            .get(pane)
            .map(|entry| entry.last_window_tokens)
    }

    /// Mutation guard: the window-change invalidation in `observe_and_decide`.
    /// Changing context windows must issue a distinct generation, reject the old
    /// queued worker at the composer barrier, and ignore its later retry re-arm.
    #[test]
    fn window_switch_invalidates_old_worker_and_starts_a_fresh_cycle() {
        let _guard = state_test_guard();
        let pane = pane();
        let old_threshold = threshold_for(1_000_000);
        let old_generation =
            observe_and_decide(&pane, 600_000, old_threshold).expect("old window crossing");

        let new_threshold = threshold_for(800_000);
        let new_generation =
            observe_and_decide(&pane, 600_000, new_threshold).expect("new window crossing");

        assert_ne!(old_generation, new_generation);
        assert_eq!(last_window_tokens(&pane), Some(800_000));
        assert!(
            !pane_still_disarmed_for_send(&pane, old_generation, old_threshold),
            "a worker bound to the prior window must fail the composer barrier"
        );
        rearm_for_retry(&pane, old_generation);
        assert_eq!(
            armed_state(&pane),
            Some(false),
            "a stale retry must not re-arm the new window's consumed cycle"
        );
        assert!(pane_still_disarmed_for_send(
            &pane,
            new_generation,
            new_threshold
        ));
    }

    /// Mutation guards: mapping a catalog miss back to `None` fails the active
    /// usage assertions, while lowering the maximum fallback fires at 499K.
    #[test]
    fn suffixed_only_catalog_bare_model_fires_at_max_window_threshold_only() {
        let _context_guard = crate::services::claude_compact_context::state_test_guard();
        let _trigger_guard = state_test_guard();
        let proxy = "http://proxy-4678-suffixed-only.test";
        crate::services::claude_compact_context::put_catalog_for_test(
            proxy,
            HashMap::from([
                ("claude-opus-4-8-hgq".to_string(), 1_000_000),
                ("claude-opus-4-8-j97".to_string(), 1_000_000),
            ]),
        );
        let gateway = crate::services::claude_gateway_proxy::ClaudeGatewayProxyEnv::Inject {
            base_url: proxy.to_string(),
        };
        crate::services::claude_compact_context::register_launch_provenance(
            "tmux-4678-high",
            &gateway,
        );
        crate::services::claude_compact_context::register_launch_provenance(
            "tmux-4678-low",
            &gateway,
        );

        let high_resolution = crate::services::claude_compact_context::context_window_for_turn(
            "tmux-4678-high",
            Some("claude-opus-4-8"),
        );
        let (window, source) =
            trigger_window_for_resolution(high_resolution).expect("launch-bound trigger window");
        let threshold = threshold_for(window);
        let high = CompactPaneKey {
            channel_id: 42,
            tmux_session_name: "tmux-4678-high".to_string(),
        };
        assert!(observe_and_decide_with_source(&high, 552_000, threshold, source).is_some());

        let low_resolution = crate::services::claude_compact_context::context_window_for_turn(
            "tmux-4678-low",
            Some("claude-opus-4-8"),
        );
        let (window, source) =
            trigger_window_for_resolution(low_resolution).expect("launch-bound trigger window");
        let low = CompactPaneKey {
            channel_id: 42,
            tmux_session_name: "tmux-4678-low".to_string(),
        };
        assert!(
            observe_and_decide_with_source(&low, 499_000, threshold_for(window), source).is_none()
        );
        assert_eq!(armed_state(&low), Some(true));
        assert_eq!(
            last_window_tokens(&low),
            Some(CLAUDE_AUTO_COMPACT_MAX_TOKENS)
        );
    }

    /// Mutation guard: replacing the conservative 1M fallback with a native 200K
    /// window makes the 350K assertion inject early.
    #[test]
    fn scrub_fallback_never_uses_a_small_native_trigger() {
        let _guard = state_test_guard();
        let (window, source) =
            trigger_window_for_resolution(Some(TurnWindowResolution::UnprovenLaunchBound))
                .expect("scrub launch-bound fallback");
        let threshold = threshold_for(window);
        for (channel_id, occupied) in [(42, 199_000), (43, 350_000)] {
            let pane = CompactPaneKey {
                channel_id,
                tmux_session_name: format!("tmux-scrub-{occupied}"),
            };
            assert!(observe_and_decide_with_source(&pane, occupied, threshold, source).is_none());
        }
    }

    /// Mutation guard: routing a proven exact hit through the 1M fallback makes
    /// the 350K crossing miss its 372K-window threshold.
    #[test]
    fn proven_window_takes_priority_over_maximum_fallback() {
        let _guard = state_test_guard();
        let (window, source) =
            trigger_window_for_resolution(Some(TurnWindowResolution::Proven(372_000)))
                .expect("proven exact hit");
        assert_eq!(source, CompactWindowSource::Proven);
        assert!(
            observe_and_decide_with_source(&pane(), 350_000, threshold_for(window), source)
                .is_some()
        );
    }

    /// Mutation guard: ignoring proof source when the numeric window remains 1M
    /// lets the fallback worker survive and prevents a fresh proven generation.
    #[test]
    fn fallback_to_proven_transition_invalidates_old_generation() {
        let _guard = state_test_guard();
        let pane = pane();
        let threshold = threshold_for(CLAUDE_AUTO_COMPACT_MAX_TOKENS);
        let old_generation = observe_and_decide_with_source(
            &pane,
            552_000,
            threshold,
            CompactWindowSource::FallbackMax,
        )
        .expect("fallback crossing");
        let new_generation =
            observe_and_decide_with_source(&pane, 552_000, threshold, CompactWindowSource::Proven)
                .expect("proven crossing starts a fresh generation");
        assert_ne!(old_generation, new_generation);
        assert!(!pane_still_disarmed_for_send(
            &pane,
            old_generation,
            threshold
        ));
        assert!(pane_still_disarmed_for_send(
            &pane,
            new_generation,
            threshold
        ));
    }

    #[test]
    fn active_usage_missing_model_or_provenance_does_not_create_pane_state() {
        let _guard = state_test_guard();
        assert!(!observe_active_usage(
            ManagedCompactTurnIdentity::test_fixture(42, "tmux-4631"),
            &ProviderKind::Claude,
            None,
            Some(560_000),
            Some(0),
            Some(0),
            50,
            300_000,
        ));
        assert!(panes_is_empty());
        assert!(!observe_active_usage(
            ManagedCompactTurnIdentity::test_fixture(42, "tmux-4631"),
            &ProviderKind::Claude,
            Some("routed-sonnet[1m]"),
            Some(560_000),
            Some(0),
            Some(0),
            50,
            300_000,
        ));
        assert!(panes_is_empty());
    }

    #[test]
    fn active_usage_rejects_partial_usage_before_creating_pane_state() {
        let _guard = state_test_guard();
        assert!(!observe_active_usage(
            ManagedCompactTurnIdentity::test_fixture(42, "tmux-4631"),
            &ProviderKind::Claude,
            Some("routed-sonnet[1m]"),
            Some(560_000),
            None,
            Some(0),
            50,
            300_000,
        ));
        assert!(panes_is_empty());
    }

    #[test]
    fn active_usage_non_claude_provider_does_not_create_pane_state() {
        let _guard = state_test_guard();
        assert!(!observe_active_usage(
            ManagedCompactTurnIdentity::test_fixture(42, "tmux-4631"),
            &ProviderKind::Codex,
            Some("routed-sonnet[1m]"),
            Some(560_000),
            Some(0),
            Some(0),
            50,
            300_000,
        ));
        assert!(panes_is_empty());
    }

    /// Mutation guard: the optimistic consume (disarm + fresh generation) in the
    /// inject branch of `observe_and_decide`. Reverting it (not disarming on
    /// inject) makes the second/third parked observation inject again, so the
    /// `.is_none()` lines below fail with a double injection while usage stays
    /// parked above the threshold.
    #[test]
    fn armed_bool_consumed_once_per_fill_cycle() {
        let _guard = state_test_guard();
        let pane = pane();
        let threshold = threshold_for(1_000_000);
        assert!(observe_and_decide(&pane, 600_000, threshold).is_some());
        assert!(observe_and_decide(&pane, 600_000, threshold).is_none());
        assert!(observe_and_decide(&pane, 999_000, threshold).is_none());
        assert_eq!(armed_state(&pane), Some(false));
    }

    /// Mutation guard: the `occupied <= rearm_floor_tokens` condition on the
    /// re-arm branch. Reverting it (re-arm whenever `!armed`, without an
    /// observable occupancy drop) re-arms on the first parked poll, so the
    /// second parked `observe_and_decide(&pane, 600_000, ...)` finds the pane
    /// re-armed and injects — the `.is_none()` on that line fails, i.e. a
    /// `/compact` flood every turn with no genuine compaction between them.
    #[test]
    fn rearm_requires_observable_occupancy_drop() {
        let _guard = state_test_guard();
        let pane = pane();
        let threshold = threshold_for(1_000_000);
        // First crossing injects and disarms.
        assert!(observe_and_decide(&pane, 600_000, threshold).is_some());
        // Parked above the re-arm floor: no re-arm, and therefore no re-inject on
        // any later poll while still parked.
        assert!(observe_and_decide(&pane, 600_000, threshold).is_none());
        // A dip that does NOT reach the floor (460_000 > 450_000) still no-ops.
        assert!(observe_and_decide(&pane, 460_000, threshold).is_none());
        assert!(observe_and_decide(&pane, 600_000, threshold).is_none());
        assert_eq!(armed_state(&pane), Some(false));
        // A genuine occupancy drop to/below the floor re-arms (compaction seen)
        // and does not itself inject.
        assert!(observe_and_decide(&pane, 450_000, threshold).is_none());
        assert_eq!(armed_state(&pane), Some(true));
        // The next genuine crossing injects again.
        assert!(observe_and_decide(&pane, 600_000, threshold).is_some());
    }

    /// Mutation guard: `pane_still_disarmed_for_send` (the observable pre-send
    /// recheck). After the latch is consumed for a crossing, an observed
    /// occupancy drop re-arms the pane (a compaction landed). Reverting the
    /// recheck to `true` (or to `matches!(get, Some(_))`, ignoring the disarmed
    /// bool) makes the final assert fail — the queued worker would send a STALE
    /// second `/compact` after the context was already reset.
    #[test]
    fn pre_send_recheck_bails_when_occupancy_drop_rearmed_the_pane() {
        let _guard = state_test_guard();
        let pane = pane();
        let threshold = threshold_for(1_000_000);
        // Cross → consume; the queued worker would still see the pane disarmed.
        let generation = observe_and_decide(&pane, 500_000, threshold).expect("crossing injects");
        assert!(pane_still_disarmed_for_send(&pane, generation, threshold));
        // A later completion observes a compaction (occupancy drop) → re-arm.
        assert!(observe_and_decide(&pane, 400_000, threshold).is_none());
        // The pre-send recheck must now bail: the world changed.
        assert!(!pane_still_disarmed_for_send(&pane, generation, threshold));
    }

    /// F3 mutation guard: the `last_occupied >= effective_tokens` clause of
    /// `pane_still_disarmed_for_send` (the occupancy re-read). After a crossing
    /// consumes the flag, a later completion whose usage falls BELOW the trigger
    /// threshold but stays ABOVE the re-arm floor leaves the pane `Some(false)`
    /// (armed bool unchanged, no re-arm), so a recheck reading ONLY the bool would
    /// pass. Reverting the occupancy clause makes the final assert fail — a stale
    /// `/compact` would be sent to a pane that already fell below threshold.
    #[test]
    fn pre_send_recheck_bails_when_occupancy_fell_below_threshold_without_rearming() {
        let _guard = state_test_guard();
        let pane = pane();
        // effective_tokens = 500_000, rearm_floor_tokens = 450_000.
        let threshold = threshold_for(1_000_000);
        let generation = observe_and_decide(&pane, 600_000, threshold).expect("crossing injects");
        assert!(pane_still_disarmed_for_send(&pane, generation, threshold));
        // Hysteresis band: below effective (500_000) but above the floor
        // (450_000), so the pane stays disarmed rather than re-arming.
        assert!(observe_and_decide(&pane, 470_000, threshold).is_none());
        assert_eq!(
            armed_state(&pane),
            Some(false),
            "a hysteresis-band drop must NOT re-arm the pane"
        );
        // The bool alone still reads "disarmed" — only the occupancy re-read bails.
        assert!(!pane_still_disarmed_for_send(&pane, generation, threshold));
    }

    /// F2 mutation guard: the `generation == generation` match in both
    /// `pane_still_disarmed_for_send` and `rearm_for_retry`. A worker queued for
    /// an OLD crossing must neither send nor re-arm after `clear_for_tmux` + a NEW
    /// same-name crossing re-issued the same `Some(false)` armed value under a
    /// strictly greater generation (the armed-bool ABA). Reverting either
    /// generation check (matching only the bool) fails an assert below: the stale
    /// recheck would pass, or the stale re-arm would clobber the new crossing's
    /// consumed flag back to `true`.
    #[test]
    fn stale_generation_worker_neither_sends_nor_rearms_after_pane_recreated() {
        let _guard = state_test_guard();
        let pane = pane();
        let threshold = threshold_for(1_000_000);
        // Old pane crossing consumes an old generation.
        let old_generation =
            observe_and_decide(&pane, 600_000, threshold).expect("old crossing injects");
        // Teardown + same-name pane recreation, then a NEW crossing consumes.
        clear_for_tmux(&pane.tmux_session_name);
        let new_generation =
            observe_and_decide(&pane, 600_000, threshold).expect("recreated crossing injects");
        assert_ne!(
            old_generation, new_generation,
            "a recreated pane's crossing must get a fresh generation"
        );
        // The stale worker must NOT send for the new pane's crossing...
        assert!(!pane_still_disarmed_for_send(
            &pane,
            old_generation,
            threshold
        ));
        // ...and its late re-arm must NOT clobber the new crossing's consumed flag.
        rearm_for_retry(&pane, old_generation);
        assert_eq!(
            armed_state(&pane),
            Some(false),
            "a stale-generation re-arm must be a no-op for a newer crossing"
        );
        // The current-generation worker remains valid.
        assert!(pane_still_disarmed_for_send(
            &pane,
            new_generation,
            threshold
        ));
    }

    /// Mutation guard: the `None` (teardown) arm of `pane_still_disarmed_for_send`.
    /// A pane torn down while a worker was queued must not send. Reverting the
    /// recheck lets the worker send into a recreated/absent pane.
    #[test]
    fn pre_send_recheck_bails_after_teardown_clear() {
        let _guard = state_test_guard();
        let pane = pane();
        let threshold = threshold_for(1_000_000);
        let generation = observe_and_decide(&pane, 600_000, threshold).expect("crossing injects");
        assert!(pane_still_disarmed_for_send(&pane, generation, threshold));
        clear_for_tmux(&pane.tmux_session_name);
        assert!(!pane_still_disarmed_for_send(&pane, generation, threshold));
    }

    /// The worker is parked behind the physical pane's composer lock. While it is
    /// parked, the live turn authority changes from the captured Managed identity
    /// to ExternalInput or ExternalAdopted. The post-lock authority recheck must
    /// reject the stale worker before the submit closure can mutate the composer.
    #[cfg(unix)]
    #[test]
    fn queued_worker_revalidates_live_turn_authority_after_composer_barrier() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::{Arc, mpsc};
        use std::time::Duration;

        #[derive(Debug)]
        enum AuthorityTransition {
            ExternalInput,
            ExternalAdopted,
        }

        for external_source in [
            AuthorityTransition::ExternalInput,
            AuthorityTransition::ExternalAdopted,
        ] {
            let _guard = state_test_guard();
            let pane = pane();
            let threshold = threshold_for(1_000_000);
            let generation =
                observe_and_decide(&pane, 500_000, threshold).expect("crossing injects");
            let expected_turn =
                ManagedCompactTurnIdentity::test_fixture(42, &pane.tmux_session_name);
            let authority_is_managed = Arc::new(AtomicBool::new(true));
            let sends = Arc::new(AtomicUsize::new(0));
            let (queued_tx, queued_rx) = mpsc::channel();
            let (outcome_tx, outcome_rx) = mpsc::channel();
            let worker_pane = pane.clone();
            let worker_authority = Arc::clone(&authority_is_managed);
            let worker_sends = Arc::clone(&sends);

            crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
                &pane.tmux_session_name,
                || {
                    let worker = std::thread::spawn(move || {
                        queued_tx.send(()).expect("signal queued compact worker");
                        let outcome = submit_under_composer_lock(
                            &worker_pane,
                            generation,
                            threshold,
                            &expected_turn,
                            |_| worker_authority.load(Ordering::SeqCst),
                            || {
                                worker_sends.fetch_add(1, Ordering::SeqCst);
                                CompactSubmitOutcome::AcceptedOrQueued
                            },
                        );
                        outcome_tx.send(outcome).expect("return compact outcome");
                    });
                    queued_rx
                        .recv_timeout(Duration::from_millis(250))
                        .expect("worker must queue behind the held composer lock");
                    assert!(outcome_rx.recv_timeout(Duration::from_millis(25)).is_err());
                    authority_is_managed.store(false, Ordering::SeqCst);
                    worker
                },
            )
            .join()
            .expect("compact worker thread");

            assert_eq!(
                outcome_rx.recv_timeout(Duration::from_millis(250)).unwrap(),
                None,
                "{external_source:?} must reject the queued worker"
            );
            assert_eq!(
                sends.load(Ordering::SeqCst),
                0,
                "{external_source:?} must cause zero composer mutations"
            );

            let fresh_generation = observe_and_decide(&pane, 500_000, threshold)
                .expect("a later Managed observation must start a fresh high-occupancy cycle");
            assert_ne!(generation, fresh_generation);
            assert!(
                observe_and_decide(&pane, 500_000, threshold).is_none(),
                "the fresh Managed cycle must schedule exactly once"
            );
            assert!(pane_still_disarmed_for_send(
                &pane,
                fresh_generation,
                threshold
            ));
            clear_for_tmux(&pane.tmux_session_name);
        }
    }

    #[test]
    fn stale_authority_rejection_cannot_rearm_a_newer_cycle() {
        let _guard = state_test_guard();
        let pane = pane();
        let threshold = threshold_for(1_000_000);
        let old_generation =
            observe_and_decide(&pane, 600_000, threshold).expect("old crossing injects");

        invalidate_after_authority_rejection(&pane, old_generation, threshold);
        let new_generation =
            observe_and_decide(&pane, 600_000, threshold).expect("fresh crossing injects");
        assert_ne!(old_generation, new_generation);

        invalidate_after_authority_rejection(&pane, old_generation, threshold);
        rearm_for_retry(&pane, old_generation);
        assert_eq!(
            armed_state(&pane),
            Some(false),
            "a stale rejection or retry must not re-arm the newer consumed cycle"
        );
        assert!(pane_still_disarmed_for_send(
            &pane,
            new_generation,
            threshold
        ));
    }

    /// Observable retry: a non-confirmed send (`PreMutationRefused` or
    /// `AmbiguousAfterMutation`) restores the flag so the next turn re-injects
    /// only while usage stays high, and a real compaction (occupancy drop) is
    /// still observed instead of re-firing. Mutation guard: `rearm_for_retry`.
    /// Reverting it (leave disarmed on ambiguous) permanently disables
    /// auto-compact for a stuck-high pane — the `observe_and_decide(&pane,
    /// 700_000, ...)` re-inject assert fails.
    #[test]
    fn ambiguous_after_mutation_rearms_for_observable_retry() {
        let _guard = state_test_guard();
        let pane = pane();
        let threshold = threshold_for(1_000_000);
        let generation = observe_and_decide(&pane, 700_000, threshold).expect("crossing injects");
        assert_eq!(armed_state(&pane), Some(false));
        // Simulate the worker's ambiguous-after-mutation outcome.
        rearm_for_retry(&pane, generation);
        assert_eq!(armed_state(&pane), Some(true));
        // Usage still high and no compaction observed → retry next turn.
        assert!(observe_and_decide(&pane, 700_000, threshold).is_some());
    }

    /// A pane the teardown path removed must not be resurrected by a late
    /// worker's re-arm. Mutation guard: the `if let Some(..)` presence check in
    /// `rearm_for_retry`. Reverting it to an unconditional insert re-creates a
    /// stale entry, so the `armed_state(&pane).is_none()` assertion fails.
    #[test]
    fn rearm_after_teardown_does_not_resurrect_a_cleared_pane() {
        let _guard = state_test_guard();
        let pane = pane();
        let threshold = threshold_for(1_000_000);
        let generation = observe_and_decide(&pane, 600_000, threshold).expect("crossing injects");
        clear_for_tmux(&pane.tmux_session_name);
        rearm_for_retry(&pane, generation);
        assert!(armed_state(&pane).is_none());
    }

    /// `clear_for_tmux` forgets every channel's flag for a recreated pane name.
    #[test]
    fn clear_for_tmux_removes_every_channel_flag_for_recreated_pane() {
        let _guard = state_test_guard();
        let first_pane = pane();
        let second_pane = CompactPaneKey {
            channel_id: 43,
            tmux_session_name: first_pane.tmux_session_name.clone(),
        };
        let threshold = threshold_for(1_000_000);
        assert!(observe_and_decide(&first_pane, 600_000, threshold).is_some());
        assert!(observe_and_decide(&second_pane, 600_000, threshold).is_some());
        clear_for_tmux(&first_pane.tmux_session_name);
        assert!(panes_is_empty());
        // A recreated pane starts armed again.
        assert!(observe_and_decide(&first_pane, 600_000, threshold).is_some());
    }

    /// Degrade-safe no-inject paths never touch the armed flag and never spawn a
    /// worker (so these are safe to call without a Tokio runtime): a non-Claude
    /// provider, an unresolvable window, and a zero/disabled percent.
    #[test]
    fn maybe_inject_degrades_safely_without_touching_the_flag() {
        let _guard = state_test_guard();
        let pane = pane();
        maybe_inject_compact(
            ManagedCompactTurnIdentity::test_fixture(pane.channel_id, &pane.tmux_session_name),
            &ProviderKind::Codex,
            600_000,
            Some(1_000_000),
            50,
            300_000,
        );
        assert!(armed_state(&pane).is_none());
        maybe_inject_compact(
            ManagedCompactTurnIdentity::test_fixture(pane.channel_id, &pane.tmux_session_name),
            &ProviderKind::Claude,
            600_000,
            None,
            50,
            300_000,
        );
        assert!(armed_state(&pane).is_none());
        maybe_inject_compact(
            ManagedCompactTurnIdentity::test_fixture(pane.channel_id, &pane.tmux_session_name),
            &ProviderKind::Claude,
            600_000,
            Some(1_000_000),
            0,
            300_000,
        );
        assert!(armed_state(&pane).is_none());
    }
}
