//! Pure streaming-edit text + pre-submission TUI prompt-error classifiers for
//! the turn bridge.
//!
//! #3479 Phase-1 rank-4 extraction: byte-identical value-in/value-out helpers
//! the relay streaming path and quiescence-gate consult — the status-panel vs
//! legacy streaming-edit body composer, and the pre-submission / transport
//! TUI prompt-error predicates that decide whether to re-queue a follow-up or
//! skip the quiescence gate. None touch `shared`/`http`/async IO; each is
//! unit-tested. Moved verbatim from `turn_bridge/mod.rs` and re-exported there
//! so call sites stay identical.

use super::*;

pub(in crate::services::discord) fn build_turn_bridge_streaming_edit_text(
    status_panel_v2_enabled: bool,
    current_portion: &str,
    status_block: &str,
    provider: &ProviderKind,
) -> String {
    if status_panel_v2_enabled {
        super::formatting::build_status_panel_streaming_edit_text(
            current_portion,
            status_block,
            provider,
        )
    } else {
        super::formatting::build_streaming_placeholder_text(current_portion, status_block)
    }
}

pub(in crate::services::discord) fn bridge_pre_submission_tui_prompt_error(
    provider: &ProviderKind,
    full_response: &str,
) -> bool {
    let Some(error_text) = full_response
        .trim_start()
        .strip_prefix("Error:")
        .map(str::trim_start)
    else {
        return false;
    };
    match provider {
        ProviderKind::Claude => {
            crate::services::claude_tui::input::is_prompt_ready_timeout_error(error_text)
        }
        ProviderKind::Codex => {
            crate::services::codex_tui::input::is_prompt_ready_timeout_error(error_text)
        }
        _ => false,
    }
}

pub(in crate::services::discord) fn bridge_tui_transport_error_should_skip_quiescence(
    provider: &ProviderKind,
    runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    full_response: &str,
) -> bool {
    let Some(error_text) = full_response
        .trim_start()
        .strip_prefix("Error:")
        .map(str::trim_start)
    else {
        return false;
    };

    match (provider, runtime_kind) {
        (
            ProviderKind::Claude,
            Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui),
        ) => {
            bridge_pre_submission_tui_prompt_error(provider, full_response)
                || error_text == "Timeout waiting for output file"
                || error_text.starts_with("timeout waiting for claude tui transcript file")
                || error_text.contains("claude tui session died")
        }
        (
            ProviderKind::Codex,
            Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui),
        ) => {
            bridge_pre_submission_tui_prompt_error(provider, full_response)
                || error_text == "Timeout waiting for output file"
                || error_text.contains("codex tui session died")
        }
        _ => false,
    }
}

#[cfg(test)]
mod streaming_edit_text_tests {
    use super::*;

    #[test]
    fn status_panel_v2_streaming_edit_moves_processing_footer_to_response_message() {
        let rendered = build_turn_bridge_streaming_edit_text(
            true,
            "E2E-CODEX-1-OK\n- Working on the backend now",
            "⠙ 계속 처리 중",
            &ProviderKind::Codex,
        );

        assert_eq!(
            rendered,
            "E2E-CODEX-1-OK\n- Working on the backend now\n\n⠙ 계속 처리 중"
        );
    }

    #[test]
    fn legacy_streaming_edit_keeps_processing_footer() {
        let rendered = build_turn_bridge_streaming_edit_text(
            false,
            "Partial answer",
            "⠙ 계속 처리 중",
            &ProviderKind::Codex,
        );

        assert_eq!(rendered, "Partial answer\n\n⠙ 계속 처리 중");
    }

    #[test]
    fn status_panel_v2_empty_streaming_edit_keeps_placeholder() {
        let rendered =
            build_turn_bridge_streaming_edit_text(true, "", "⠙ 계속 처리 중", &ProviderKind::Codex);

        assert_eq!(rendered, "⠙ 계속 처리 중");
    }
}

#[cfg(test)]
mod pre_submission_tui_prompt_error_tests {
    use super::*;

    #[test]
    fn classifier_matches_wrapped_readiness_errors() {
        assert!(bridge_pre_submission_tui_prompt_error(
            &ProviderKind::Claude,
            "Error: timeout waiting for claude tui follow-up prompt input readiness after 45s; reason=prompt_marker_not_detected; previous_tui_turn_still_running=true; capture_available=true",
        ));
        assert!(bridge_pre_submission_tui_prompt_error(
            &ProviderKind::Codex,
            "Error: timeout waiting for codex tui follow-up prompt input readiness after 45s; reason=composer_not_detected; previous_tui_turn_still_running=true; capture_available=true",
        ));
        assert!(!bridge_pre_submission_tui_prompt_error(
            &ProviderKind::Claude,
            "Error: claude tui session died during follow-up output reading",
        ));
        assert!(!bridge_pre_submission_tui_prompt_error(
            &ProviderKind::Claude,
            "timeout waiting for claude tui follow-up prompt input readiness after 45s",
        ));
    }

    #[test]
    fn tui_transport_errors_skip_quiescence_only_for_matching_tui_runtime() {
        use crate::services::agent_protocol::RuntimeHandoffKind;

        assert!(bridge_tui_transport_error_should_skip_quiescence(
            &ProviderKind::Claude,
            Some(RuntimeHandoffKind::ClaudeTui),
            "Error: Timeout waiting for output file",
        ));
        assert!(bridge_tui_transport_error_should_skip_quiescence(
            &ProviderKind::Claude,
            Some(RuntimeHandoffKind::ClaudeTui),
            "Error: timeout waiting for claude tui transcript file after 120s; capture_available=true; prompt_marker_detected=true; prompt_draft_detected=false",
        ));
        assert!(bridge_tui_transport_error_should_skip_quiescence(
            &ProviderKind::Codex,
            Some(RuntimeHandoffKind::CodexTui),
            "Error: timeout waiting for codex tui follow-up prompt input readiness after 45s; reason=composer_not_detected; previous_tui_turn_still_running=true; capture_available=true",
        ));
        assert!(!bridge_tui_transport_error_should_skip_quiescence(
            &ProviderKind::Claude,
            Some(RuntimeHandoffKind::LegacyTmuxWrapper),
            "Error: Timeout waiting for output file",
        ));
        assert!(!bridge_tui_transport_error_should_skip_quiescence(
            &ProviderKind::Claude,
            Some(RuntimeHandoffKind::ClaudeTui),
            "Error: upstream API returned 500",
        ));
    }
}
