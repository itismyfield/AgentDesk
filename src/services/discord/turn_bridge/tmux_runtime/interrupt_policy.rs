//! Provider turn-interrupt policy decisions for the tmux turn runtime
//! (#3479 split).
//!
//! Behavior-preserving extraction from `tmux_runtime.rs`: the pure
//! decision helpers + value types that select HOW a provider turn is
//! interrupted without performing any I/O — the per-provider send-keys plan
//! (`ProviderTurnInterruptPlan`), the interrupt outcome record
//! (`ProviderTurnInterruptOutcome`), the `#3029(A)` missed-SIGINT-target
//! detection, the `#3021` SIGINT-fallback gating, the `#3207` claude
//! session-preserving delivery selection + control envelope, and the `#3169`
//! teardown-SIGINT suppression sentinel/decision. The async orchestration that
//! drives these (`interrupt_provider_cli_turn`, `stop_active_turn`, the
//! hard-stop path) stays in the parent module and reaches these via
//! `use interrupt_policy::*`.

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
    /// #3029(A): set when the SIGINT-only interrupt path (empty key list —
    /// claude, whose pane C-c targets the wrapper) needed to deliver a SIGINT
    /// to an actively-generating turn but the provider PID lookup returned
    /// `None` (ps failure, command-name drift, or a just-spawned child not yet
    /// visible). On that path the interrupt is the *only* delivery mechanism,
    /// so a `None` PID is a silent no-op: the mailbox marks the turn [Stopped]
    /// but no signal reaches the provider. This flag converts that into an
    /// explicit failure the hard-stop path can escalate on, instead of
    /// reporting unconditional success.
    pub sigint_target_missing: bool,
}

/// #3029(A): does this provider/plan combination treat the direct SIGINT
/// fallback as its *only* interrupt delivery (i.e. there is no send-keys path
/// that could have reached the turn)? Claude uses an empty key list because a
/// pane C-c hits the wrapper and tears the session down (#1260) — so when its
/// SIGINT target is missing, the interrupt silently did nothing.
fn interrupt_is_sigint_only(provider: &ProviderKind, plan_keys_empty: bool) -> bool {
    plan_keys_empty && matches!(provider, ProviderKind::Claude)
}

/// #3029(A): the interrupt silently did nothing when the SIGINT-only path was
/// the only delivery mechanism, the turn was genuinely active
/// (`ready_for_input == false`), yet no SIGINT target PID could be resolved.
/// An idle pane (`ready_for_input == true`) intentionally resolves to no PID
/// and is NOT a missed interrupt (#3021).
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
        // Claude runs as a child of `agentdesk tmux-wrapper`, with stdin
        // *piped* from the wrapper rather than wired to the PTY. A
        // `tmux send-keys C-c` on the pane therefore delivers SIGINT to the
        // wrapper (the PTY foreground), not to claude — and the wrapper has
        // no SIGINT handler, so it dies and tears the pane down with it
        // (#1260). We send SIGINT directly to claude's PID via the fallback
        // path instead; the empty key list signals "skip send-keys, go
        // straight to the SIGINT fallback".
        ProviderKind::Claude => Some(ProviderTurnInterruptPlan { keys: &[] }),
        ProviderKind::Codex => Some(ProviderTurnInterruptPlan { keys: &["Escape"] }),
        ProviderKind::Qwen => Some(ProviderTurnInterruptPlan { keys: &["C-c"] }),
        ProviderKind::Gemini | ProviderKind::OpenCode | ProviderKind::Unsupported(_) => None,
    }
}

/// #3207 (part 1): how claude's TURN is cancelled without killing the session.
/// claude has no SIGINT-safe interrupt — a direct SIGINT to the CLI exits it,
/// the pane collapses, and the watcher tears the whole tmux session down
/// ("dead session after turn"). The session-preserving cancel depends on how
/// claude is hosted in the pane:
///   * `TuiEscape` — claude runs as the interactive TUI in the pane foreground
///     (the default `TuiHosting` driver since #2110). `tmux send-keys Escape`
///     reaches the TUI and cancels the active generation exactly like a user
///     pressing ESC, leaving the session alive for the next turn.
///   * `StreamJsonControlRequest` — claude runs as a child of
///     `agentdesk tmux-wrapper` in `--print --input-format stream-json
///     --output-format stream-json` mode. A `control_request{subtype:interrupt}`
///     line written to the wrapper's input FIFO is forwarded verbatim to claude
///     stdin; the CLI acks it (`control_response{success}`), aborts the turn
///     (`terminal_reason=aborted_streaming`), and keeps the session open for the
///     next `user` envelope. Empirically verified against claude CLI 2.1.168.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClaudeTurnInterruptDelivery {
    TuiEscape,
    StreamJsonControlRequest,
}

/// #3207 (part 1): select the session-preserving claude interrupt mechanism from
/// whether the tmux pane foreground is the `agentdesk tmux-wrapper` (stream-json
/// host) or the bare claude TUI. When the pane command cannot be classified we
/// default to `TuiEscape`: ESC is the live `TuiHosting` path, and an ESC keystroke
/// delivered to a wrapper PTY is an inert partial-line that cannot kill anything
/// (the wrapper only acts on complete newline-terminated JSON envelopes).
pub(crate) fn claude_turn_interrupt_delivery(
    pane_foreground_is_wrapper: bool,
) -> ClaudeTurnInterruptDelivery {
    if pane_foreground_is_wrapper {
        ClaudeTurnInterruptDelivery::StreamJsonControlRequest
    } else {
        ClaudeTurnInterruptDelivery::TuiEscape
    }
}

/// #3207 (part 1): build the stream-json interrupt control envelope the Agent
/// SDK `interrupt()` uses. Written as a single newline-terminated line to the
/// wrapper input FIFO, which forwards it verbatim to claude stdin.
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
        // #3021: Claude has NO send-keys interrupt path (empty key list — see
        // `provider_turn_interrupt_plan`), so the direct SIGINT is both its
        // only interrupt AND, on an idle pane, a process-kill that terminates
        // the TUI, kills the pane, and makes the watcher tear down the whole
        // tmux session as "dead after turn". Under `PreserveSession` (e.g. ⏳
        // reaction removal on a finished-but-still-"active" turn) that destroys
        // the session + context — the opposite of the policy intent. When the
        // pane is confirmed idle (`ready_for_input`, double-checked by the
        // caller's confirmation re-probe so a stale post-submit read cannot
        // pass) there is no generation to interrupt, so skip the SIGINT and
        // leave the live process alone. An actively streaming turn reports
        // `ready_for_input == false` and is still interrupted (#1260). The
        // hard-stop path already treats `ready_for_input` as "do not kill"
        // (`hard_stop_pid_for_unresponsive_provider`); this mirrors it.
        ProviderKind::Claude => {
            if ready_for_input {
                None
            } else {
                provider_pid
            }
        }
        // Codex/Qwen also send Escape/C-c, but those keys reach the wrapper
        // PTY rather than the separately-spawned provider child, so the direct
        // SIGINT fallback is what actually stops them. The readiness probe can
        // read a stale terminal/ready state in the sub-second window after a
        // follow-up submit, and (unlike Claude) these providers get no
        // confirmation re-probe — so do NOT gate their interrupt on
        // `ready_for_input`. Always deliver the fallback when we have the child
        // PID, matching base-branch behavior (#1260).
        ProviderKind::Codex | ProviderKind::Qwen => provider_pid,
        ProviderKind::Gemini | ProviderKind::OpenCode | ProviderKind::Unsupported(_) => None,
    }
}

/// #3169 (death #3 — warm-followup teardown SIGINT self-collision): the
/// sentinel `reason` the turn_bridge cancel epilogue records when the
/// cancellation carried NO user-attributable `cancel_source`
/// (`CancelToken::cancel_source()` was `None`). This is the signature of an
/// anonymous / internal `PreserveSession` teardown — supersede, redundant
/// delivery, or a failed watcher handoff — NOT a user-initiated stop. Every
/// user stop path (`/stop`, `!stop`, `⏳` reaction removal, restart, skill
/// stop) and the watchdog timeout reach `stop_active_turn` under their own
/// descriptive reason and never produce this sentinel. Defined here and
/// referenced by the epilogue (`turn_bridge/mod.rs`) so the producer and this
/// SIGINT guard share a single source of truth.
pub(in crate::services::discord) const ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON: &str =
    "turn_bridge_cancelled";

/// #3169 (death #3): claude's ONLY interrupt is a direct SIGINT (it has no
/// send-keys path — see `provider_turn_interrupt_plan`), and on a *busy* TUI
/// that SIGINT is a process kill (#1260; the wrapper has no SIGINT handler, so
/// claude exits and the pane/session collapses — see
/// `fallback_sigint_pid_for_provider`). Killing the session is the correct,
/// desired outcome for an explicit user stop. But for an anonymous internal
/// teardown whose whole intent is to PRESERVE the reusable session, that SIGINT
/// destroys the just-started warm-followup turn and the entire claude session
/// (the #3169 self-collision: a busy-queue follow-up is injected, claude starts
/// a fresh generation, the watcher handoff for that turn fails on the
/// #2161/#2293 quiescence-gate timeout, the same bridge turn cancels, and the
/// teardown SIGINTs the live busy claude → exit 0 → session death).
///
/// Suppress the teardown SIGINT for claude on that anonymous path only. The
/// live turn is left running for the watcher to reconcile/finalize on its next
/// pass (the quiescence-gate already hands a stalled handoff to a
/// deadline-armed reconciler). A genuine claude *hang* is handled by the
/// separate stall-watchdog, so the teardown SIGINT is not the safety net for
/// hangs and dropping it here does not strand a stuck turn. User-explicit stops
/// keep their SIGINT (stop still works), and providers with a real send-keys
/// interrupt (codex/qwen) are unaffected — they are never claude and their
/// interrupt does not kill the session.
pub(super) fn claude_teardown_sigint_suppressed(provider: &ProviderKind, reason: &str) -> bool {
    matches!(provider, ProviderKind::Claude) && reason == ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON
}

// #3029(A): the "missed SIGINT target" decision is a pure boolean and runs
// under the default `cargo test` invocation (the main suite is gated behind
// the `legacy-sqlite-tests` feature, which CI does not enable by default).
#[cfg(test)]
mod sigint_target_missing_tests {
    use super::interrupt_sigint_target_missing;
    use crate::services::provider::ProviderKind;

    #[test]
    fn active_claude_without_pid_is_a_missed_interrupt() {
        // SIGINT-only path (empty keys = claude), turn actively generating
        // (ready_for_input=false), and NO resolved PID → silent no-op that must
        // now be flagged for escalation instead of reporting success.
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
        // #3021: an idle pane intentionally resolves to no PID and is left
        // alone; that is NOT a missed interrupt.
        assert!(
            !interrupt_sigint_target_missing(&ProviderKind::Claude, true, true, None),
            "idle claude (ready_for_input=true) is intentionally skipped, not a miss (#3021)"
        );
    }

    #[test]
    fn wrapped_providers_are_not_sigint_only() {
        // Codex/Qwen have a send-keys path, so a missing fallback PID is not a
        // SIGINT-only silent no-op (their send-keys still reaches the wrapper).
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

// #3207 (part 1): the claude session-preserving interrupt selection + envelope
// builder are pure and run under the default `cargo test` invocation (the main
// suite is gated behind `legacy-sqlite-tests`, which CI does not enable).
#[cfg(test)]
mod claude_session_preserving_interrupt_tests {
    use super::{
        ClaudeTurnInterruptDelivery, build_claude_interrupt_control_line,
        claude_turn_interrupt_delivery,
    };

    #[test]
    fn tui_pane_uses_escape_keystroke() {
        // The live `TuiHosting` driver: claude is the interactive pane fg, so ESC
        // cancels the turn without killing the session.
        assert_eq!(
            claude_turn_interrupt_delivery(false),
            ClaudeTurnInterruptDelivery::TuiEscape
        );
    }

    #[test]
    fn wrapper_pane_uses_stream_json_control_request() {
        // Legacy wrapper host: claude reads stdin from a pipe, so the turn is
        // cancelled via a stream-json control_request forwarded through the FIFO.
        assert_eq!(
            claude_turn_interrupt_delivery(true),
            ClaudeTurnInterruptDelivery::StreamJsonControlRequest
        );
    }

    #[test]
    fn control_request_envelope_matches_agent_sdk_interrupt_shape() {
        // Empirically verified against claude CLI 2.1.168: this exact envelope
        // yields control_response{success} + terminal_reason=aborted_streaming
        // and the session survives for the next user turn.
        let line = build_claude_interrupt_control_line("req-123");
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("valid json line");
        assert_eq!(parsed["type"], "control_request");
        assert_eq!(parsed["request_id"], "req-123");
        assert_eq!(parsed["request"]["subtype"], "interrupt");
        assert!(!line.contains('\n'), "envelope is a single line");
    }
}

// #3169 (death #3): the teardown-SIGINT suppression decision is a pure boolean
// and runs under the default `cargo test` invocation (the main suite is gated
// behind the `legacy-sqlite-tests` feature, which CI does not enable).
#[cfg(test)]
mod claude_teardown_sigint_tests {
    use super::{ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON, claude_teardown_sigint_suppressed};
    use crate::services::provider::ProviderKind;

    #[test]
    fn claude_anonymous_teardown_suppresses_sigint() {
        // The forensic death path: an anonymous/internal PreserveSession
        // teardown (no user cancel_source) reaches stop_active_turn under the
        // sentinel reason. claude's SIGINT would kill the busy TUI + session, so
        // it MUST be suppressed (#3169).
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
        // Every user-initiated stop passes its own descriptive reason directly
        // to stop_active_turn — never the anonymous sentinel — so the stop
        // feature is preserved: claude is still interrupted on an explicit stop.
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
        // codex/qwen have a real send-keys interrupt and their SIGINT does not
        // kill the session, so the suppression never applies to them — even on
        // the anonymous teardown reason. Existing behaviour is preserved.
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
