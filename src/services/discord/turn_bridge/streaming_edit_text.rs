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
    super::response_sanitizer::subagent_notification_card::streaming_rollover_should_skip(
        current_portion,
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

/// #3885 (reworked): same-input-aware gate for the claude_tui follow-up
/// pre-submit requeue.
///
/// A follow-up pre-submit readiness timeout normally requeues the inflight
/// message — the pre-submit assumption is "the prompt never reached the pane, so
/// re-injecting is safe". The risk is a DUPLICATE: when the SAME input already
/// landed on the pane (it is still streaming, or it just completed), the turn it
/// drives already delivers the response, so a requeue re-injects a second copy →
/// duplicate prose relay.
///
/// The first cut of #3885 gated this on a CHANNEL-SCOPED busy probe
/// (`idle_queue_blocked_by_hosted_tui_busy_pane`): suppress whenever the pane was
/// busy. That probe has ZERO correlation to *which* turn is streaming, so it
/// conflated two cases:
///   - (A) the SAME input is the streaming/just-completed turn → requeue dups
///     (must suppress), and
///   - (B) a DIFFERENT prior turn occupies the pane while a genuinely-unsubmitted
///     follow-up waits behind it → suppressing DROPS the follow-up (it is
///     finalized as a transport-error failure and never retried).
/// It also missed the already-COMPLETED same-input case (idle pane → probe reads
/// not-busy → requeue → dup).
///
/// This gate instead keys on INPUT CORRELATION. `same_input_occupies_pane` is
/// true only when the recorded prompt anchor for this pane resolves to THIS
/// inflight's user message id (see
/// [`claude_tui_followup_same_input_occupies_pane`]): the same input already
/// landed (streaming or just-completed) so the response is covered. A
/// different/absent anchor means the follow-up is genuinely unsubmitted, so it
/// STILL requeues — the deferred idle-queue kickoff is itself gated on pane-busy,
/// so a case-(B) follow-up is DEFERRED behind the occupying turn (preserved in
/// the mailbox), not dropped. This preserves the original dup-prevention for
/// case (A) without the case-(B) drop or the already-completed dup.
///
/// `requeue_candidate` is the base decision (feature enabled + readiness-timeout
/// error).
pub(in crate::services::discord) fn claude_tui_followup_requeue_streaming_aware(
    requeue_candidate: bool,
    same_input_occupies_pane: bool,
) -> bool {
    requeue_candidate && !same_input_occupies_pane
}

/// #3885 (reworked): does the input that occupies the pane match THIS follow-up?
///
/// `anchor_message_id` is the message id of the prompt anchor recorded for this
/// pane (`tui_prompt_dedupe::prompt_anchor_for_response`, a non-consuming peek);
/// it is the user-message id of the prompt the relay last submitted to the pane,
/// which equals the synthetic inflight's `user_msg_id`. When it matches this
/// inflight's `inflight_user_msg_id`, the same input already landed (it is the
/// streaming or just-completed turn) and a requeue would duplicate its prose.
///
/// A `None` anchor (none recorded / TTL-expired / channel mismatch) or a
/// mismatched id means a DIFFERENT input — or none — holds the pane, so the
/// follow-up is genuinely unsubmitted and must be preserved (requeued/deferred),
/// not suppressed. `inflight_user_msg_id == 0` (id-0 synthetic turns) never
/// matches: `record_prompt_anchor` rejects a zero message id, so a recorded
/// anchor is always nonzero and the zero guard avoids a false suppression.
pub(in crate::services::discord) fn claude_tui_followup_same_input_occupies_pane(
    anchor_message_id: Option<u64>,
    inflight_user_msg_id: u64,
) -> bool {
    inflight_user_msg_id != 0 && anchor_message_id == Some(inflight_user_msg_id)
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

    // ---- #3885 (reworked): same-input-aware follow-up requeue gate ----
    //
    // The base requeue decision (`requeue_candidate`) is UNCHANGED; the gate is
    // now keyed on INPUT CORRELATION (`same_input_occupies_pane`) instead of a
    // channel-scoped busy probe. These pin that:
    //   - the SAME input already on the pane (streaming OR just-completed) does
    //     NOT requeue → the covering turn delivers, no duplicate prose relay
    //     (original dup-prevention + the already-completed dup, both closed);
    //   - a genuinely-unsubmitted follow-up (different/absent anchor) STILL
    //     requeues → the deferred idle-queue kickoff defers it behind any
    //     occupying turn instead of DROPPING it as a transport-error failure.

    #[test]
    fn same_input_on_pane_suppresses_followup_requeue_no_dup_relay() {
        // (1b) + (2): the same input already landed on the pane and is either
        // still streaming or just completed (anchor still resolves to this
        // inflight). Requeuing would re-inject a duplicate, so the candidate must
        // be suppressed — the streaming/completed turn already delivers the prose.
        assert!(!claude_tui_followup_requeue_streaming_aware(true, true));
    }

    #[test]
    fn different_or_absent_pane_input_still_requeues_so_followup_is_deferred_not_dropped() {
        // (1a): a DIFFERENT prior turn occupies the pane (or it is quiescent) and
        // this follow-up's prompt never reached the TUI. The candidate must stay
        // true so the follow-up is requeued — the deferred idle-queue kickoff
        // (gated on pane-busy) defers it behind the occupying turn rather than
        // letting the suppressed path finalize it as a transport-error drop.
        assert!(claude_tui_followup_requeue_streaming_aware(true, false));
    }

    #[test]
    fn non_requeue_base_never_requeues_regardless_of_pane_input() {
        // When the base decision is false (feature off / non-readiness error)
        // the gate must never synthesize a requeue from the correlation signal.
        assert!(!claude_tui_followup_requeue_streaming_aware(false, false));
        assert!(!claude_tui_followup_requeue_streaming_aware(false, true));
    }

    #[test]
    fn same_input_occupies_pane_matches_only_this_inflights_anchor() {
        // Pure correlation: a recorded anchor that resolves to THIS inflight's
        // user_msg_id means the same input already landed → suppress; a different
        // or absent anchor means a different/unsubmitted input → requeue.
        let this_msg = 7_001_u64;
        let other_msg = 9_002_u64;
        assert!(claude_tui_followup_same_input_occupies_pane(
            Some(this_msg),
            this_msg
        ));
        assert!(!claude_tui_followup_same_input_occupies_pane(
            Some(other_msg),
            this_msg
        ));
        assert!(!claude_tui_followup_same_input_occupies_pane(
            None, this_msg
        ));
        // id-0 synthetic turns never match (record_prompt_anchor rejects id 0).
        assert!(!claude_tui_followup_same_input_occupies_pane(Some(0), 0));
        assert!(!claude_tui_followup_same_input_occupies_pane(None, 0));
    }

    #[test]
    fn same_input_correlation_against_live_recorded_prompt_anchor() {
        // Wiring pin: feed the gate from the REAL shared dedupe anchor state the
        // bridge reads via `prompt_anchor_for_response` (a non-consuming peek).
        // The relay records the anchor for the submitted input; the bridge then
        // recognises its own input and suppresses the dup-prone requeue, while a
        // follow-up whose id differs from the recorded anchor still requeues.
        use crate::services::tui_prompt_dedupe::{
            prompt_anchor_for_response, record_prompt_anchor, reset_state_for_tests,
        };
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        reset_state_for_tests();

        let tmux = "AgentDesk-claude-followup-requeue-corr";
        let channel = 4242_u64;
        let same_input = 5_551_u64;
        let other_input = 6_662_u64;

        // Relay submitted `same_input` to the pane → anchor recorded.
        record_prompt_anchor("claude", tmux, channel, same_input);

        let resolve =
            || prompt_anchor_for_response("claude", tmux, channel).map(|anchor| anchor.message_id);

        // Same-input follow-up: anchor resolves to it → suppress (no dup relay),
        // and the peek must NOT consume the anchor the watcher still needs.
        let same = claude_tui_followup_same_input_occupies_pane(resolve(), same_input);
        assert!(same, "same-input follow-up must be recognised as on-pane");
        assert!(
            !claude_tui_followup_requeue_streaming_aware(true, same),
            "same-input follow-up must NOT requeue (dup-prevention)"
        );

        // Different follow-up behind the same occupying turn: not the recorded
        // anchor → must still requeue so it is deferred, not dropped.
        let different = claude_tui_followup_same_input_occupies_pane(resolve(), other_input);
        assert!(
            !different,
            "a different-input follow-up must not match the recorded anchor"
        );
        assert!(
            claude_tui_followup_requeue_streaming_aware(true, different),
            "different/unsubmitted follow-up must requeue (deferred, not dropped)"
        );
    }
}
