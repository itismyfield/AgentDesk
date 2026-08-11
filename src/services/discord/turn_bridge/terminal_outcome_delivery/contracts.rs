use super::*;
use crate::services::{agent_protocol::RuntimeHandoffKind, discord::inflight::CodexRange};

pub(in crate::services::discord::turn_bridge) struct TerminalOutcomeDeliveryContext {
    pub(in crate::services::discord::turn_bridge) channel_id: ChannelId,
    pub(in crate::services::discord::turn_bridge) user_msg_id: Option<MessageId>,
    pub(in crate::services::discord::turn_bridge) current_msg_id: MessageId,
    pub(in crate::services::discord::turn_bridge) status_panel_msg_id: Option<MessageId>,
    pub(in crate::services::discord::turn_bridge) cancelled: bool,
    pub(in crate::services::discord::turn_bridge) transport_error: bool,
    pub(in crate::services::discord::turn_bridge) recovery_retry: bool,
    pub(in crate::services::discord::turn_bridge) rx_disconnected: bool,
    pub(in crate::services::discord::turn_bridge) tmux_last_offset: Option<u64>,
    pub(in super::super) codex_tui_terminal_range: Option<CodexRange>,
    pub(in crate::services::discord::turn_bridge) watcher_owner_channel_id: ChannelId,
    pub(in crate::services::discord::turn_bridge) watcher_handoff_claim_outcome:
        WatcherHandoffClaimOutcome,
    pub(in crate::services::discord::turn_bridge) bridge_created_response_placeholder_msg_id:
        Option<MessageId>,
    pub(in crate::services::discord::turn_bridge) bridge_relay_delegated_to_watcher: bool,
    pub(in crate::services::discord::turn_bridge) bridge_output_owner: Option<BridgeOutputOwner>,
    pub(in crate::services::discord::turn_bridge) should_complete_work_dispatch_after_delivery:
        bool,
    pub(in crate::services::discord::turn_bridge) should_fail_dispatch_after_delivery: bool,
    pub(in crate::services::discord::turn_bridge) can_chain_locally: bool,
    pub(in crate::services::discord::turn_bridge) single_message_panel_footer_mode: bool,
    pub(in crate::services::discord::turn_bridge) is_prompt_too_long: bool,
    pub(in crate::services::discord::turn_bridge) claude_tui_followup_pre_submit_requeue_candidate:
        bool,
    pub(in crate::services::discord::turn_bridge) tui_error_classification: TuiErrorClassification,
    pub(in crate::services::discord::turn_bridge) had_prior_session_id_at_turn_start: bool,
    pub(in crate::services::discord::turn_bridge) session_handshake_seen: bool,
    pub(in crate::services::discord::turn_bridge) turn_start: std::time::Instant,
    #[cfg(unix)]
    pub(in crate::services::discord::turn_bridge) bridge_tui_gate_outcome_early:
        Option<super::super::super::tmux::TuiCompletionGateOutcome>,
}

pub(in crate::services::discord::turn_bridge) struct TerminalOutcomeDeliveryState {
    pub(in crate::services::discord::turn_bridge) shared_owned: Arc<SharedData>,
    pub(in crate::services::discord::turn_bridge) gateway: Arc<dyn TurnGateway>,
    pub(in crate::services::discord::turn_bridge) provider: ProviderKind,
    pub(in crate::services::discord::turn_bridge) cancel_token:
        Arc<crate::services::provider::CancelToken>,
    pub(in crate::services::discord::turn_bridge) turn_id: String,
    pub(in crate::services::discord::turn_bridge) user_text_owned: String,
    pub(in crate::services::discord::turn_bridge) adk_session_key: Option<String>,
    pub(in crate::services::discord::turn_bridge) adk_cwd: Option<String>,
    pub(in crate::services::discord::turn_bridge) dispatch_id: Option<String>,
    pub(in crate::services::discord::turn_bridge) new_session_id: Option<String>,
    pub(in crate::services::discord::turn_bridge) new_raw_provider_session_id: Option<String>,
    pub(in crate::services::discord::turn_bridge) full_response: String,
    pub(in crate::services::discord::turn_bridge) active_background_child_session_ids: Vec<i64>,
    pub(in crate::services::discord::turn_bridge) pending_long_running_open_after_state_save:
        PendingLongRunningOpenAfterStateSave,
    pub(in crate::services::discord::turn_bridge) pending_long_running_retarget_after_state_save:
        PendingLongRunningRetargetAfterStateSave,
    pub(in crate::services::discord::turn_bridge) long_running_placeholder_active:
        LongRunningPlaceholderActive,
    pub(in crate::services::discord::turn_bridge) inflight_state: InflightTurnState,
    pub(in crate::services::discord::turn_bridge) api_friction_reports:
        Vec<crate::services::api_friction::ApiFrictionReport>,
    pub(in crate::services::discord::turn_bridge) review_dispatch_warning: Option<String>,
    pub(in crate::services::discord::turn_bridge) last_edit_text: String,
    pub(in crate::services::discord::turn_bridge) terminal_empty_response_notice: Option<String>,
    pub(in crate::services::discord::turn_bridge) terminal_full_replay_cleanup_msg_ids:
        Vec<MessageId>,
    pub(in crate::services::discord::turn_bridge) resume_failure_detected: bool,
    pub(in crate::services::discord::turn_bridge) response_sent_offset: usize,
}

pub(in crate::services::discord::turn_bridge) enum TerminalOutcomeDeliveryOutcome {
    Completed,
}

pub(in crate::services::discord::turn_bridge) struct TerminalOutcomeDeliveryOutput {
    pub(in crate::services::discord::turn_bridge) outcome: TerminalOutcomeDeliveryOutcome,
    pub(in crate::services::discord::turn_bridge) shared_owned: Arc<SharedData>,
    pub(in crate::services::discord::turn_bridge) gateway: Arc<dyn TurnGateway>,
    pub(in crate::services::discord::turn_bridge) provider: ProviderKind,
    pub(in crate::services::discord::turn_bridge) cancel_token:
        Arc<crate::services::provider::CancelToken>,
    pub(in crate::services::discord::turn_bridge) turn_id: String,
    pub(in crate::services::discord::turn_bridge) user_text_owned: String,
    pub(in crate::services::discord::turn_bridge) adk_session_key: Option<String>,
    pub(in crate::services::discord::turn_bridge) adk_cwd: Option<String>,
    pub(in crate::services::discord::turn_bridge) dispatch_id: Option<String>,
    pub(in crate::services::discord::turn_bridge) new_session_id: Option<String>,
    pub(in crate::services::discord::turn_bridge) new_raw_provider_session_id: Option<String>,
    pub(in crate::services::discord::turn_bridge) full_response: String,
    pub(in crate::services::discord::turn_bridge) active_background_child_session_ids: Vec<i64>,
    pub(in crate::services::discord::turn_bridge) pending_long_running_open_after_state_save:
        PendingLongRunningOpenAfterStateSave,
    pub(in crate::services::discord::turn_bridge) pending_long_running_retarget_after_state_save:
        PendingLongRunningRetargetAfterStateSave,
    pub(in crate::services::discord::turn_bridge) long_running_placeholder_active:
        LongRunningPlaceholderActive,
    pub(in crate::services::discord::turn_bridge) inflight_state: InflightTurnState,
    pub(in crate::services::discord::turn_bridge) api_friction_reports:
        Vec<crate::services::api_friction::ApiFrictionReport>,
    pub(in crate::services::discord::turn_bridge) status_panel_terminal_committed: bool,
    pub(in crate::services::discord::turn_bridge) bridge_should_emit_completion: bool,
    pub(in crate::services::discord::turn_bridge) completion_footer_terminal_text: Option<String>,
    pub(in crate::services::discord::turn_bridge) busy_requeue_outcome:
        Option<followup_requeue::FollowupRequeueOutcome>,
    pub(in crate::services::discord::turn_bridge) preserve_inflight_for_cleanup_retry: bool,
    pub(in crate::services::discord::turn_bridge) bridge_skip_holder_owns_inflight: bool,
    pub(in crate::services::discord::turn_bridge) terminal_delivery_committed: bool,
    pub(in crate::services::discord::turn_bridge) resume_failure_detected: bool,
    pub(in crate::services::discord::turn_bridge) terminal_empty_response_notice: Option<String>,
    pub(in crate::services::discord::turn_bridge) terminal_full_replay_cleanup_msg_ids:
        Vec<MessageId>,
    pub(in crate::services::discord::turn_bridge) response_sent_offset: usize,
    pub(in crate::services::discord::turn_bridge) turn_start: std::time::Instant,
}

/// The two range ends a terminal delivery needs, deliberately kept apart.
///
/// #5264 PR-B: `pinned` is the pinned receipt/frontier authority. For a Codex TUI turn it
/// exists only when a terminal frame was admitted, because the exact source receipt is
/// what authorises advancing the durable frontier.
///
/// `exclusion_lease` is the end of the #3041 P1-2 cross-actor lease range, and the
/// admitted latch must never narrow it. A non-admitted CodexTui turn still has a real
/// `[turn_start_offset, tmux_last_offset)` to deliver; collapsing that end to `None` makes
/// `BridgeDeliveryLease::acquire` return `NoRange`, so the bridge sends holding no lease
/// while the watcher can independently acquire the same cell and range. That is the
/// cross-actor duplicate prevention this PR exists to strengthen, so the two ends are
/// returned separately rather than as one value two consumers reinterpret.
/// The fields are private and the pinned accessor consumes, which blocks ONE mutation
/// shape: reading `into_pinned()` before `exclusion_lease()` moves the value and fails to
/// compile.
///
/// That is the whole of what the type buys, and an earlier revision of this comment
/// overstated it. A review disproved the stronger claim with a cheaper edit that still
/// compiles and passes every suite:
///
/// ```ignore
/// let pinned_range_end = range_ends.into_pinned();
/// let tmux_last_offset = pinned_range_end;
/// ```
///
/// Nothing drives `run_terminal_outcome_delivery`, so no test observes which end the legacy
/// fallback actually consumes. Until a driver for that call site exists, this wiring is
/// unsealed and the type is a speed bump, not a proof.
pub(super) struct TerminalRangeEnds {
    pinned: Option<u64>,
    exclusion_lease: Option<u64>,
}

impl TerminalRangeEnds {
    /// The end the legacy fallback delivers and leases over. Read this BEFORE
    /// [`Self::into_pinned`].
    pub(super) fn exclusion_lease(&self) -> Option<u64> {
        self.exclusion_lease
    }

    pub(super) fn into_pinned(self) -> Option<u64> {
        self.pinned
    }
}

pub(super) fn terminal_range_ends(
    provider: &ProviderKind,
    runtime_kind: Option<RuntimeHandoffKind>,
    tmux_last_offset: Option<u64>,
    admitted: Option<&CodexRange>,
) -> TerminalRangeEnds {
    TerminalRangeEnds {
        pinned: ordered_terminal_range_end(provider, runtime_kind, tmux_last_offset, admitted),
        exclusion_lease: tmux_last_offset,
    }
}

pub(super) fn ordered_terminal_range_end(
    provider: &ProviderKind,
    runtime_kind: Option<RuntimeHandoffKind>,
    tmux_last_offset: Option<u64>,
    admitted: Option<&CodexRange>,
) -> Option<u64> {
    match (provider, runtime_kind) {
        (ProviderKind::Codex, Some(RuntimeHandoffKind::CodexTui)) => {
            admitted.map(CodexRange::complete_record_end)
        }
        _ => tmux_last_offset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[rustfmt::skip]
    #[test]
    fn codex_tui_terminal_range_end_is_latch_only_5264() {
        assert_eq!(ordered_terminal_range_end(&ProviderKind::Codex, Some(RuntimeHandoffKind::CodexTui), Some(99), None), None);
        assert_eq!(ordered_terminal_range_end(&ProviderKind::Claude, None, Some(99), None), Some(99));
    }

    // #5264 PR-B: the latch above is the pinned receipt/frontier authority ONLY. It must
    // not reach the #3041 exclusion lease: a non-admitted CodexTui turn with a real
    // observed range that acquires no lease lets the bridge send while the watcher can
    // independently acquire the same cell. The test above pins the latch projection and
    // says nothing about the lease, which is exactly how that regression shipped.
    #[test]
    fn codex_tui_admitted_latch_does_not_narrow_the_exclusion_lease_5264() {
        let codex = terminal_range_ends(
            &ProviderKind::Codex,
            Some(RuntimeHandoffKind::CodexTui),
            Some(99),
            None,
        );
        assert_eq!(
            codex.pinned, None,
            "no admitted frame means no pinned receipt authority"
        );
        assert_eq!(
            codex.exclusion_lease,
            Some(99),
            "the #3041 cross-actor lease must still cover the observed range"
        );
        let claude = terminal_range_ends(&ProviderKind::Claude, None, Some(99), None);
        assert_eq!(
            (claude.pinned, claude.exclusion_lease),
            (Some(99), Some(99))
        );
    }
}
