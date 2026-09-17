//! Provider turn-interrupt policy decisions for the tmux turn runtime (#3479).
//!
//! Pure decision helpers only — no I/O. Covers the per-provider send-keys plan
//! (`ProviderTurnInterruptPlan`), the SIGINT-fallback gating (#3021, #3029(A)),
//! claude's session-preserving delivery selection (#3207), and the
//! teardown-SIGINT suppression sentinel (#3169). Async orchestration
//! (`interrupt_provider_cli_turn`, `stop_active_turn`) stays in the parent module.

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ProviderTurnInterruptPlan {
    pub(super) keys: &'static [&'static str],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::services::discord) struct ProviderTurnInterruptOutcome {
    pub tmux_session: Option<String>,
    pub sent_keys: bool,
    pub fallback_sigint_pid: Option<u32>,
    pub missing_tmux_session: bool,
    /// #3029(A): true when claude's SIGINT-only interrupt needed a PID for an
    /// actively-generating turn but the lookup returned `None` — a silent
    /// no-op (mailbox marks [Stopped], no signal reaches the provider). Lets
    /// the hard-stop path escalate instead of reporting false success.
    pub sigint_target_missing: bool,
}

/// #3029(A): true iff SIGINT is the *only* interrupt delivery for this
/// provider (no send-keys path reaches the turn). Claude qualifies: a pane
/// C-c hits the wrapper and tears the session down (#1260).
fn interrupt_is_sigint_only(provider: &ProviderKind, plan_keys_empty: bool) -> bool {
    plan_keys_empty && matches!(provider, ProviderKind::Claude)
}

/// #3029(A): true when the SIGINT-only path found the turn active
/// (`ready_for_input == false`) but no PID resolved. An idle pane resolving to
/// no PID is intentional (#3021), not a miss.
pub(super) fn interrupt_sigint_target_missing(
    provider: &ProviderKind,
    plan_keys_empty: bool,
    ready_for_input: bool,
    resolved_sigint_pid: Option<u32>,
) -> bool {
    interrupt_is_sigint_only(provider, plan_keys_empty)
        && !ready_for_input
        && resolved_sigint_pid.is_none()
}

pub(super) fn provider_turn_interrupt_plan(
    provider: &ProviderKind,
) -> Option<ProviderTurnInterruptPlan> {
    match provider {
        // Claude's stdin is piped from the wrapper, not wired to the PTY, so
        // `send-keys C-c` hits the wrapper (no SIGINT handler → pane dies,
        // #1260) instead of claude. Empty keys signals "skip send-keys, use
        // the SIGINT fallback" (direct PID signal) instead.
        ProviderKind::Claude => Some(ProviderTurnInterruptPlan { keys: &[] }),
        ProviderKind::Codex => Some(ProviderTurnInterruptPlan { keys: &["Escape"] }),
        ProviderKind::Qwen => Some(ProviderTurnInterruptPlan { keys: &["C-c"] }),
        ProviderKind::Gemini
        | ProviderKind::Grok
        | ProviderKind::Antigravity
        | ProviderKind::OpenCode
        | ProviderKind::Unsupported(_) => None,
    }
}

/// #3207: how claude's turn is cancelled without killing the session — a
/// direct SIGINT exits the CLI and tears the whole tmux session down.
///   * `TuiEscape` — claude is the interactive TUI in the pane fg (`TuiHosting`,
///     #2110); `send-keys Escape` cancels the generation like a user ESC.
///   * `StreamJsonControlRequest` — claude runs under `agentdesk tmux-wrapper`
///     in stream-json mode; a `control_request{subtype:interrupt}` line to the
///     wrapper FIFO is forwarded to claude stdin, which acks and aborts the
///     turn while keeping the session open. Verified against claude CLI 2.1.168.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClaudeTurnInterruptDelivery {
    TuiEscape,
    StreamJsonControlRequest,
}

/// #3207: selects the delivery mechanism from whether the pane fg is the
/// wrapper (stream-json) or the bare TUI. Unclassified panes default to
/// `TuiEscape` — an ESC delivered to a wrapper PTY is an inert partial line
/// (the wrapper only acts on complete JSON envelopes), so it cannot kill it.
pub(crate) fn claude_turn_interrupt_delivery(
    pane_foreground_is_wrapper: bool,
) -> ClaudeTurnInterruptDelivery {
    if pane_foreground_is_wrapper {
        ClaudeTurnInterruptDelivery::StreamJsonControlRequest
    } else {
        ClaudeTurnInterruptDelivery::TuiEscape
    }
}

/// #3207: the stream-json interrupt control envelope (Agent SDK `interrupt()`
/// shape), written as one newline-terminated line to the wrapper input FIFO.
pub(crate) fn build_claude_interrupt_control_line(request_id: &str) -> String {
    serde_json::json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "interrupt" }
    })
    .to_string()
}

pub(super) fn fallback_sigint_pid_for_provider(
    provider: &ProviderKind,
    ready_for_input: bool,
    provider_pid: Option<u32>,
) -> Option<u32> {
    match provider {
        // #3021: claude's only interrupt is direct SIGINT, which on an idle
        // pane is a process-kill (tears the session down as "dead after
        // turn") — destructive under `PreserveSession`. Skip it when the pane
        // is confirmed idle (`ready_for_input`, re-probed by the caller); an
        // actively streaming turn (`ready_for_input == false`) is still
        // interrupted (#1260). Mirrors `hard_stop_pid_for_unresponsive_provider`.
        ProviderKind::Claude => {
            if ready_for_input {
                None
            } else {
                provider_pid
            }
        }
        // Codex/Qwen's send-keys reaches the wrapper PTY, not the child
        // process, so the SIGINT fallback is what actually stops them. Unlike
        // Claude they get no confirmation re-probe, so never gate on
        // `ready_for_input` — always deliver when the PID is known (#1260).
        ProviderKind::Codex | ProviderKind::Qwen => provider_pid,
        ProviderKind::Gemini
        | ProviderKind::Grok
        | ProviderKind::Antigravity
        | ProviderKind::OpenCode
        | ProviderKind::Unsupported(_) => None,
    }
}

/// #3169: sentinel `reason` the turn_bridge cancel epilogue records when the
/// cancellation had no user-attributable `cancel_source` — an anonymous
/// internal `PreserveSession` teardown, not a user stop (every user stop path
/// and the watchdog pass their own descriptive reason). Shared with the
/// producer in `turn_bridge/mod.rs` as a single source of truth.
pub(in crate::services::discord) const ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON: &str =
    "turn_bridge_cancelled";

/// #3169: claude's only interrupt is SIGINT, which on a busy TUI is a process
/// kill (#1260) — correct for an explicit user stop, but destructive for an
/// anonymous internal teardown meant to PRESERVE the session. Self-collision:
/// a busy-queue follow-up starts a fresh generation, its watcher handoff times
/// out on the #2161/#2293 quiescence gate, the same bridge turn cancels, and
/// the teardown SIGINT kills the live session.
///
/// Suppressed only on that anonymous path; the live turn is left for the
/// watcher's deadline-armed reconciler. A genuine hang is caught separately by
/// the stall-watchdog, so this does not strand stuck turns. User-explicit
/// stops keep their SIGINT, and codex/qwen (real send-keys interrupt) are
/// unaffected.
pub(super) fn claude_teardown_sigint_suppressed(provider: &ProviderKind, reason: &str) -> bool {
    matches!(provider, ProviderKind::Claude) && reason == ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON
}

#[cfg(test)]
mod sigint_target_missing_tests {
    use super::interrupt_sigint_target_missing;
    use crate::services::provider::ProviderKind;

    #[test]
    fn active_claude_without_pid_is_a_missed_interrupt() {
        assert!(
            interrupt_sigint_target_missing(&ProviderKind::Claude, true, false, None),
            "active claude with no resolvable PID must escalate (#3029 A), not silently succeed"
        );
    }

    #[test]
    fn active_claude_with_pid_is_not_missed() {
        assert!(
            !interrupt_sigint_target_missing(&ProviderKind::Claude, true, false, Some(42)),
            "a resolved PID means the SIGINT had a target — not a miss"
        );
    }

    #[test]
    fn idle_claude_without_pid_is_not_missed() {
        assert!(
            !interrupt_sigint_target_missing(&ProviderKind::Claude, true, true, None),
            "idle claude (ready_for_input=true) is intentionally skipped, not a miss (#3021)"
        );
    }

    #[test]
    fn wrapped_providers_are_not_sigint_only() {
        // Codex/Qwen have a send-keys path, so a missing PID isn't SIGINT-only.
        assert!(!interrupt_sigint_target_missing(
            &ProviderKind::Codex,
            false,
            false,
            None
        ));
        assert!(!interrupt_sigint_target_missing(
            &ProviderKind::Qwen,
            false,
            false,
            None
        ));
    }
}

#[cfg(test)]
mod claude_session_preserving_interrupt_tests {
    use super::{
        ClaudeTurnInterruptDelivery, build_claude_interrupt_control_line,
        claude_turn_interrupt_delivery,
    };

    #[test]
    fn tui_pane_uses_escape_keystroke() {
        assert_eq!(
            claude_turn_interrupt_delivery(false),
            ClaudeTurnInterruptDelivery::TuiEscape
        );
    }

    #[test]
    fn wrapper_pane_uses_stream_json_control_request() {
        assert_eq!(
            claude_turn_interrupt_delivery(true),
            ClaudeTurnInterruptDelivery::StreamJsonControlRequest
        );
    }

    #[test]
    fn control_request_envelope_matches_agent_sdk_interrupt_shape() {
        let line = build_claude_interrupt_control_line("req-123");
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("valid json line");
        assert_eq!(parsed["type"], "control_request");
        assert_eq!(parsed["request_id"], "req-123");
        assert_eq!(parsed["request"]["subtype"], "interrupt");
        assert!(!line.contains('\n'), "envelope is a single line");
    }
}

#[cfg(test)]
mod claude_teardown_sigint_tests {
    use super::{ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON, claude_teardown_sigint_suppressed};
    use crate::services::provider::ProviderKind;

    #[test]
    fn claude_anonymous_teardown_suppresses_sigint() {
        // Anonymous PreserveSession teardown; claude's SIGINT would kill the
        // busy session, so it must be suppressed (#3169).
        assert!(
            claude_teardown_sigint_suppressed(
                &ProviderKind::Claude,
                ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON
            ),
            "claude on an anonymous turn_bridge teardown must NOT receive the session-killing SIGINT"
        );
    }

    #[test]
    fn claude_user_explicit_stop_keeps_sigint() {
        // User stops pass their own reason, never the anonymous sentinel.
        for reason in [
            "/stop",
            "!stop",
            "reaction remove ⏳",
            "!skill stop",
            "!cc stop",
            "mailbox_cancel_active_turn",
            "watchdog timeout",
        ] {
            assert!(
                !claude_teardown_sigint_suppressed(&ProviderKind::Claude, reason),
                "user-explicit / watchdog stop ({reason}) must still SIGINT claude (stop preserved)"
            );
        }
    }

    #[test]
    fn non_claude_providers_are_unaffected() {
        // codex/qwen have a real send-keys interrupt; suppression never applies.
        for provider in [ProviderKind::Codex, ProviderKind::Qwen] {
            assert!(
                !claude_teardown_sigint_suppressed(
                    &provider,
                    ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON
                ),
                "non-claude provider must keep its existing teardown SIGINT behaviour"
            );
        }
    }
}
