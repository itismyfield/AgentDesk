use super::*;

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
