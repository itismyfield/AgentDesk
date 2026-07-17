//! Exact-token Claude `/compact` triggering and busy-turn steering.
//!
//! The trigger never waits on a pane becoming idle. It owns an independent
//! per-session latch and hands a compact-specific submit to the narrow composer
//! mutation lock; normal TUI follow-ups retain their turn-lifetime lock.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use crate::services::claude_compact_context::{CompactThreshold, compact_threshold};
use crate::services::claude_tui::input::CompactSubmitOutcome;
use crate::services::provider::ProviderKind;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CompactLatchPaneKey {
    channel_id: u64,
    tmux_session_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CompactLatchIdentity {
    provider_session_id: String,
    model_selector: String,
    actual_window_tokens: u64,
    compact_percent: u64,
    lower_bound_tokens: u64,
    effective_threshold_tokens: u64,
    rearm_floor_tokens: u64,
}

#[derive(Clone, Debug)]
struct CompactLatchState {
    identity: CompactLatchIdentity,
    armed: bool,
    epoch: u64,
}

#[derive(Clone, Debug)]
struct CompactLatchTicket {
    pane: CompactLatchPaneKey,
    identity: CompactLatchIdentity,
    epoch: u64,
}

static LATCH_BY_PANE: LazyLock<Mutex<HashMap<CompactLatchPaneKey, CompactLatchState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_LATCH_EPOCH: AtomicU64 = AtomicU64::new(1);

/// Observe exact token occupancy at a turn-completion boundary. The lower bound
/// and ratio have already been combined into `threshold`; presentation percent
/// is deliberately not used for the decision.
pub(crate) fn maybe_inject_compact(
    channel_id: u64,
    tmux_session_name: &str,
    provider: &ProviderKind,
    provider_session_id: Option<&str>,
    model_selector: Option<&str>,
    usage_tokens: u64,
    actual_window_tokens: Option<u64>,
    compact_percent: u64,
    lower_bound_tokens: u64,
) {
    if !matches!(provider, ProviderKind::Claude) {
        return;
    }
    let pane = CompactLatchPaneKey {
        channel_id,
        tmux_session_name: tmux_session_name.trim().to_string(),
    };
    if pane.tmux_session_name.is_empty() {
        return;
    }
    // Zero is an explicit policy disable. Forget any old disarmed state even
    // when this completion lacks a model/window proof.
    if compact_percent == 0 {
        clear_pane_latch(&pane);
        return;
    }
    let Some(provider_session_id) = provider_session_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return;
    };
    let Some(model_selector) = model_selector
        .map(str::trim)
        .filter(|model| !model.is_empty())
    else {
        return;
    };
    let Some(actual_window_tokens) = actual_window_tokens else {
        return;
    };
    let Some(threshold) =
        compact_threshold(actual_window_tokens, compact_percent, lower_bound_tokens)
    else {
        return;
    };
    let identity = CompactLatchIdentity {
        provider_session_id: provider_session_id.to_string(),
        model_selector: model_selector.to_string(),
        actual_window_tokens,
        compact_percent,
        lower_bound_tokens,
        effective_threshold_tokens: threshold.effective_tokens,
        rearm_floor_tokens: threshold.rearm_floor_tokens,
    };
    let Some(ticket) = observe_and_decide(&pane, identity, usage_tokens, threshold) else {
        return;
    };

    // The exact-token latch is consumed before scheduling so concurrent
    // completion observations make at most one worker. This blocking work
    // performs no readiness wait and never acquires the turn-lifetime lock.
    tokio::task::spawn_blocking(move || {
        let outcome = crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
            &ticket.pane.tmux_session_name,
            || {
                crate::services::claude_tui::input::send_compact_while_busy(
                    &ticket.pane.tmux_session_name,
                )
            },
        );
        match outcome {
            CompactSubmitOutcome::AcceptedOrQueued => {
                tracing::info!(
                    tmux_session_name = %ticket.pane.tmux_session_name,
                    provider_session_id = %ticket.identity.provider_session_id,
                    model_selector = %ticket.identity.model_selector,
                    usage_tokens,
                    threshold_tokens = threshold.effective_tokens,
                    "Claude auto compact accepted or queued"
                );
            }
            CompactSubmitOutcome::PreMutationRefused => {
                rearm_after_pre_mutation_refusal(&ticket);
                tracing::debug!(
                    tmux_session_name = %ticket.pane.tmux_session_name,
                    "Claude auto compact refused before mutation; latch re-armed"
                );
            }
            CompactSubmitOutcome::AmbiguousAfterMutation => {
                // Never re-arm here. Retrying could enqueue a second compact.
                tracing::warn!(
                    tmux_session_name = %ticket.pane.tmux_session_name,
                    "Claude auto compact outcome ambiguous after tmux mutation; leaving latch disarmed without cleanup or retry"
                );
            }
        }
    });
}

fn observe_and_decide(
    pane: &CompactLatchPaneKey,
    identity: CompactLatchIdentity,
    usage_tokens: u64,
    threshold: CompactThreshold,
) -> Option<CompactLatchTicket> {
    let mut latches = LATCH_BY_PANE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let state = latches
        .entry(pane.clone())
        .or_insert_with(|| CompactLatchState {
            identity: identity.clone(),
            armed: true,
            epoch: NEXT_LATCH_EPOCH.fetch_add(1, Ordering::Relaxed),
        });
    if state.identity != identity {
        *state = CompactLatchState {
            identity,
            armed: true,
            epoch: NEXT_LATCH_EPOCH.fetch_add(1, Ordering::Relaxed),
        };
    }
    if !state.armed && usage_tokens <= threshold.rearm_floor_tokens {
        state.armed = true;
        return None;
    }
    if state.armed && usage_tokens >= threshold.effective_tokens {
        state.armed = false;
        return Some(CompactLatchTicket {
            pane: pane.clone(),
            identity: state.identity.clone(),
            epoch: state.epoch,
        });
    }
    None
}

fn rearm_after_pre_mutation_refusal(ticket: &CompactLatchTicket) {
    let mut latches = LATCH_BY_PANE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let Some(state) = latches.get_mut(&ticket.pane) else {
        return;
    };
    if state.epoch == ticket.epoch && state.identity == ticket.identity {
        state.armed = true;
    }
}

fn clear_pane_latch(pane: &CompactLatchPaneKey) {
    LATCH_BY_PANE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(pane);
}

/// Forget every compact latch tied to a physical tmux pane. This runs when a
/// new hosted Claude pane is prepared and when runtime bindings are torn down,
/// so a recreated pane cannot inherit an old ambiguous/disarmed latch.
pub(crate) fn clear_for_tmux(tmux_session_name: &str) {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return;
    }
    LATCH_BY_PANE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .retain(|pane, _| pane.tmux_session_name != tmux_session_name);
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    LATCH_BY_PANE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
}

/// The compact latch is process-global test state. Keep every stateful test
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

    fn pane() -> CompactLatchPaneKey {
        CompactLatchPaneKey {
            channel_id: 42,
            tmux_session_name: "tmux-4591".to_string(),
        }
    }

    fn identity(
        session_id: &str,
        model: &str,
        window: u64,
    ) -> (CompactLatchIdentity, CompactThreshold) {
        let compact_percent = 50;
        let lower_bound_tokens = 300_000;
        let threshold = compact_threshold(window, compact_percent, lower_bound_tokens).unwrap();
        (
            CompactLatchIdentity {
                provider_session_id: session_id.to_string(),
                model_selector: model.to_string(),
                actual_window_tokens: window,
                compact_percent,
                lower_bound_tokens,
                effective_threshold_tokens: threshold.effective_tokens,
                rearm_floor_tokens: threshold.rearm_floor_tokens,
            },
            threshold,
        )
    }

    #[test]
    fn exact_token_crossing_and_five_percent_hysteresis() {
        let _guard = state_test_guard();
        let pane = pane();
        let (identity, threshold) = identity("session-a", "claude-sonnet", 372_000);
        assert_eq!(threshold.effective_tokens, 300_000);
        assert_eq!(threshold.rearm_floor_tokens, 281_400);
        assert!(observe_and_decide(&pane, identity.clone(), 299_999, threshold).is_none());
        assert!(observe_and_decide(&pane, identity.clone(), 300_000, threshold).is_some());
        assert!(observe_and_decide(&pane, identity.clone(), 371_000, threshold).is_none());
        assert!(observe_and_decide(&pane, identity.clone(), 281_401, threshold).is_none());
        assert!(observe_and_decide(&pane, identity.clone(), 281_400, threshold).is_none());
        assert!(observe_and_decide(&pane, identity, 300_000, threshold).is_some());
    }

    #[test]
    fn post_mutation_unknown_stays_disarmed_but_pre_mutation_refusal_rearms() {
        let _guard = state_test_guard();
        let pane = pane();
        let (identity, threshold) = identity("session-a", "claude-sonnet", 1_000_000);
        let ticket = observe_and_decide(&pane, identity.clone(), 500_000, threshold).unwrap();
        // Ambiguous-after-mutation does not call rearm; another high poll cannot
        // enqueue a duplicate compact.
        assert!(observe_and_decide(&pane, identity.clone(), 700_000, threshold).is_none());
        rearm_after_pre_mutation_refusal(&ticket);
        assert!(observe_and_decide(&pane, identity, 700_000, threshold).is_some());
    }

    #[test]
    fn new_provider_session_model_or_window_gets_a_fresh_latch_identity() {
        let _guard = state_test_guard();
        let pane = pane();
        let (session_a, threshold_a) = identity("session-a", "claude-sonnet", 1_000_000);
        let first = observe_and_decide(&pane, session_a.clone(), 500_000, threshold_a).unwrap();
        assert_eq!(first.identity.provider_session_id, "session-a");

        let (session_b, threshold_b) = identity("session-b", "claude-sonnet", 1_000_000);
        let second = observe_and_decide(&pane, session_b.clone(), 500_000, threshold_b).unwrap();
        assert_eq!(second.identity.provider_session_id, "session-b");

        let (model_changed, threshold_model_changed) =
            identity("session-b", "claude-opus", 1_000_000);
        let third = observe_and_decide(
            &pane,
            model_changed.clone(),
            500_000,
            threshold_model_changed,
        )
        .unwrap();
        assert_eq!(third.identity.model_selector, "claude-opus");

        let (window_changed, threshold_window_changed) =
            identity("session-b", "claude-opus", 1_200_000);
        let fourth =
            observe_and_decide(&pane, window_changed, 600_000, threshold_window_changed).unwrap();
        assert_eq!(fourth.identity.actual_window_tokens, 1_200_000);
    }

    #[test]
    fn stale_refusal_ticket_cannot_rearm_replaced_or_cleared_latch() {
        let _guard = state_test_guard();
        let pane = pane();
        let (first_identity, first_threshold) = identity("session-a", "claude-sonnet", 1_000_000);
        let stale_ticket =
            observe_and_decide(&pane, first_identity, 500_000, first_threshold).unwrap();

        let (replacement, replacement_threshold) =
            identity("session-b", "claude-sonnet", 1_000_000);
        assert!(
            observe_and_decide(&pane, replacement.clone(), 500_000, replacement_threshold)
                .is_some()
        );
        rearm_after_pre_mutation_refusal(&stale_ticket);
        assert!(observe_and_decide(&pane, replacement, 700_000, replacement_threshold).is_none());

        clear_pane_latch(&pane);
        rearm_after_pre_mutation_refusal(&stale_ticket);
        assert!(
            LATCH_BY_PANE
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&pane)
                .is_none()
        );
    }

    #[test]
    fn clear_for_tmux_removes_every_channel_latch_for_recreated_pane() {
        let _guard = state_test_guard();
        let first_pane = pane();
        let second_pane = CompactLatchPaneKey {
            channel_id: 43,
            tmux_session_name: first_pane.tmux_session_name.clone(),
        };
        let (identity, threshold) = identity("session-a", "claude-sonnet", 1_000_000);
        assert!(observe_and_decide(&first_pane, identity.clone(), 500_000, threshold).is_some());
        assert!(observe_and_decide(&second_pane, identity.clone(), 500_000, threshold).is_some());

        clear_for_tmux(&first_pane.tmux_session_name);
        assert!(
            LATCH_BY_PANE
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_empty()
        );
        assert!(observe_and_decide(&first_pane, identity, 500_000, threshold).is_some());
    }

    #[test]
    fn zero_percent_clears_without_model_or_window_and_missing_proof_preserves_state() {
        let _guard = state_test_guard();
        let pane = pane();
        let (identity, threshold) = identity("session-a", "claude-sonnet", 1_000_000);
        assert!(observe_and_decide(&pane, identity.clone(), 500_000, threshold).is_some());

        // An incomplete completion must fail closed without accidentally
        // rearming or replacing a known disarmed identity.
        maybe_inject_compact(
            pane.channel_id,
            &pane.tmux_session_name,
            &ProviderKind::Claude,
            None,
            None,
            700_000,
            Some(1_000_000),
            50,
            300_000,
        );
        let state = LATCH_BY_PANE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&pane)
            .cloned()
            .expect("incomplete completion must retain the known latch");
        assert_eq!(state.identity, identity);
        assert!(!state.armed);

        // Zero is the one exception: it explicitly disables the policy and
        // clears even when the completion has no authoritative identity.
        maybe_inject_compact(
            pane.channel_id,
            &pane.tmux_session_name,
            &ProviderKind::Claude,
            None,
            None,
            0,
            None,
            0,
            300_000,
        );
        assert!(
            LATCH_BY_PANE
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&pane)
                .is_none()
        );
    }
}
