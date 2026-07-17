//! Exact-token Claude `/compact` triggering and busy-turn steering.
//!
//! The trigger never waits on a pane becoming idle. It owns an independent
//! per-session latch and hands a compact-specific submit to the narrow composer
//! mutation lock; normal TUI follow-ups retain their turn-lifetime lock.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::services::claude_compact_context::{CompactThreshold, compact_threshold};
use crate::services::claude_tui::input::CompactSubmitOutcome;
use crate::services::provider::ProviderKind;

/// A submitted control must be observed promptly. Keeping this small prevents a
/// dropped queued `/compact` from swallowing an unrelated human command much
/// later while still allowing normal hook/transcript scheduling jitter.
const MACHINE_CONTROL_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CompactLatchKey {
    channel_id: u64,
    tmux_session_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct MachineControlKey {
    provider: String,
    tmux_session_name: String,
    provider_session_id: String,
}

#[derive(Clone, Debug)]
struct MachineControlMarker {
    nonce: u64,
    fence: DateTime<Utc>,
    recorded_at: Instant,
    // Set immediately after tmux accepts the submitting Enter. This is
    // deliberately provisional: a later passive capture can still be
    // ambiguous, in which case the ticket is cleared and never retried.
    enter_submitted: bool,
}

#[derive(Clone, Debug)]
struct MachineControlTicket {
    key: MachineControlKey,
    nonce: u64,
}

static ARMED_BY_SESSION: LazyLock<Mutex<HashMap<CompactLatchKey, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static MACHINE_CONTROLS: LazyLock<Mutex<HashMap<MachineControlKey, MachineControlMarker>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static MACHINE_CONTROL_NONCE: AtomicU64 = AtomicU64::new(1);

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
        let Some(provider_session_id) =
            crate::services::tui_prompt_dedupe::provider_session_for_tmux(
                "claude",
                &key.tmux_session_name,
            )
        else {
            rearm_after_pre_mutation_refusal(&key);
            tracing::debug!(tmux_session_name = %key.tmux_session_name, "Claude auto compact refused: provider session identity is unavailable");
            return;
        };
        let ticket =
            begin_machine_compact_control("claude", &key.tmux_session_name, &provider_session_id);
        let outcome = crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
            &key.tmux_session_name,
            || {
                crate::services::claude_tui::input::send_compact_while_busy_after_enter(
                    &key.tmux_session_name,
                    || commit_machine_compact_control(&ticket),
                )
            },
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
                clear_machine_compact_control(&ticket);
                rearm_after_pre_mutation_refusal(&key);
                tracing::debug!(
                    tmux_session_name = %key.tmux_session_name,
                    "Claude auto compact refused before mutation; latch re-armed"
                );
            }
            CompactSubmitOutcome::AmbiguousAfterMutation => {
                // Never re-arm here. Retrying could enqueue a second compact;
                // clearing the marker makes a later human `/compact` unambiguous.
                clear_machine_compact_control(&ticket);
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

fn begin_machine_compact_control(
    provider: &str,
    tmux_session_name: &str,
    provider_session_id: &str,
) -> MachineControlTicket {
    let key = MachineControlKey {
        provider: provider.trim().to_ascii_lowercase(),
        tmux_session_name: tmux_session_name.trim().to_string(),
        provider_session_id: provider_session_id.trim().to_string(),
    };
    let nonce = MACHINE_CONTROL_NONCE.fetch_add(1, Ordering::Relaxed);
    MACHINE_CONTROLS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(
            key.clone(),
            MachineControlMarker {
                nonce,
                // The fence is stamped before the first tmux key call. A
                // historical transcript replay can never consume this marker.
                fence: Utc::now(),
                recorded_at: Instant::now(),
                enter_submitted: false,
            },
        );
    MachineControlTicket { key, nonce }
}

fn commit_machine_compact_control(ticket: &MachineControlTicket) {
    let mut controls = MACHINE_CONTROLS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    purge_expired_machine_controls(&mut controls);
    if let Some(marker) = controls.get_mut(&ticket.key)
        && marker.nonce == ticket.nonce
    {
        marker.enter_submitted = true;
    }
}

fn clear_machine_compact_control(ticket: &MachineControlTicket) {
    let mut controls = MACHINE_CONTROLS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if controls
        .get(&ticket.key)
        .is_some_and(|marker| marker.nonce == ticket.nonce)
    {
        controls.remove(&ticket.key);
    }
}

/// Consume exactly one machine `/compact` transcript observation whose
/// submitting Enter was accepted by tmux. The raw slash classifier remains the
/// fallback relay guard; this narrow, provisional marker prevents only the
/// machine turn from gaining a second control lifecycle while preserving later
/// human commands after ambiguous sends.
pub(crate) fn consume_enter_submitted_machine_compact(
    provider: &str,
    tmux_session_name: &str,
    provider_session_id: &str,
    prompt: &str,
    observed_at: DateTime<Utc>,
) -> bool {
    if !is_exact_compact(prompt) {
        return false;
    }
    let key = MachineControlKey {
        provider: provider.trim().to_ascii_lowercase(),
        tmux_session_name: tmux_session_name.trim().to_string(),
        provider_session_id: provider_session_id.trim().to_string(),
    };
    let mut controls = MACHINE_CONTROLS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    purge_expired_machine_controls(&mut controls);
    let Some(marker) = controls.get(&key) else {
        return false;
    };
    let upper_fence = marker.fence
        + chrono::Duration::from_std(MACHINE_CONTROL_TTL)
            .expect("machine-control TTL fits chrono duration");
    let expired_for_observation = observed_at > upper_fence;
    let consumed =
        marker.enter_submitted && observed_at >= marker.fence && !expired_for_observation;
    if consumed || expired_for_observation {
        controls.remove(&key);
    }
    consumed
}

fn is_exact_compact(prompt: &str) -> bool {
    prompt.trim() == "/compact"
}

/// New provider session registration means an old command cannot belong to the
/// newly launched pane, so clear every pending/confirmed marker for that tmux
/// session before future observations are relayed.
pub(crate) fn clear_machine_compact_controls_for_tmux(tmux_session_name: &str) {
    let tmux_session_name = tmux_session_name.trim();
    let mut controls = MACHINE_CONTROLS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    controls.retain(|key, _| key.tmux_session_name != tmux_session_name);
}

fn purge_expired_machine_controls(controls: &mut HashMap<MachineControlKey, MachineControlMarker>) {
    controls.retain(|_, marker| marker.recorded_at.elapsed() <= MACHINE_CONTROL_TTL);
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    ARMED_BY_SESSION
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
    MACHINE_CONTROLS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
}

/// The compact latch and machine-control markers are process-global test state.
/// Keep every stateful test serialized so one test cannot clear another test's
/// in-flight fixture.
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

/// Test-only fixture setup for a hook observation that races the passive tmux
/// settle after Enter. Sibling relay tests hold [`STATE_TEST_LOCK`] while using
/// this so they cannot collide with compact-trigger unit fixtures.
#[cfg(test)]
pub(crate) fn record_enter_submitted_machine_compact_for_test(
    provider: &str,
    tmux_session_name: &str,
    provider_session_id: &str,
) {
    let ticket = begin_machine_compact_control(provider, tmux_session_name, provider_session_id);
    commit_machine_compact_control(&ticket);
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

    #[test]
    fn post_enter_marker_consumes_before_passive_settle_finishes() {
        let _guard = state_test_guard();
        let ticket = begin_machine_compact_control("claude", "tmux-4591", "session-a");
        let before_fence = Utc::now() - chrono::Duration::seconds(1);
        assert!(!consume_enter_submitted_machine_compact(
            "claude",
            "tmux-4591",
            "session-a",
            "/compact",
            before_fence
        ));
        // `send_compact_while_busy_after_enter` commits at this boundary, before
        // its passive 120ms settle. A hook observed during that settle must be
        // consumable immediately rather than creating an external turn.
        commit_machine_compact_control(&ticket);
        assert!(consume_enter_submitted_machine_compact(
            "claude",
            "tmux-4591",
            "session-a",
            "/compact",
            Utc::now()
        ));
        assert!(!consume_enter_submitted_machine_compact(
            "claude",
            "tmux-4591",
            "session-a",
            "/compact",
            Utc::now()
        ));
    }

    #[test]
    fn ambiguous_marker_clear_does_not_swallow_later_human_compact() {
        let _guard = state_test_guard();
        let ticket = begin_machine_compact_control("claude", "tmux-4591", "session-a");
        // Model Enter having succeeded (and thus the provisional marker being
        // visible) before later passive confirmation becomes ambiguous.
        commit_machine_compact_control(&ticket);
        clear_machine_compact_control(&ticket);
        assert!(!consume_enter_submitted_machine_compact(
            "claude",
            "tmux-4591",
            "session-a",
            "/compact",
            Utc::now()
        ));
    }

    #[test]
    fn expired_marker_does_not_swallow_later_human_compact() {
        let _guard = state_test_guard();
        let ticket = begin_machine_compact_control("claude", "tmux-4591", "session-a");
        commit_machine_compact_control(&ticket);

        assert!(
            !consume_enter_submitted_machine_compact(
                "claude",
                "tmux-4591",
                "session-a",
                "/compact",
                Utc::now() + chrono::Duration::minutes(11),
            ),
            "a compact observation beyond the upper fence belongs to a later human command"
        );
        assert!(
            !consume_enter_submitted_machine_compact(
                "claude",
                "tmux-4591",
                "session-a",
                "/compact",
                Utc::now(),
            ),
            "the expired marker must be removed rather than waiting to swallow a later command"
        );
    }
}
