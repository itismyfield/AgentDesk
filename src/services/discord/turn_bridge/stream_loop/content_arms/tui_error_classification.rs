//! Raw TUI error classification paired with provider error presentation.

use super::super::super::streaming_edit_text::{TuiErrorClassification, classify_raw_tui_error};
use super::provider_error_presentation::{ProviderErrorPresentation, provider_error_presentation};
use crate::services::provider::ProviderKind;

pub(super) struct ProviderErrorArmResolution {
    pub(super) presentation: ProviderErrorPresentation,
    pub(super) tui_error_classification: TuiErrorClassification,
}

pub(super) fn resolve_tui_error(
    provider: &ProviderKind,
    message: &str,
    stderr: &str,
) -> ProviderErrorArmResolution {
    ProviderErrorArmResolution {
        presentation: provider_error_presentation(message, stderr),
        // Classify the raw message before presentation folds `Error:` into a
        // spoiler. Finalization must not infer lifecycle behavior from UI text.
        tui_error_classification: classify_raw_tui_error(provider, message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::discord::turn_bridge::streaming_edit_text::{
        bridge_claude_tui_followup_busy_readiness_timeout,
        bridge_claude_tui_followup_requeue_prompt_error,
        bridge_tui_transport_error_should_skip_quiescence,
        claude_tui_followup_requeue_streaming_aware,
    };

    fn assert_wiring_joint(source: &str, expected: &str, joint: &str) {
        let compact_source = source.split_whitespace().collect::<String>();
        let compact_expected = expected.split_whitespace().collect::<String>();
        assert!(
            compact_source.contains(&compact_expected),
            "{joint} must preserve the Error-arm TUI classification",
        );
    }

    #[test]
    fn folded_readiness_timeout_from_error_arm_is_requeued_and_skips_quiescence() {
        let provider = ProviderKind::Claude;
        let resolution = resolve_tui_error(
            &provider,
            concat!(
                "timeout waiting for claude tui follow-up prompt input readiness after 45s; ",
                "reason=prompt_marker_not_detected; previous_tui_turn_still_running=true; ",
                "prompt_marker_detected=false; prompt_draft_detected=false; ",
                "capture_available=true",
            ),
            "",
        );
        let ProviderErrorPresentation::Failure(full_response) = resolution.presentation else {
            panic!("readiness timeout must remain an ordinary provider failure");
        };

        // Exercise the actual Error-arm producer output: presentation is folded
        // and deliberately no longer starts with the legacy lifecycle marker.
        let folded_prefix = concat!("⚠️ provider가 응답을 완료하지 ", "못했어요.",);
        assert!(full_response.starts_with(folded_prefix));
        assert!(!full_response.trim_start().starts_with("Error:"));

        let requeue_candidate = bridge_claude_tui_followup_requeue_prompt_error(
            &provider,
            Some(RuntimeHandoffKind::ClaudeTui),
            &full_response,
            resolution.tui_error_classification,
        );
        assert!(claude_tui_followup_requeue_streaming_aware(
            requeue_candidate,
            false,
        ));
        assert!(bridge_claude_tui_followup_busy_readiness_timeout(
            &provider,
            Some(RuntimeHandoffKind::ClaudeTui),
            resolution.tui_error_classification,
        ));
        assert!(
            !bridge_claude_tui_followup_busy_readiness_timeout(
                &provider,
                Some(RuntimeHandoffKind::CodexTui),
                resolution.tui_error_classification,
            ),
            "only a hosted Claude TUI follow-up may bypass the resume heuristic"
        );
        assert!(
            !bridge_claude_tui_followup_busy_readiness_timeout(
                &provider,
                Some(RuntimeHandoffKind::ClaudeTui),
                resolve_tui_error(
                    &provider,
                    "timeout waiting for claude tui follow-up prompt input readiness after 45s; reason=prompt_marker_not_detected; previous_tui_turn_still_running=false",
                    "",
                )
                .tui_error_classification,
            ),
            "a timeout without an active previous turn must not suppress native resume recovery"
        );
        assert!(bridge_tui_transport_error_should_skip_quiescence(
            &provider,
            Some(RuntimeHandoffKind::ClaudeTui),
            &full_response,
            resolution.tui_error_classification,
        ));

        // Mutation guards for the production wiring. The async finalization
        // contexts require live gateway/shared state, so pin each handoff at
        // its construction site while the assertions above prove the carried
        // value drives both lifecycle decisions.
        assert_wiring_joint(
            include_str!("../content_arms.rs"),
            r#"
                let error_resolution = resolve_tui_error(&provider, &message, &stderr);
                tui_error_classification = error_resolution.tui_error_classification;
                transport_error = true;
            "#,
            "Error arm -> stream-loop state",
        );
        assert_wiring_joint(
            include_str!("../../mod.rs"),
            r#"
                transport_error,
                tui_error_classification: stream_loop_output.tui_error_classification,
                recovery_retry,
            "#,
            "StreamLoopOutput -> PostLoopFinalizeContext",
        );
        assert_wiring_joint(
            include_str!("../../post_loop_finalize.rs"),
            r#"
                claude_tui_followup_pre_submit_requeue_candidate,
                tui_error_classification,
                review_dispatch_warning,
            "#,
            "post-loop local -> PostLoopFinalizeOutput",
        );
        assert_wiring_joint(
            include_str!("../../terminal_outcome_delivery.rs"),
            r#"
                claude_tui_followup_pre_submit_requeue_candidate,
                claude_tui_busy_requeue_pending,
                tui_error_classification,
                #[cfg(unix)]
                bridge_tui_gate_outcome_early,
            "#,
            "terminal outcome -> DeliveryEpilogueContext",
        );
    }

    /// #4640 regression: a follow-up readiness timeout where the pane is idle at
    /// a ready prompt (prompt_marker_detected=true, hence
    /// previous_tui_turn_still_running=false) proves the resumed provider session
    /// is alive — only the follow-up input failed to confirm in time. It must be
    /// classified as a session-preserving readiness timeout so the empty-response
    /// no-handshake fallback does NOT kill the live tmux session for a fresh
    /// retry. Before #4640, only previous_tui_turn_still_running=true (marker
    /// absent) was preserved, so this restart follow-up case leaked to
    /// fresh-session and dropped the resumed session.
    #[test]
    fn readiness_timeout_idle_at_prompt_marker_preserves_live_session() {
        let provider = ProviderKind::Claude;
        let resolution = resolve_tui_error(
            &provider,
            concat!(
                "timeout waiting for claude tui follow-up prompt input readiness after 45s; ",
                "reason=prompt_marker_unconfirmed; previous_tui_turn_still_running=false; ",
                "prompt_marker_detected=true; prompt_draft_detected=false; ",
                "capture_available=true",
            ),
            "",
        );
        assert!(
            bridge_claude_tui_followup_busy_readiness_timeout(
                &provider,
                Some(RuntimeHandoffKind::ClaudeTui),
                resolution.tui_error_classification,
            ),
            "an idle-at-prompt readiness timeout (marker detected) proves the resumed \
             session is alive and must be preserved from the fresh-session fallback"
        );
        // The follow-up input never confirmed, so it is still requeued like any
        // other readiness timeout — preservation only blocks the destructive
        // fresh-session retry, it does not swallow the pending follow-up.
        let ProviderErrorPresentation::Failure(full_response) = resolution.presentation else {
            panic!("readiness timeout must remain an ordinary provider failure");
        };
        assert!(bridge_claude_tui_followup_requeue_prompt_error(
            &provider,
            Some(RuntimeHandoffKind::ClaudeTui),
            &full_response,
            resolution.tui_error_classification,
        ));
    }
}
