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
        build_provider_streaming_placeholder_text(current_portion, status_block, provider)
    }
}

pub(in crate::services::discord) fn bridge_streaming_rollover_should_skip(
    current_portion: &str,
) -> bool {
    streaming_subagent_notification_card(current_portion).is_some()
}

fn streaming_subagent_notification_card(current_portion: &str) -> Option<String> {
    let direct =
        crate::services::discord::response_sanitizer::subagent_notification_card::sanitize_start_anchored_subagent_notification(
            current_portion,
        );
    if direct.is_some() {
        return direct;
    }
    let stripped = crate::services::discord::response_sanitizer::strip_leading_tui_response_chrome(
        current_portion,
    );
    if stripped == current_portion {
        return None;
    }
    crate::services::discord::response_sanitizer::subagent_notification_card::sanitize_start_anchored_subagent_notification(
        &stripped,
    )
}

fn build_provider_streaming_placeholder_text(
    current_portion: &str,
    status_block: &str,
    provider: &ProviderKind,
) -> String {
    if current_portion.is_empty() {
        return super::formatting::build_streaming_placeholder_text("", status_block);
    }
    let formatted =
        super::formatting::format_for_discord_with_status_panel(current_portion, provider);
    super::formatting::build_streaming_placeholder_text(&formatted, status_block)
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

pub(in crate::services::discord) const CLAUDE_TUI_FOLLOWUP_REQUEUE_DELIVERY_NOTICE: &str = "📬 Claude TUI가 아직 이전 터미널 턴을 처리 중이라 이 메시지를 바로 주입하지 못했습니다. 현재 응답이 끝나면 자동으로 다시 제출되도록 재시도 큐에 넣습니다.";

pub(in crate::services::discord) fn bridge_claude_tui_followup_requeue_prompt_error(
    provider: &ProviderKind,
    runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    full_response: &str,
) -> bool {
    if !matches!(provider, ProviderKind::Claude)
        || !matches!(
            runtime_kind,
            Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui)
        )
    {
        return false;
    }
    let Some(error_text) = full_response
        .trim_start()
        .strip_prefix("Error:")
        .map(str::trim_start)
    else {
        return false;
    };
    crate::services::claude_tui::input::is_prompt_ready_timeout_error(error_text)
        && error_text.contains("follow-up prompt input readiness")
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
    fn legacy_streaming_edit_sanitizes_subagent_notification_3777() {
        let current_portion = r#"<subagent_notification>
{"agent_path":"/tmp/agent","status":{"completed":"Read-only review complete.\n\nVERDICT: CLEAN"}}
</subagent_notification>"#;
        let rendered = build_turn_bridge_streaming_edit_text(
            false,
            current_portion,
            "⠙ 계속 처리 중",
            &ProviderKind::Codex,
        );

        assert!(rendered.contains("Subagent completed"));
        assert!(rendered.contains("Read-only review complete."));
        assert!(rendered.contains("VERDICT: CLEAN"));
        assert!(rendered.ends_with("⠙ 계속 처리 중"));
        assert!(!rendered.contains("<subagent_notification>"));
        assert!(!rendered.contains("agent_path"));
        assert!(!rendered.contains("/tmp/agent"));
    }

    #[test]
    fn rollover_skips_start_anchored_subagent_notification_3777() {
        let current_portion = format!(
            r#"<subagent_notification>
{{"agent_path":"/tmp/agent","status":{{"completed":"{}"}}}}
</subagent_notification>"#,
            "x".repeat(2400),
        );

        assert!(bridge_streaming_rollover_should_skip(&current_portion));

        let rendered = build_turn_bridge_streaming_edit_text(
            false,
            &current_portion,
            "⠙ 계속 처리 중",
            &ProviderKind::Codex,
        );
        assert!(rendered.contains("Subagent completed"));
        assert!(!rendered.contains("<subagent_notification>"));
        assert!(!rendered.contains("agent_path"));
        assert!(rendered.len() <= 2000);
    }

    #[test]
    fn rollover_skips_chrome_prefixed_subagent_notification_3777() {
        let current_portion = format!(
            "No response requested.\n<subagent_notification>\n{{\"agent_path\":\"/tmp/agent\",\"status\":{{\"completed\":\"{}\"}}}}\n</subagent_notification>",
            "x".repeat(2400),
        );

        assert!(bridge_streaming_rollover_should_skip(&current_portion));

        let rendered = build_turn_bridge_streaming_edit_text(
            false,
            &current_portion,
            "⠙ 계속 처리 중",
            &ProviderKind::Codex,
        );
        assert!(rendered.contains("Subagent completed"));
        assert!(!rendered.contains("No response requested."));
        assert!(!rendered.contains("<subagent_notification>"));
        assert!(!rendered.contains("agent_path"));
        assert!(rendered.len() <= 2000);
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
    use crate::services::agent_protocol::RuntimeHandoffKind;

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
    fn followup_requeue_classifier_only_accepts_claude_tui_followup_readiness_timeouts() {
        let followup = "Error: timeout waiting for claude tui follow-up prompt input readiness after 45s; reason=prompt_marker_not_detected; previous_tui_turn_still_running=true; prompt_marker_detected=false; prompt_draft_detected=false; capture_available=true";
        assert!(bridge_claude_tui_followup_requeue_prompt_error(
            &ProviderKind::Claude,
            Some(RuntimeHandoffKind::ClaudeTui),
            followup
        ));

        for response in [
            "Error: timeout waiting for claude tui fresh prompt input readiness after 120s; fresh prompt readiness attempts exhausted (3 attempts)",
            "Error: timeout waiting for codex tui prompt input readiness after 8s",
            "Error: claude tui session died after prompt submit",
        ] {
            assert!(
                !bridge_claude_tui_followup_requeue_prompt_error(
                    &ProviderKind::Claude,
                    Some(RuntimeHandoffKind::ClaudeTui),
                    response
                ),
                "{response} must not enter the Claude follow-up requeue path"
            );
        }

        assert!(!bridge_claude_tui_followup_requeue_prompt_error(
            &ProviderKind::Codex,
            Some(RuntimeHandoffKind::CodexTui),
            followup
        ));
        assert!(!bridge_claude_tui_followup_requeue_prompt_error(
            &ProviderKind::Claude,
            None,
            followup
        ));
        assert!(CLAUDE_TUI_FOLLOWUP_REQUEUE_DELIVERY_NOTICE.contains("재시도 큐"));
    }

    #[test]
    fn classifier_rejects_post_submit_and_ambiguous_tui_errors() {
        for response in [
            "Error: claude tui session died after prompt submit",
            "Error: claude tui prompt submit confirmation unavailable after 3 retries; capture_available=false",
            "Error: claude tui prompt submit left draft after 3 enter retries; prompt_marker_detected=true; prompt_draft_detected=true; capture_available=true",
            "Error: Timeout waiting for output file",
        ] {
            assert!(
                !bridge_pre_submission_tui_prompt_error(&ProviderKind::Claude, response),
                "{response} must not be retried as a fresh prompt"
            );
        }
    }

    #[test]
    fn tui_transport_errors_skip_quiescence_only_for_matching_tui_runtime() {
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
