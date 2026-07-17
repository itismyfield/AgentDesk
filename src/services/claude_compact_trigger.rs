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

const MACHINE_CONTROL_TTL: Duration = Duration::from_secs(4 * 60 * 60);

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
    passively_confirmed: bool,
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
        let outcome = crate::services::claude::with_claude_tui_composer_mutation_lock(
            &key.tmux_session_name,
            || crate::services::claude_tui::input::send_compact_while_busy(&key.tmux_session_name),
        );
        match outcome {
            CompactSubmitOutcome::AcceptedOrQueued => {
                commit_machine_compact_control(&ticket);
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
                passively_confirmed: false,
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
        marker.passively_confirmed = true;
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

/// Consume exactly one confirmed machine `/compact` transcript observation.
/// The raw slash classifier remains the fallback relay guard; this narrower
/// marker prevents only the accepted/queued machine turn from gaining a second
/// control lifecycle while preserving later human commands after failures.
pub(crate) fn consume_passively_confirmed_machine_compact(
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
    let consumed = controls
        .get(&key)
        .is_some_and(|marker| marker.passively_confirmed && observed_at >= marker.fence);
    if consumed {
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
        reset_for_test();
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
        reset_for_test();
        let threshold = compact_threshold(1_000_000, 50, 300_000).unwrap();
        assert!(observe_and_decide(&key(), 500_000, threshold));
        // Ambiguous-after-mutation does not call rearm; another high poll cannot
        // enqueue a duplicate compact.
        assert!(!observe_and_decide(&key(), 700_000, threshold));
        rearm_after_pre_mutation_refusal(&key());
        assert!(observe_and_decide(&key(), 700_000, threshold));
    }

    #[test]
    fn confirmed_marker_consumes_one_fence_bound_observation_only() {
        reset_for_test();
        let ticket = begin_machine_compact_control("claude", "tmux-4591", "session-a");
        let before_fence = Utc::now() - chrono::Duration::seconds(1);
        assert!(!consume_passively_confirmed_machine_compact(
            "claude",
            "tmux-4591",
            "session-a",
            "/compact",
            before_fence
        ));
        commit_machine_compact_control(&ticket);
        assert!(consume_passively_confirmed_machine_compact(
            "claude",
            "tmux-4591",
            "session-a",
            "/compact",
            Utc::now()
        ));
        assert!(!consume_passively_confirmed_machine_compact(
            "claude",
            "tmux-4591",
            "session-a",
            "/compact",
            Utc::now()
        ));
    }

    #[test]
    fn ambiguous_marker_clear_does_not_swallow_later_human_compact() {
        reset_for_test();
        let ticket = begin_machine_compact_control("claude", "tmux-4591", "session-a");
        clear_machine_compact_control(&ticket);
        assert!(!consume_passively_confirmed_machine_compact(
            "claude",
            "tmux-4591",
            "session-a",
            "/compact",
            Utc::now()
        ));
    }
}
