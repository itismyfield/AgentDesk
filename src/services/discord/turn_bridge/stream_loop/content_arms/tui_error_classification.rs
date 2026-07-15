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
        bridge_claude_tui_followup_requeue_prompt_error,
        bridge_tui_transport_error_should_skip_quiescence,
        claude_tui_followup_requeue_streaming_aware,
    };

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
        assert!(bridge_tui_transport_error_should_skip_quiescence(
            &provider,
            Some(RuntimeHandoffKind::ClaudeTui),
            &full_response,
            resolution.tui_error_classification,
        ));
    }
}
