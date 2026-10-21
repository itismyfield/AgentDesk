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

    /// One error-classification hand-off: the scope that holds it and its exact field run.
    struct Joint {
        source: &'static str,
        scope: &'static str,
        expected: &'static str,
        name: &'static str,
    }

    const JOINTS: [Joint; 4] = [
        Joint {
            source: include_str!("../content_arms.rs"),
            scope: "StreamContentArmMessage::Error { message, stderr }",
            expected: "let error_resolution = resolve_tui_error(&provider, &message, &stderr); tui_error_classification = error_resolution.tui_error_classification; transport_error = !recovery_retry;",
            name: "Error arm -> stream-loop state",
        },
        Joint {
            source: include_str!("../../mod.rs"),
            scope: "post_loop_finalize::PostLoopFinalizeContext {",
            expected: "transport_error, tui_error_classification: stream_loop_output.tui_error_classification, recovery_retry,",
            name: "StreamLoopOutput -> PostLoopFinalizeContext",
        },
        Joint {
            source: include_str!("../../post_loop_finalize.rs"),
            scope: "pub(super) async fn run_post_loop_finalize(",
            expected: "claude_tui_followup_pre_submit_requeue_candidate, tui_error_classification, review_dispatch_warning,",
            name: "post-loop local -> PostLoopFinalizeOutput",
        },
        Joint {
            source: include_str!("../../terminal_outcome_delivery.rs"),
            scope: "DeliveryEpilogueContext {",
            expected: "claude_tui_followup_pre_submit_requeue_candidate, claude_tui_busy_requeue_pending, tui_error_classification, #[cfg(unix)] bridge_tui_gate_outcome_early,",
            name: "terminal outcome -> DeliveryEpilogueContext",
        },
    ];

    /// Classification 2: inside the joint's real arm, struct literal or fn, the field run
    /// appears in code, comments and whitespace ignored. A lexical tripwire: no reachability.
    fn wiring_joint_gaps(source: &str, joint: &Joint) -> Vec<String> {
        use crate::services::discord::router::source_guard_support::Lexed;
        let tight = |text: &str| Lexed::new(text).compact().text().to_string();
        let lexed = Lexed::new(source).compact();
        let scope = if joint.scope.ends_with('{') || joint.scope.ends_with('(') {
            lexed.block(&tight(joint.scope))
        } else {
            lexed.arm(&tight(joint.scope))
        };
        match scope {
            Some(scope) if scope.contains(&tight(joint.expected)) => Vec::new(),
            Some(_) => vec![format!(
                "{} must preserve the Error-arm TUI classification",
                joint.name
            )],
            None => vec![format!("{}: missing `{}`", joint.name, joint.scope)],
        }
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

        // The async finalization contexts need live gateway/shared state, so each hand-off is
        // pinned at its construction site while the assertions above prove the carried value.
        for joint in &JOINTS {
            assert_eq!(wiring_joint_gaps(joint.source, joint), Vec::<String>::new());
        }
    }

    #[test]
    fn wiring_joints_reject_commented_fields_and_out_of_scope_copies() {
        use crate::services::discord::router::source_guard_support::forge;
        use crate::test_support::CommentForgery;
        let fields = [
            "error_resolution.tui_error_classification;",
            "tui_error_classification: stream_loop_output.tui_error_classification,",
            "tui_error_classification,\n        review_dispatch_warning,",
            "claude_tui_busy_requeue_pending,",
        ];
        for (joint, field) in JOINTS.iter().zip(fields) {
            for needle in [joint.scope, field] {
                for forgery in CommentForgery::ALL {
                    let forged = forge(joint.source, needle, forgery);
                    assert!(
                        !wiring_joint_gaps(&forged, joint).is_empty(),
                        "{}: {forgery:?} commented `{needle}` still passes",
                        joint.name
                    );
                }
            }
        }
        // The field run moved out of the PostLoopFinalizeContext literal no longer counts.
        let joint = &JOINTS[1];
        let field = fields[1];
        let moved = joint.source.replacen(field, "", 1)
            + "\nfn _copy() { x(transport_error, tui_error_classification: stream_loop_output.tui_error_classification, recovery_retry,) }\n";
        assert!(!wiring_joint_gaps(&moved, joint).is_empty());
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
