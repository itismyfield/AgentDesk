use std::collections::VecDeque;
use std::sync::Arc;

use super::super::bridge_latency_spans::BridgeLatencySpans;
use super::super::stream_tick::{
    LongRunningPlaceholderActive, PendingLongRunningOpenAfterStateSave,
    PendingLongRunningRetargetAfterStateSave,
};
use super::super::{streaming_edit_text::TuiErrorClassification, *};
use super::exit_reconcile::StreamLoopOutcome;

macro_rules! refresh_expected_after_handoff {
    ($identity:expr, $baseline:expr, $state:expr, $outcome:expr) => {
        $crate::services::discord::turn_bridge::stream_loop::expected_identity::refresh_stream_tick_expected_identity_after_handoff(
            &mut $identity,
            &mut $baseline,
            &$state,
            $outcome,
        )
    };
}
pub(super) use refresh_expected_after_handoff;

macro_rules! refresh_or_retain_runtime_handoff_inner {
    ($identity:expr, $baseline:expr, $state:expr, $pending:expr, $outcome:expr, $retry_pending:ident, $retry_retained:ident) => {{
        let outcome = $outcome;
        refresh_expected_after_handoff!($identity, $baseline, $state, outcome.guarded_save_outcome);
        if let Some(retry_message) = outcome.retry_message {
            $pending.push_front(retry_message.into_stream_message());
            $retry_pending = true;
            $retry_retained = true;
            true
        } else {
            $retry_retained = false;
            false
        }
    }};
}
pub(super) use refresh_or_retain_runtime_handoff_inner;

macro_rules! stop_on_tool_authority_loss {
    ($outcome:expr, $loop_outcome:ident, $label:lifetime) => {
        if matches!(
            $outcome,
            $crate::services::discord::turn_bridge::stream_loop::tool_arms::StreamToolArmOutcome::AuthorityLost
        ) {
            $loop_outcome = StreamLoopOutcome::AuthorityLost;
            break $label;
        }
    };
}
pub(super) use stop_on_tool_authority_loss;

pub(super) async fn receive_next_stream_message(
    rx: &mut StreamMessageReceiverAdapter,
    pending: &mut VecDeque<StreamMessage>,
    done: bool,
    drain_until: Option<std::time::Instant>,
) {
    let wait = turn_bridge_stream_wait_duration(done, drain_until, std::time::Instant::now());
    if wait.is_zero() {
        if let Ok(message) = rx.try_recv() {
            pending.push_back(message);
        }
    } else if let Ok(Some(message)) = tokio::time::timeout(wait, rx.recv()).await {
        pending.push_back(message);
    }
}

pub(in crate::services::discord::turn_bridge) struct StreamLoopContext {
    pub(in crate::services::discord::turn_bridge) shared_owned: Arc<SharedData>,
    pub(in crate::services::discord::turn_bridge) gateway: Arc<dyn TurnGateway>,
    pub(in crate::services::discord::turn_bridge) channel_id: ChannelId,
    pub(in crate::services::discord::turn_bridge) provider: ProviderKind,
    pub(in crate::services::discord::turn_bridge) cancel_token:
        Arc<crate::services::provider::CancelToken>,
    pub(in crate::services::discord::turn_bridge) user_text_owned: String,
    pub(in crate::services::discord::turn_bridge) request_owner_name: String,
    pub(in crate::services::discord::turn_bridge) adk_session_key: Option<String>,
    pub(in crate::services::discord::turn_bridge) adk_session_name: Option<String>,
    pub(in crate::services::discord::turn_bridge) adk_session_info: Option<String>,
    pub(in crate::services::discord::turn_bridge) adk_cwd: Option<String>,
    pub(in crate::services::discord::turn_bridge) dispatch_id: Option<String>,
    pub(in crate::services::discord::turn_bridge) role_binding: Option<RoleBinding>,
    pub(in crate::services::discord::turn_bridge) turn_id: String,
    pub(in crate::services::discord::turn_bridge) voice_progress_playback_channel_id:
        Option<ChannelId>,
    pub(in crate::services::discord::turn_bridge) single_message_panel_footer_mode: bool,
    pub(in crate::services::discord::turn_bridge) footer_owner:
        crate::services::discord::footer_view_reconciler::CompletionFooterOwner,
    pub(in crate::services::discord::turn_bridge) status_panel_started_at: i64,
    pub(in crate::services::discord::turn_bridge) status_interval: std::time::Duration,
    pub(in crate::services::discord::turn_bridge) context_window_tokens: u64,
    pub(in crate::services::discord::turn_bridge) context_compact_percent: u64,
}

pub(in crate::services::discord::turn_bridge) struct StreamLoopState<'a> {
    pub(in crate::services::discord::turn_bridge) rx: &'a mut StreamMessageReceiverAdapter,
    pub(in crate::services::discord::turn_bridge) full_response: &'a mut String,
    pub(in crate::services::discord::turn_bridge) last_edit_text: &'a mut String,
    pub(in crate::services::discord::turn_bridge) done: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) cancelled: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) rx_disconnected: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) current_tool_line: &'a mut Option<String>,
    pub(in crate::services::discord::turn_bridge) prev_tool_status: &'a mut Option<String>,
    pub(in crate::services::discord::turn_bridge) last_tool_name: &'a mut Option<String>,
    pub(in crate::services::discord::turn_bridge) last_tool_summary: &'a mut Option<String>,
    pub(in crate::services::discord::turn_bridge) accumulated_input_tokens: &'a mut u64,
    pub(in crate::services::discord::turn_bridge) accumulated_cache_create_tokens: &'a mut u64,
    pub(in crate::services::discord::turn_bridge) accumulated_cache_read_tokens: &'a mut u64,
    pub(in crate::services::discord::turn_bridge) accumulated_output_tokens: &'a mut u64,
    pub(in crate::services::discord::turn_bridge) spin_idx: &'a mut usize,
    pub(in crate::services::discord::turn_bridge) restart_followup_pending: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) any_tool_used: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) has_post_tool_text: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) tmux_handed_off: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) watcher_owns_assistant_relay: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) watcher_relay_available_for_turn: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) watcher_handoff_claim_outcome:
        &'a mut WatcherHandoffClaimOutcome,
    pub(in crate::services::discord::turn_bridge) standby_relay_owns_output: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) last_assistant_text_line: &'a mut Option<String>,
    pub(in crate::services::discord::turn_bridge) long_running_placeholder_active:
        &'a mut LongRunningPlaceholderActive,
    pub(in crate::services::discord::turn_bridge) active_background_child_session_ids:
        &'a mut Vec<i64>,
    pub(in crate::services::discord::turn_bridge) transport_error: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) transcript_events:
        &'a mut Vec<SessionTranscriptEvent>,
    pub(in crate::services::discord::turn_bridge) resume_failure_detected: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) session_handshake_seen: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) terminal_session_reset_required: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) recovery_retry: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) last_adk_heartbeat: &'a mut std::time::Instant,
    pub(in crate::services::discord::turn_bridge) pending_stream_messages:
        &'a mut VecDeque<StreamMessage>,
    pub(in crate::services::discord::turn_bridge) pending_status_tool_results:
        &'a mut VecDeque<String>,
    pub(in crate::services::discord::turn_bridge) pending_status_tool_results_by_id:
        &'a mut std::collections::HashMap<String, String>,
    pub(in crate::services::discord::turn_bridge) last_inflight_long_run_heartbeat:
        &'a mut std::time::Instant,
    pub(in crate::services::discord::turn_bridge) last_activity_heartbeat_at:
        &'a mut Option<std::time::Instant>,
    pub(in crate::services::discord::turn_bridge) terminal_control_ready_observed: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) terminal_control_drain_until:
        &'a mut Option<std::time::Instant>,
    pub(in crate::services::discord::turn_bridge) current_msg_id: &'a mut MessageId,
    pub(in crate::services::discord::turn_bridge) expected_current_message: &'a mut (u64, usize),
    pub(in crate::services::discord::turn_bridge) bridge_created_response_placeholder_msg_id:
        &'a mut Option<MessageId>,
    pub(in crate::services::discord::turn_bridge) response_sent_offset: &'a mut usize,
    pub(in crate::services::discord::turn_bridge) bridge_confirmed_response_sent_offset:
        &'a mut usize,
    pub(in crate::services::discord::turn_bridge) streamed_assistant_text_this_turn: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) streaming_rollover_frozen_msg_ids:
        &'a mut Vec<MessageId>,
    pub(in crate::services::discord::turn_bridge) terminal_full_replay_cleanup_msg_ids:
        &'a mut Vec<MessageId>,
    pub(in crate::services::discord::turn_bridge) tmux_last_offset: &'a mut Option<u64>,
    pub(in crate::services::discord::turn_bridge) watcher_owner_channel_id: &'a mut ChannelId,
    pub(in crate::services::discord::turn_bridge) new_session_id: &'a mut Option<String>,
    pub(in crate::services::discord::turn_bridge) new_raw_provider_session_id:
        &'a mut Option<String>,
    pub(in crate::services::discord::turn_bridge) inflight_state: &'a mut InflightTurnState,
    pub(in crate::services::discord::turn_bridge) last_status_edit: &'a mut tokio::time::Instant,
    pub(in crate::services::discord::turn_bridge) first_answer_relayed: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) last_session_panel_lifecycle_refresh:
        &'a mut tokio::time::Instant,
    pub(in crate::services::discord::turn_bridge) status_panel_msg_id: &'a mut Option<MessageId>,
    pub(in crate::services::discord::turn_bridge) last_status_panel_text: &'a mut String,
    pub(in crate::services::discord::turn_bridge) status_panel_dirty: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) last_status_panel_edit:
        &'a mut tokio::time::Instant,
    pub(in crate::services::discord::turn_bridge) bridge_spans: &'a mut BridgeLatencySpans,
    pub(in crate::services::discord::turn_bridge) status_panel_generation: &'a mut u64,
    pub(in crate::services::discord::turn_bridge) entry_watcher_epoch_current: &'a mut bool,
}

pub(in crate::services::discord::turn_bridge) struct StreamLoopOutput {
    pub(in crate::services::discord::turn_bridge) outcome: StreamLoopOutcome,
    pub(in crate::services::discord::turn_bridge) tui_error_classification: TuiErrorClassification,
    pub(in crate::services::discord::turn_bridge) codex_tui_terminal_range_end: Option<u64>,
    pub(in crate::services::discord::turn_bridge) pending_long_running_open_after_state_save:
        PendingLongRunningOpenAfterStateSave,
    pub(in crate::services::discord::turn_bridge) pending_long_running_retarget_after_state_save:
        PendingLongRunningRetargetAfterStateSave,
}
