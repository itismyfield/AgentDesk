//! Exactly-once and phase-safe Claude turn interrupt policy.

use super::interrupt_policy::ClaudeTurnInterruptDelivery;
use crate::services::provider::CancelToken;
use crate::services::tui_turn_state::TuiTurnState;

/// Claude state observed at the single stop-delivery ownership boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ClaudeTuiInterruptPhase {
    PromptReady,
    UserSubmitted,
    ActiveGeneration,
    Ambiguous,
}

impl ClaudeTuiInterruptPhase {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::PromptReady => "prompt_ready",
            Self::UserSubmitted => "user_submitted",
            Self::ActiveGeneration => "active_generation",
            Self::Ambiguous => "ambiguous",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ClaudeStopDeliveryDecision {
    Deliver(ClaudeTurnInterruptDelivery),
    SkipDuplicate,
    SkipPreGeneration,
    SkipAmbiguous,
}

impl ClaudeStopDeliveryDecision {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Deliver(_) => "deliver",
            Self::SkipDuplicate => "skip_duplicate",
            Self::SkipPreGeneration => "skip_pre_generation",
            Self::SkipAmbiguous => "skip_ambiguous",
        }
    }
}

pub(super) fn classify_tui_interrupt_phase(
    structured_state: Option<TuiTurnState>,
    pane_ready: bool,
    pane_active: bool,
) -> ClaudeTuiInterruptPhase {
    match structured_state {
        Some(TuiTurnState::Idle) => ClaudeTuiInterruptPhase::PromptReady,
        Some(TuiTurnState::UserSubmitted) => ClaudeTuiInterruptPhase::UserSubmitted,
        Some(TuiTurnState::Streaming) if pane_active => ClaudeTuiInterruptPhase::ActiveGeneration,
        Some(TuiTurnState::Streaming | TuiTurnState::Unknown) => {
            ClaudeTuiInterruptPhase::Ambiguous
        }
        None if pane_ready => ClaudeTuiInterruptPhase::PromptReady,
        None => ClaudeTuiInterruptPhase::Ambiguous,
    }
}

/// Claim the turn generation before any runtime probe so only one stop path can
/// observe state or mutate the provider session.
pub(super) fn claim_claude_stop_delivery(token: &CancelToken) -> bool {
    token.claim_claude_interrupt()
}

/// Decide delivery after the caller has claimed the token-local ownership fence.
pub(super) fn decide_claimed_claude_stop_delivery(
    delivery: ClaudeTurnInterruptDelivery,
    tui_phase: ClaudeTuiInterruptPhase,
) -> ClaudeStopDeliveryDecision {
    let phase_allows_delivery = match delivery {
        ClaudeTurnInterruptDelivery::TuiEscape => {
            matches!(tui_phase, ClaudeTuiInterruptPhase::ActiveGeneration)
        }
        ClaudeTurnInterruptDelivery::StreamJsonControlRequest => !matches!(
            tui_phase,
            ClaudeTuiInterruptPhase::PromptReady | ClaudeTuiInterruptPhase::Ambiguous
        ),
    };

    if phase_allows_delivery {
        ClaudeStopDeliveryDecision::Deliver(delivery)
    } else if matches!(tui_phase, ClaudeTuiInterruptPhase::Ambiguous) {
        ClaudeStopDeliveryDecision::SkipAmbiguous
    } else {
        ClaudeStopDeliveryDecision::SkipPreGeneration
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn claimed_decision(
        token: &CancelToken,
        delivery: ClaudeTurnInterruptDelivery,
        phase: ClaudeTuiInterruptPhase,
    ) -> ClaudeStopDeliveryDecision {
        if claim_claude_stop_delivery(token) {
            decide_claimed_claude_stop_delivery(delivery, phase)
        } else {
            ClaudeStopDeliveryDecision::SkipDuplicate
        }
    }

    fn decision_mutates_composer(decision: ClaudeStopDeliveryDecision) -> bool {
        matches!(
            decision,
            ClaudeStopDeliveryDecision::Deliver(ClaudeTurnInterruptDelivery::TuiEscape)
        )
    }

    #[test]
    fn prompt_ready_and_just_injected_never_escape_or_mutate_composer() {
        for phase in [
            classify_tui_interrupt_phase(Some(TuiTurnState::Idle), true, false),
            classify_tui_interrupt_phase(Some(TuiTurnState::UserSubmitted), false, false),
        ] {
            let token = CancelToken::new();
            let decision = claimed_decision(
                &token,
                ClaudeTurnInterruptDelivery::TuiEscape,
                phase,
            );
            assert!(matches!(
                phase,
                ClaudeTuiInterruptPhase::PromptReady
                    | ClaudeTuiInterruptPhase::UserSubmitted
            ));
            assert_eq!(decision, ClaudeStopDeliveryDecision::SkipPreGeneration);
            assert!(!decision_mutates_composer(decision));
            assert_eq!(
                claimed_decision(
                    &token,
                    ClaudeTurnInterruptDelivery::TuiEscape,
                    ClaudeTuiInterruptPhase::ActiveGeneration,
                ),
                ClaudeStopDeliveryDecision::SkipDuplicate,
                "the pre-generation owner must fence a later observer"
            );
        }
    }

    #[test]
    fn active_generation_escape_is_delivered_once_across_stop_race() {
        let token = Arc::new(CancelToken::new());
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();

        for reason in ["/stop", "mailbox_cancel_active_turn"] {
            let token = Arc::clone(&token);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                (
                    reason,
                    claimed_decision(
                        token.as_ref(),
                        ClaudeTurnInterruptDelivery::TuiEscape,
                        ClaudeTuiInterruptPhase::ActiveGeneration,
                    ),
                )
            }));
        }
        barrier.wait();

        let decisions: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("stop racer must not panic").1)
            .collect();
        let delivery_count = decisions
            .iter()
            .filter(|decision| {
                matches!(
                    decision,
                    ClaudeStopDeliveryDecision::Deliver(
                        ClaudeTurnInterruptDelivery::TuiEscape
                    )
                )
            })
            .count();
        let duplicate_count = decisions
            .iter()
            .filter(|decision| matches!(decision, ClaudeStopDeliveryDecision::SkipDuplicate))
            .count();
        assert_eq!(delivery_count, 1);
        assert_eq!(duplicate_count, 1);
    }

    #[test]
    fn a_new_turn_generation_gets_a_fresh_delivery_fence() {
        let first = CancelToken::new();
        let next = CancelToken::new();
        assert_ne!(
            first.claude_interrupt_generation(),
            next.claude_interrupt_generation()
        );

        for token in [&first, &next] {
            assert!(matches!(
                claimed_decision(
                    token,
                    ClaudeTurnInterruptDelivery::TuiEscape,
                    ClaudeTuiInterruptPhase::ActiveGeneration,
                ),
                ClaudeStopDeliveryDecision::Deliver(_)
            ));
        }
    }

    #[test]
    fn ambiguous_or_unconfirmed_streaming_state_fails_safe_and_fences_retry() {
        for phase in [
            classify_tui_interrupt_phase(Some(TuiTurnState::Streaming), false, false),
            classify_tui_interrupt_phase(Some(TuiTurnState::Unknown), false, true),
            classify_tui_interrupt_phase(None, false, true),
        ] {
            let token = CancelToken::new();
            assert_eq!(phase, ClaudeTuiInterruptPhase::Ambiguous);
            assert_eq!(
                claimed_decision(
                    &token,
                    ClaudeTurnInterruptDelivery::TuiEscape,
                    phase,
                ),
                ClaudeStopDeliveryDecision::SkipAmbiguous
            );
            assert_eq!(
                claimed_decision(
                    &token,
                    ClaudeTurnInterruptDelivery::TuiEscape,
                    ClaudeTuiInterruptPhase::ActiveGeneration,
                ),
                ClaudeStopDeliveryDecision::SkipDuplicate,
                "an ambiguous owner must fence a later observer"
            );
        }
    }

    #[test]
    fn wrapper_interrupt_is_fenced_but_can_cancel_submitted_generation() {
        let token = CancelToken::new();
        assert!(matches!(
            claimed_decision(
                &token,
                ClaudeTurnInterruptDelivery::StreamJsonControlRequest,
                ClaudeTuiInterruptPhase::UserSubmitted,
            ),
            ClaudeStopDeliveryDecision::Deliver(_)
        ));
        assert_eq!(
            claimed_decision(
                &token,
                ClaudeTurnInterruptDelivery::StreamJsonControlRequest,
                ClaudeTuiInterruptPhase::UserSubmitted,
            ),
            ClaudeStopDeliveryDecision::SkipDuplicate
        );
    }
}
