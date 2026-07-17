//! Exact-token Claude `/compact` triggering and busy-turn steering.
//!
//! The trigger never waits on a pane becoming idle. It owns an independent
//! per-session latch and hands a compact-specific submit to the narrow composer
//! mutation lock; normal TUI follow-ups retain their turn-lifetime lock.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::services::claude_compact_context::{CompactThreshold, compact_threshold};
use crate::services::claude_tui::input::CompactSubmitOutcome;
use crate::services::provider::ProviderKind;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CompactLatchKey {
    channel_id: u64,
    tmux_session_name: String,
}

static ARMED_BY_SESSION: LazyLock<Mutex<HashMap<CompactLatchKey, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Observe exact token occupancy at a turn-completion boundary. The lower bound
/// and ratio have already been combined into `threshold`; presentation percent
/// is deliberately not used for the decision.
pub(crate) fn maybe_inject_compact(
    channel_id: u64,
    tmux_session_name: &str,
    provider: &ProviderKind,
    usage_tokens: u64,
    actual_window_tokens: u64,
    compact_percent: u64,
    lower_bound_tokens: u64,
) {
    if !matches!(provider, ProviderKind::Claude) {
        return;
    }
    let Some(threshold) =
        compact_threshold(actual_window_tokens, compact_percent, lower_bound_tokens)
    else {
        return;
    };
    let key = CompactLatchKey {
        channel_id,
        tmux_session_name: tmux_session_name.trim().to_string(),
    };
    if key.tmux_session_name.is_empty() || !observe_and_decide(&key, usage_tokens, threshold) {
        return;
    }

    // The exact-token latch is consumed before scheduling so concurrent
    // completion observations make at most one worker. This blocking work
    // performs no readiness wait and never acquires the turn-lifetime lock.
    tokio::task::spawn_blocking(move || {
        let outcome = crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
            &key.tmux_session_name,
            || crate::services::claude_tui::input::send_compact_while_busy(&key.tmux_session_name),
        );
        match outcome {
            CompactSubmitOutcome::AcceptedOrQueued => {
                tracing::info!(
                    tmux_session_name = %key.tmux_session_name,
                    usage_tokens,
                    threshold_tokens = threshold.effective_tokens,
                    "Claude auto compact accepted or queued"
                );
            }
            CompactSubmitOutcome::PreMutationRefused => {
                rearm_after_pre_mutation_refusal(&key);
                tracing::debug!(
                    tmux_session_name = %key.tmux_session_name,
                    "Claude auto compact refused before mutation; latch re-armed"
                );
            }
            CompactSubmitOutcome::AmbiguousAfterMutation => {
                // Never re-arm here. Retrying could enqueue a second compact.
                tracing::warn!(
                    tmux_session_name = %key.tmux_session_name,
                    "Claude auto compact outcome ambiguous after tmux mutation; leaving latch disarmed without cleanup or retry"
                );
            }
        }
    });
}

fn observe_and_decide(
    key: &CompactLatchKey,
    usage_tokens: u64,
    threshold: CompactThreshold,
) -> bool {
    let mut latches = ARMED_BY_SESSION
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let armed = *latches.entry(key.clone()).or_insert(true);
    if !armed && usage_tokens <= threshold.rearm_floor_tokens {
        latches.insert(key.clone(), true);
        return false;
    }
    if armed && usage_tokens >= threshold.effective_tokens {
        latches.insert(key.clone(), false);
        return true;
    }
    false
}

fn rearm_after_pre_mutation_refusal(key: &CompactLatchKey) {
    ARMED_BY_SESSION
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(key.clone(), true);
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    ARMED_BY_SESSION
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

    fn key() -> CompactLatchKey {
        CompactLatchKey {
            channel_id: 42,
            tmux_session_name: "tmux-4591".to_string(),
        }
    }

    #[test]
    fn exact_token_crossing_and_five_percent_hysteresis() {
        let _guard = state_test_guard();
        let threshold = compact_threshold(372_000, 50, 300_000).unwrap();
        assert_eq!(threshold.effective_tokens, 300_000);
        assert_eq!(threshold.rearm_floor_tokens, 281_400);
        assert!(!observe_and_decide(&key(), 299_999, threshold));
        assert!(observe_and_decide(&key(), 300_000, threshold));
        assert!(!observe_and_decide(&key(), 371_000, threshold));
        assert!(!observe_and_decide(&key(), 281_401, threshold));
        assert!(!observe_and_decide(&key(), 281_400, threshold));
        assert!(observe_and_decide(&key(), 300_000, threshold));
    }

    #[test]
    fn post_mutation_unknown_stays_disarmed_but_pre_mutation_refusal_rearms() {
        let _guard = state_test_guard();
        let threshold = compact_threshold(1_000_000, 50, 300_000).unwrap();
        assert!(observe_and_decide(&key(), 500_000, threshold));
        // Ambiguous-after-mutation does not call rearm; another high poll cannot
        // enqueue a duplicate compact.
        assert!(!observe_and_decide(&key(), 700_000, threshold));
        rearm_after_pre_mutation_refusal(&key());
        assert!(observe_and_decide(&key(), 700_000, threshold));
    }
}
