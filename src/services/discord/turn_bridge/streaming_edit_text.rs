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

/// #3813 Phase 1b: should the streaming status-edit gate open on this loop pass?
///
/// The default gate opens only once every `status_interval` (default 5s), which
/// forces the very first assistant answer to wait up to that interval before it
/// reaches Discord. This predicate adds a single-shot fast lane: when the FIRST
/// non-empty assistant text portion is observed (`!first_answer_relayed &&
/// !current_portion_empty`), the gate opens immediately regardless of the
/// interval, so the opening answer is relayed without the 5s delay.
///
/// After that first non-empty edit the caller flips `first_answer_relayed`, so
/// every subsequent pass falls back to the pure `elapsed_ge_interval` throttle —
/// the fast lane never fires twice and status/tool-only passes (empty portion)
/// never consume it.
pub(in crate::services::discord) fn bridge_streaming_edit_gate_open(
    elapsed_ge_interval: bool,
    first_answer_relayed: bool,
    current_portion_empty: bool,
) -> bool {
    elapsed_ge_interval || (!first_answer_relayed && !current_portion_empty)
}

/// Presentation-independent classification captured from a raw provider error.
///
/// Provider errors are rendered as folded, actionable guidance before terminal
/// finalization. Keep the lifecycle decisions alongside that presentation so
/// requeue and quiescence behavior do not depend on the rendered text shape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::services::discord) struct TuiErrorClassification {
    pre_submission_prompt_error: bool,
    claude_followup_requeue_prompt_error: bool,
    claude_followup_busy_readiness_timeout: bool,
    transport_error_should_skip_quiescence: bool,
}

pub(in crate::services::discord) fn classify_raw_tui_error(
    provider: &ProviderKind,
    error_text: &str,
) -> TuiErrorClassification {
    let pre_submission_prompt_error = match provider {
        ProviderKind::Claude => {
            crate::services::claude_tui::input::is_prompt_ready_timeout_error(error_text)
        }
        ProviderKind::Codex => {
            crate::services::codex_tui::input::is_prompt_ready_timeout_error(error_text)
        }
        _ => false,
    };
    let claude_followup_requeue_prompt_error = matches!(provider, ProviderKind::Claude)
        && pre_submission_prompt_error
        && error_text.contains("follow-up prompt input readiness");
    // A follow-up readiness timeout only reaches this classifier when the tmux
    // pane is still alive (the capture that produced the timeout proves the pane
    // exists). The producer derives its two witnesses from that same pane
    // snapshot:
    //   previous_tui_turn_still_running = tmux_pane_alive && !prompt_marker_detected
    //   prompt_marker_detected          = tmux_pane_alive && REPL idle at a prompt
    // They are mutually exclusive and each implies tmux_pane_alive, so their
    // union is exactly "pane-alive readiness timeout". A live pane means the
    // resumed provider session is intact — the follow-up input simply failed to
    // confirm in time — so the empty-response no-handshake fallback must NOT kill
    // it for a fresh retry (that would drop the resumed session and start over,
    // violating the restart live-turn-preservation contract). #4605 covered only
    // the first witness (pane busy, no marker); the second (pane idle at a ready
    // prompt — a *more* certain "session alive" signal, and the exact restart
    // follow-up case) leaked through to fresh-session (#4640). A genuinely failed
    // resume surfaces a stale session_id error in the output file, which
    // output_file_has_stale_resume_error_after_offset detects and retries ahead
    // of this preserve path, so widening here cannot mask a real resume failure.
    let claude_followup_busy_readiness_timeout = claude_followup_requeue_prompt_error
        && (error_text.contains("previous_tui_turn_still_running=true")
            || error_text.contains("prompt_marker_detected=true"));
    let transport_error_should_skip_quiescence = match provider {
        ProviderKind::Claude => {
            pre_submission_prompt_error
                || error_text == "Timeout waiting for output file"
                || error_text.starts_with("timeout waiting for claude tui transcript file")
                || error_text.contains("claude tui session died")
        }
        ProviderKind::Codex => {
            pre_submission_prompt_error
                || error_text == "Timeout waiting for output file"
                || error_text.contains("codex tui session died")
        }
        _ => false,
    };

    TuiErrorClassification {
        pre_submission_prompt_error,
        claude_followup_requeue_prompt_error,
        claude_followup_busy_readiness_timeout,
        transport_error_should_skip_quiescence,
    }
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
    classification: TuiErrorClassification,
) -> bool {
    if classification.pre_submission_prompt_error {
        return true;
    }
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

pub(in crate::services::discord) const CLAUDE_TUI_FOLLOWUP_REQUEUE_DELIVERY_NOTICE: &str = "";

pub(in crate::services::discord) fn bridge_claude_tui_followup_busy_readiness_timeout(
    provider: &ProviderKind,
    runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    classification: TuiErrorClassification,
) -> bool {
    matches!(provider, ProviderKind::Claude)
        && matches!(
            runtime_kind,
            Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui)
        )
        && classification.claude_followup_busy_readiness_timeout
}

pub(in crate::services::discord) fn bridge_claude_tui_followup_requeue_prompt_error(
    provider: &ProviderKind,
    runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    full_response: &str,
    classification: TuiErrorClassification,
) -> bool {
    if !matches!(provider, ProviderKind::Claude)
        || !matches!(
            runtime_kind,
            Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui)
        )
    {
        return false;
    }
    if classification.claude_followup_requeue_prompt_error {
        return true;
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
/// The first cut of #3885 gated this on a CHANNEL-SCOPED busy probe: suppress
/// whenever the pane was busy. That probe has ZERO correlation to *which* turn
/// is streaming, so it conflated two cases:
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
/// STILL requeues and stays preserved in the mailbox. Dispatch remains blocked
/// by the active-turn snapshot guard until finalization publishes the completion
/// event that kicks the drain. This preserves the original dup-prevention for
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
    classification: TuiErrorClassification,
) -> bool {
    let legacy_error_text = full_response
        .trim_start()
        .strip_prefix("Error:")
        .map(str::trim_start);

    match (provider, runtime_kind) {
        (
            ProviderKind::Claude,
            Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui),
        ) => {
            bridge_pre_submission_tui_prompt_error(provider, full_response, classification)
                || classification.transport_error_should_skip_quiescence
                || legacy_error_text.is_some_and(|error_text| {
                    error_text == "Timeout waiting for output file"
                        || error_text.starts_with("timeout waiting for claude tui transcript file")
                        || error_text.contains("claude tui session died")
                })
        }
        (
            ProviderKind::Codex,
            Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui),
        ) => {
            bridge_pre_submission_tui_prompt_error(provider, full_response, classification)
                || classification.transport_error_should_skip_quiescence
                || legacy_error_text.is_some_and(|error_text| {
                    error_text == "Timeout waiting for output file"
                        || error_text.contains("codex tui session died")
                })
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

    // ---- #3813 Phase 1b: first-output fast-lane status-edit gate ----
    //
    // Truth table over (elapsed_ge_interval, first_answer_relayed,
    // current_portion_empty): the fast lane opens the gate exactly once — for the
    // first non-empty assistant text before the interval elapses — and otherwise
    // defers to the pure interval throttle.
    #[test]
    fn fast_lane_opens_for_first_non_empty_answer_before_interval() {
        // First non-empty assistant text, interval not yet reached → fast lane
        // opens the gate so the opening answer is not delayed up to 5s.
        assert!(bridge_streaming_edit_gate_open(false, false, false));
    }

    #[test]
    fn fast_lane_stays_closed_for_status_only_before_interval() {
        // No assistant body yet (status/tool-only change), interval not reached →
        // gate stays closed so status-only passes never consume the fast lane.
        assert!(!bridge_streaming_edit_gate_open(false, false, true));
    }

    #[test]
    fn fast_lane_not_reused_after_first_answer_before_interval() {
        // First answer already relayed, interval not reached → gate closed, i.e.
        // subsequent streaming edits return to the normal throttle.
        assert!(!bridge_streaming_edit_gate_open(false, true, false));
    }

    #[test]
    fn interval_elapsed_always_opens_gate_preserving_legacy_behavior() {
        // Once the interval has elapsed the gate always opens, regardless of the
        // fast-lane inputs — the pre-existing throttle behavior is preserved.
        assert!(bridge_streaming_edit_gate_open(true, true, false));
        assert!(bridge_streaming_edit_gate_open(true, false, true));
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
            TuiErrorClassification::default(),
        ));
        assert!(bridge_pre_submission_tui_prompt_error(
            &ProviderKind::Codex,
            "Error: timeout waiting for codex tui follow-up prompt input readiness after 45s; reason=composer_not_detected; previous_tui_turn_still_running=true; capture_available=true",
            TuiErrorClassification::default(),
        ));
        assert!(!bridge_pre_submission_tui_prompt_error(
            &ProviderKind::Claude,
            "Error: claude tui session died during follow-up output reading",
            TuiErrorClassification::default(),
        ));
        assert!(!bridge_pre_submission_tui_prompt_error(
            &ProviderKind::Claude,
            "timeout waiting for claude tui follow-up prompt input readiness after 45s",
            TuiErrorClassification::default(),
        ));
    }

    #[test]
    fn followup_requeue_classifier_only_accepts_claude_tui_followup_readiness_timeouts() {
        let followup = "Error: timeout waiting for claude tui follow-up prompt input readiness after 45s; reason=prompt_marker_not_detected; previous_tui_turn_still_running=true; prompt_marker_detected=false; prompt_draft_detected=false; capture_available=true";
        assert!(bridge_claude_tui_followup_requeue_prompt_error(
            &ProviderKind::Claude,
            Some(RuntimeHandoffKind::ClaudeTui),
            followup,
            TuiErrorClassification::default(),
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
                    response,
                    TuiErrorClassification::default(),
                ),
                "{response} must not enter the Claude follow-up requeue path"
            );
        }

        assert!(!bridge_claude_tui_followup_requeue_prompt_error(
            &ProviderKind::Codex,
            Some(RuntimeHandoffKind::CodexTui),
            followup,
            TuiErrorClassification::default(),
        ));
        assert!(!bridge_claude_tui_followup_requeue_prompt_error(
            &ProviderKind::Claude,
            None,
            followup,
            TuiErrorClassification::default(),
        ));
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
                !bridge_pre_submission_tui_prompt_error(
                    &ProviderKind::Claude,
                    response,
                    TuiErrorClassification::default(),
                ),
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
            TuiErrorClassification::default(),
        ));
        assert!(bridge_tui_transport_error_should_skip_quiescence(
            &ProviderKind::Claude,
            Some(RuntimeHandoffKind::ClaudeTui),
            "Error: timeout waiting for claude tui transcript file after 120s; capture_available=true; prompt_marker_detected=true; prompt_draft_detected=false",
            TuiErrorClassification::default(),
        ));
        assert!(bridge_tui_transport_error_should_skip_quiescence(
            &ProviderKind::Codex,
            Some(RuntimeHandoffKind::CodexTui),
            "Error: timeout waiting for codex tui follow-up prompt input readiness after 45s; reason=composer_not_detected; previous_tui_turn_still_running=true; capture_available=true",
            TuiErrorClassification::default(),
        ));
        assert!(!bridge_tui_transport_error_should_skip_quiescence(
            &ProviderKind::Claude,
            Some(RuntimeHandoffKind::LegacyTmuxWrapper),
            "Error: Timeout waiting for output file",
            TuiErrorClassification::default(),
        ));
        assert!(!bridge_tui_transport_error_should_skip_quiescence(
            &ProviderKind::Claude,
            Some(RuntimeHandoffKind::ClaudeTui),
            "Error: upstream API returned 500",
            TuiErrorClassification::default(),
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
        // true so the follow-up is requeued, preserved behind the live turn, and
        // kicked by the completion event instead of letting the suppressed path
        // finalize it as a transport-error drop.
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

    #[test]
    fn long_streaming_same_input_still_suppresses_followup_requeue_past_legacy_ttl() {
        // #3885 follow-up residual close: a build/agent turn that streams 30-60min
        // is the routine issue-pipeline workflow. The anchor is stamped ONCE at
        // submit and not re-stamped, so under the legacy 30min purge the anchor
        // vanished mid-stream → bridge peek None → same_input=false → requeue →
        // the original #3885 duplicate. With `PROMPT_ANCHOR_SUBMIT_TTL` (4h) the
        // anchor survives, so a same-input follow-up arriving 31min into the
        // stream still correlates and is suppressed (no dup).
        use crate::services::tui_prompt_dedupe::{
            prompt_anchor_for_response, record_prompt_anchor_aged_for_tests, reset_state_for_tests,
        };
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        reset_state_for_tests();

        let tmux = "AgentDesk-claude-longstream-corr";
        let channel = 9393_u64;
        let streaming_input = 1_212_u64;

        // Anchor stamped at submit for a turn that has now streamed 31min
        // (> the legacy 30min purge).
        record_prompt_anchor_aged_for_tests(
            "claude",
            tmux,
            channel,
            streaming_input,
            std::time::Duration::from_secs(31 * 60),
        );

        let anchor_msg_id =
            prompt_anchor_for_response("claude", tmux, channel).map(|anchor| anchor.message_id);
        let same = claude_tui_followup_same_input_occupies_pane(anchor_msg_id, streaming_input);
        assert!(
            same,
            "a 31min-streaming same-input turn's anchor must still resolve (no mid-stream purge)"
        );
        assert!(
            !claude_tui_followup_requeue_streaming_aware(true, same),
            "long-streaming same-input follow-up must stay suppressed (no #3885 dup)"
        );
    }
}
