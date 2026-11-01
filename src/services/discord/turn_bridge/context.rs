use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum WatcherHandoffClaimOutcome {
    None,
    ReusedExisting,
    Spawned,
}
// Shared by the bridge task body below and the extracted stream_loop.rs
// (#4230 S6) — must live at module scope so both resolve them.
pub(super) const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub(super) const LIVE_LONG_RUN_HEARTBEAT_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(30);

/// Bridge lifecycle notification, not proof of durable terminal delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum BridgeCompletionSignal {
    /// Terminal transport was confirmed. The consumer still checks the durable row.
    Finalized,
    /// Terminal delivery remains with a persisted custody obligation.
    DeferredToCustody,
    /// Another relay owns the pending terminal delivery.
    DeferredToOwner,
    /// No confirmed delivery or successful custody handoff was established.
    Unresolved,
    /// Pre-authority abort: no stream frame consumed and no finalizer registered.
    EntryAborted,
}

pub(in crate::services::discord) struct TurnBridgeContext {
    pub(in crate::services::discord) provider: ProviderKind,
    pub(in crate::services::discord) gateway: Arc<dyn TurnGateway>,
    pub(in crate::services::discord) channel_id: ChannelId,
    /// `None` for a recovery turn with no anchored Discord user message
    /// (user_msg_id == 0, e.g. a TUI-direct turn). All Discord-message side
    /// effects keyed on it (reactions, analytics row) are skipped.
    pub(in crate::services::discord) user_msg_id: Option<MessageId>,
    pub(in crate::services::discord) user_text_owned: String,
    pub(in crate::services::discord) request_owner_name: String,
    pub(in crate::services::discord) role_binding: Option<RoleBinding>,
    pub(in crate::services::discord) adk_session_key: Option<String>,
    pub(in crate::services::discord) adk_session_name: Option<String>,
    pub(in crate::services::discord) adk_session_info: Option<String>,
    pub(in crate::services::discord) adk_cwd: Option<String>,
    pub(in crate::services::discord) dispatch_id: Option<String>,
    pub(in crate::services::discord) dispatch_kind: Option<String>,
    pub(in crate::services::discord) memory_recall_usage: TokenUsage,
    pub(in crate::services::discord) context_window_tokens: u64,
    pub(in crate::services::discord) context_compact_percent: u64,
    /// `None` for a recovery turn that never anchored a Discord placeholder
    /// (current_msg_id == 0, e.g. a TUI-direct turn). The bridge then creates a
    /// fresh placeholder on first output instead of editing a nonexistent one.
    pub(in crate::services::discord) current_msg_id: Option<MessageId>,
    pub(in crate::services::discord) response_sent_offset: usize,
    pub(in crate::services::discord) full_response: String,
    pub(in crate::services::discord) tmux_last_offset: Option<u64>,
    pub(in crate::services::discord) new_session_id: Option<String>,
    pub(in crate::services::discord) defer_watcher_resume: bool,
    /// Reuse the persisted V2 status panel only when resuming the same
    /// in-flight turn. Fresh turns must allocate a new panel near the new
    /// response instead of editing an old panel buried in scrollback.
    pub(in crate::services::discord) reuse_status_panel_message: bool,
    pub(in crate::services::discord) completion_tx:
        Option<tokio::sync::oneshot::Sender<BridgeCompletionSignal>>,
    /// `true` ONLY at the two TUI external-input idle callers. Default `false`
    /// for every other bridge caller; used by footer/chrome decisions that need
    /// the origin without a `request_owner_name` string compare.
    pub(in crate::services::discord) is_external_input_tui_direct: bool,
    pub(in crate::services::discord) inflight_state: InflightTurnState,
}

#[cfg(test)]
mod tests {
    use crate::services::discord::router::source_guard_support::{Lexed, forge};
    use crate::test_support::CommentForgery;

    const INTAKE: &str = include_str!("../router/message_handler/intake_turn.rs");
    const WAIT: &str = "if let Some(rx) = completion_rx {";
    const EXECUTED: &str = "Ok(IntakeExecution::Executed)";
    const WAITER: &str = "if let Some(rx) = completion_rx {\n    rx.await\n        .map_err(|_| \"queued turn completion wait failed\".to_string())?;\n}";

    /// Classification 2: from the first coded wait to the executed return, the code is exactly
    /// the waiter that drops the typed payload, ignoring comments and whitespace outside literals.
    fn completion_waiter_gaps(intake: &str) -> Vec<String> {
        let lexed = Lexed::new(intake);
        let waiter = lexed.find(WAIT).and_then(|at| {
            let end = lexed.find_after(at, EXECUTED)?;
            Some(lexed.slice(at..end).compact())
        });
        match waiter {
            Some(found) if found.text() == Lexed::new(WAITER).compact().text() => Vec::new(),
            found => vec![format!(
                "the completion waiter is not exactly the recv-error map: {found:?}"
            )],
        }
    }

    #[test]
    fn intake_completion_waiter_discards_typed_payload_and_only_maps_recv_error() {
        assert_eq!(completion_waiter_gaps(INTAKE), Vec::<String>::new());
    }

    #[test]
    fn completion_waiter_guard_follows_code_not_comments_or_quoted_copies() {
        for needle in [WAIT, "rx.await", ".to_string())?;", EXECUTED] {
            for forgery in CommentForgery::ALL {
                let forged = forge(INTAKE, needle, forgery);
                assert!(
                    !completion_waiter_gaps(&forged).is_empty(),
                    "{forgery:?} `{needle}`"
                );
            }
        }
        let noted = INTAKE.replacen("rx.await", "rx.await // payload dropped", 1);
        assert_ne!(noted, INTAKE);
        assert_eq!(completion_waiter_gaps(&noted), Vec::<String>::new());
        // A quoted copy of the waiter ahead of a real waiter that keeps the payload is no proof.
        let at = INTAKE.find(WAIT).expect("intake completion wait");
        let decoy = format!("let _decoy = r#\"{WAITER}\n{EXECUTED}\"#;\n");
        let kept = INTAKE.replacen(
            ".map_err(|_| \"queued",
            ".map(|payload| payload).map_err(|_| \"queued",
            1,
        );
        assert_ne!(kept, INTAKE);
        let decoyed = [&kept[..at], decoy.as_str(), &kept[at..]].concat();
        assert!(!completion_waiter_gaps(&decoyed).is_empty());
    }
}
