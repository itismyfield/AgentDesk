//! Exactly-once and phase-safe Claude turn interrupt policy.

use super::interrupt_policy::{
    ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON, ClaudeTurnInterruptDelivery,
    ProviderTurnInterruptOutcome, build_claude_interrupt_control_line,
    claude_turn_interrupt_delivery,
};
use super::process_table::{pane_foreground_is_provider_wrapper, write_line_to_wrapper_fifo};
use super::tmux_runtime_paths;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::tui_turn_state::TuiTurnState;
use std::sync::Arc;

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
        Some(TuiTurnState::Streaming | TuiTurnState::Unknown) => ClaudeTuiInterruptPhase::Ambiguous,
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

/// Cancel Claude's active turn while preserving its tmux session.
///
/// Delivery uses Escape for the interactive TUI or a stream-json interrupt
/// control request for the wrapper FIFO. A token-local CAS elects one owner
/// before runtime probing; interactive Escape additionally requires structured
/// streaming state and positive active-pane evidence.
pub(super) async fn interrupt_claude_turn_session_preserving(
    token: &Arc<CancelToken>,
    tmux_session: Option<String>,
    reason: &str,
) -> ProviderTurnInterruptOutcome {
    // #3169: an anonymous internal PreserveSession teardown
    // (`turn_bridge_cancelled`, no user `cancel_source`) must NOT cancel the
    // live claude turn — leave it running for the watcher to reconcile, exactly
    // as the prior SIGINT-suppression did, just without the session-kill risk.
    if reason == ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON {
        return ProviderTurnInterruptOutcome {
            tmux_session,
            sent_keys: false,
            fallback_sigint_pid: None,
            missing_tmux_session: false,
            sigint_target_missing: false,
        };
    }

    let Some(session_name) = tmux_session.clone() else {
        return ProviderTurnInterruptOutcome {
            tmux_session,
            sent_keys: false,
            fallback_sigint_pid: None,
            missing_tmux_session: true,
            sigint_target_missing: false,
        };
    };

    if !claim_claude_stop_delivery(token) {
        tracing::info!(
            "claude turn interrupt decision: provider=claude session={} generation={} reason={} mechanism=not_probed runtime_kind=not_probed structured_state=not_probed pane_ready=not_probed pane_active=not_probed pane_has_draft=not_probed phase=not_probed decision={}",
            session_name,
            token.claude_interrupt_generation(),
            reason,
            ClaudeStopDeliveryDecision::SkipDuplicate.as_str()
        );
        return ProviderTurnInterruptOutcome {
            tmux_session,
            sent_keys: false,
            fallback_sigint_pid: None,
            missing_tmux_session: false,
            sigint_target_missing: false,
        };
    }

    let session_for_probe = session_name.clone();
    let probe_result = tokio::task::spawn_blocking(move || {
        let is_wrapper = pane_foreground_is_provider_wrapper(&session_for_probe);
        let delivery = claude_turn_interrupt_delivery(is_wrapper);
        let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
            &session_for_probe,
        );
        let runtime_kind = binding
            .as_ref()
            .map(|binding| binding.runtime_kind)
            .or_else(|| {
                crate::services::tmux_common::resolve_tmux_runtime_kind_marker(&session_for_probe)
            });
        let (default_output_path, default_input_fifo_path) = tmux_runtime_paths(&session_for_probe);
        let wrapper_input_fifo_path = if matches!(
            delivery,
            ClaudeTurnInterruptDelivery::StreamJsonControlRequest
        ) {
            Some(
                binding
                    .as_ref()
                    .and_then(|binding| binding.input_fifo_path.clone())
                    .unwrap_or(default_input_fifo_path),
            )
        } else {
            None
        };
        let structured_state = if matches!(delivery, ClaudeTurnInterruptDelivery::TuiEscape) {
            binding.as_ref().and_then(|binding| {
                crate::services::tui_turn_state::runtime_binding_turn_state(
                    &ProviderKind::Claude,
                    binding,
                )
            })
        } else {
            let output_path = binding
                .as_ref()
                .map(|binding| binding.output_path.clone())
                .unwrap_or(default_output_path);
            Some(
                crate::services::tui_turn_state::observe_claude_jsonl_turn_state(
                    std::path::Path::new(&output_path),
                ),
            )
        };
        let pane = if matches!(delivery, ClaudeTurnInterruptDelivery::TuiEscape) {
            crate::services::platform::tmux::capture_pane(&session_for_probe, -160)
        } else {
            None
        };
        let pane_ready = pane.as_deref().is_some_and(
            crate::services::tmux_common::tmux_capture_indicates_claude_tui_ready_for_input,
        );
        let pane_active = pane.as_deref().is_some_and(
            crate::services::tmux_common::tmux_capture_indicates_claude_tui_actively_streaming,
        );
        let pane_has_draft = pane.as_deref().is_some_and(
            crate::services::tmux_common::tmux_capture_indicates_claude_tui_prompt_draft,
        );
        let phase = match delivery {
            ClaudeTurnInterruptDelivery::TuiEscape => classify_tui_interrupt_phase(
                structured_state,
                pane_ready || pane_has_draft,
                pane_active,
            ),
            ClaudeTurnInterruptDelivery::StreamJsonControlRequest => match structured_state {
                Some(TuiTurnState::Idle) => ClaudeTuiInterruptPhase::PromptReady,
                Some(TuiTurnState::UserSubmitted) => ClaudeTuiInterruptPhase::UserSubmitted,
                Some(TuiTurnState::Streaming) => ClaudeTuiInterruptPhase::ActiveGeneration,
                Some(TuiTurnState::Unknown) | None => ClaudeTuiInterruptPhase::Ambiguous,
            },
        };
        (
            delivery,
            runtime_kind,
            wrapper_input_fifo_path,
            structured_state,
            pane_ready,
            pane_active,
            pane_has_draft,
            phase,
        )
    })
    .await;

    let (
        delivery,
        runtime_kind,
        wrapper_input_fifo_path,
        structured_state,
        pane_ready,
        pane_active,
        pane_has_draft,
        phase,
    ) = match probe_result {
        Ok(probe) => probe,
        Err(error) => {
            tracing::warn!(
                "claude turn interrupt probe join error: session={} reason={} generation={} decision=skip_ambiguous error={}",
                session_name,
                reason,
                token.claude_interrupt_generation(),
                error
            );
            return ProviderTurnInterruptOutcome {
                tmux_session,
                sent_keys: false,
                fallback_sigint_pid: None,
                missing_tmux_session: false,
                sigint_target_missing: false,
            };
        }
    };

    let decision = decide_claimed_claude_stop_delivery(delivery, phase);
    tracing::info!(
        "claude turn interrupt decision: provider=claude session={} generation={} reason={} mechanism={:?} runtime_kind={} structured_state={} pane_ready={} pane_active={} pane_has_draft={} phase={} decision={}",
        session_name,
        token.claude_interrupt_generation(),
        reason,
        delivery,
        runtime_kind
            .map(crate::services::agent_protocol::RuntimeHandoffKind::as_str)
            .unwrap_or("unknown"),
        structured_state
            .map(|state| state.as_str())
            .unwrap_or("unavailable"),
        pane_ready,
        pane_active,
        pane_has_draft,
        phase.as_str(),
        decision.as_str()
    );

    let ClaudeStopDeliveryDecision::Deliver(delivery) = decision else {
        return ProviderTurnInterruptOutcome {
            tmux_session,
            sent_keys: false,
            fallback_sigint_pid: None,
            missing_tmux_session: false,
            sigint_target_missing: false,
        };
    };

    let session_for_task = session_name.clone();
    let request_id = format!("agentdesk-interrupt-{}", uuid::Uuid::new_v4());
    let delivery_result = tokio::task::spawn_blocking(move || match delivery {
        ClaudeTurnInterruptDelivery::TuiEscape => {
            match crate::services::platform::tmux::send_keys(&session_for_task, &["Escape"]) {
                Ok(output) if output.status.success() => Ok(()),
                Ok(output) => Err(format!(
                    "tmux send-keys Escape failed: status={}",
                    output.status
                )),
                Err(error) => Err(format!("tmux send-keys Escape error: {error}")),
            }
        }
        ClaudeTurnInterruptDelivery::StreamJsonControlRequest => {
            let Some(input_fifo) = wrapper_input_fifo_path else {
                return Err("claude wrapper input FIFO unavailable after probe".to_string());
            };
            let line = build_claude_interrupt_control_line(&request_id);
            write_line_to_wrapper_fifo(&input_fifo, &line)
        }
    })
    .await;

    let delivered = match delivery_result {
        Ok(Ok(())) => {
            tracing::info!(
                "claude turn interrupt delivered (session preserved): session={} generation={} reason={} mechanism={:?} phase={}",
                session_name,
                token.claude_interrupt_generation(),
                reason,
                delivery,
                phase.as_str()
            );
            true
        }
        Ok(Err(error)) => {
            // Deliberately NO SIGINT fallback: a failed turn-cancel must not
            // escalate to a session-kill. The cooperative cancel flag still
            // flips in `cancel_active_token`, and the watcher reconciles the
            // turn on its next pass.
            tracing::warn!(
                "claude turn interrupt delivery failed (session left intact, no SIGINT escalation): session={} generation={} reason={} mechanism={:?} phase={} error={}",
                session_name,
                token.claude_interrupt_generation(),
                reason,
                delivery,
                phase.as_str(),
                error
            );
            false
        }
        Err(error) => {
            tracing::warn!(
                "claude turn interrupt join error: session={} generation={} reason={} mechanism={:?} phase={} error={}",
                session_name,
                token.claude_interrupt_generation(),
                reason,
                delivery,
                phase.as_str(),
                error
            );
            false
        }
    };

    ProviderTurnInterruptOutcome {
        tmux_session,
        sent_keys: delivered,
        fallback_sigint_pid: None,
        missing_tmux_session: false,
        sigint_target_missing: false,
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
            let decision = claimed_decision(&token, ClaudeTurnInterruptDelivery::TuiEscape, phase);
            assert!(matches!(
                phase,
                ClaudeTuiInterruptPhase::PromptReady | ClaudeTuiInterruptPhase::UserSubmitted
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
                    ClaudeStopDeliveryDecision::Deliver(ClaudeTurnInterruptDelivery::TuiEscape)
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
                claimed_decision(&token, ClaudeTurnInterruptDelivery::TuiEscape, phase,),
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
