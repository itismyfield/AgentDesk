use std::sync::Arc;

use crate::services::agent_protocol::TaskNotificationKind;

use super::super::super::stream_tick::{
    LongRunningPlaceholderActive, PendingLongRunningOpenAfterStateSave,
    PendingLongRunningRetargetAfterStateSave,
};
use super::super::super::*;

pub(in crate::services::discord::turn_bridge::stream_loop) enum StreamToolArmMessage {
    ToolUse {
        name: String,
        input: String,
        tool_use_id: Option<String>,
    },
    ToolResult {
        content: String,
        is_error: bool,
        tool_use_id: Option<String>,
    },
    TaskNotification {
        tool_use_id: Option<String>,
        summary: String,
        status: String,
        kind: TaskNotificationKind,
    },
}

pub(in crate::services::discord::turn_bridge::stream_loop) struct StreamToolArmContext<'a> {
    pub(in crate::services::discord::turn_bridge::stream_loop) shared_owned: &'a Arc<SharedData>,
    pub(in crate::services::discord::turn_bridge::stream_loop) gateway: &'a Arc<dyn TurnGateway>,
    pub(in crate::services::discord::turn_bridge::stream_loop) channel_id: ChannelId,
    pub(in crate::services::discord::turn_bridge::stream_loop) provider: &'a ProviderKind,
    pub(in crate::services::discord::turn_bridge::stream_loop) user_text_owned: &'a String,
    pub(in crate::services::discord::turn_bridge::stream_loop) request_owner_name: &'a str,
    pub(in crate::services::discord::turn_bridge::stream_loop) adk_session_key: &'a Option<String>,
    pub(in crate::services::discord::turn_bridge::stream_loop) adk_session_name: &'a Option<String>,
    pub(in crate::services::discord::turn_bridge::stream_loop) role_binding:
        &'a Option<RoleBinding>,
    pub(in crate::services::discord::turn_bridge::stream_loop) voice_progress_playback_channel_id:
        Option<ChannelId>,
    pub(in crate::services::discord::turn_bridge::stream_loop) single_message_panel_footer_mode:
        bool,
    pub(in crate::services::discord::turn_bridge::stream_loop) footer_owner:
        crate::services::discord::footer_view_reconciler::CompletionFooterOwner,
    pub(in crate::services::discord::turn_bridge::stream_loop) current_msg_id: MessageId,
}

pub(in crate::services::discord::turn_bridge::stream_loop) struct StreamToolArmState<'a> {
    pub(in crate::services::discord::turn_bridge::stream_loop) state_dirty: &'a mut bool,
    pub(in crate::services::discord::turn_bridge::stream_loop) inflight_state:
        &'a mut InflightTurnState,
    pub(in crate::services::discord::turn_bridge::stream_loop) persisted_inflight_baseline:
        &'a mut InflightTurnState,
    pub(in crate::services::discord::turn_bridge::stream_loop) stream_tick_expected_identity:
        &'a crate::services::discord::inflight::InflightTurnIdentity,
    pub(in crate::services::discord::turn_bridge::stream_loop) expected_current_message:
        &'a mut (u64, usize),
    pub(in crate::services::discord::turn_bridge::stream_loop) current_tool_line:
        &'a mut Option<String>,
    pub(in crate::services::discord::turn_bridge::stream_loop) prev_tool_status:
        &'a mut Option<String>,
    pub(in crate::services::discord::turn_bridge::stream_loop) last_tool_name:
        &'a mut Option<String>,
    pub(in crate::services::discord::turn_bridge::stream_loop) last_tool_summary:
        &'a mut Option<String>,
    pub(in crate::services::discord::turn_bridge::stream_loop) any_tool_used: &'a mut bool,
    pub(in crate::services::discord::turn_bridge::stream_loop) has_post_tool_text: &'a mut bool,
    pub(in crate::services::discord::turn_bridge::stream_loop) last_assistant_text_line:
        &'a mut Option<String>,
    pub(in crate::services::discord::turn_bridge::stream_loop) spin_idx: &'a mut usize,
    pub(in crate::services::discord::turn_bridge::stream_loop) transcript_events:
        &'a mut Vec<SessionTranscriptEvent>,
    pub(in crate::services::discord::turn_bridge::stream_loop) pending_status_tool_results:
        &'a mut VecDeque<String>,
    pub(in crate::services::discord::turn_bridge::stream_loop) pending_status_tool_results_by_id:
        &'a mut std::collections::HashMap<String, String>,
    pub(in crate::services::discord::turn_bridge::stream_loop) long_running_placeholder_active:
        &'a mut LongRunningPlaceholderActive,
    pub(in crate::services::discord::turn_bridge::stream_loop) active_background_child_session_ids:
        &'a mut Vec<i64>,
    pub(in crate::services::discord::turn_bridge::stream_loop) pending_long_running_open_after_state_save:
        &'a mut PendingLongRunningOpenAfterStateSave,
    pub(in crate::services::discord::turn_bridge::stream_loop) pending_long_running_retarget_after_state_save:
        &'a mut PendingLongRunningRetargetAfterStateSave,
    pub(in crate::services::discord::turn_bridge::stream_loop) restart_followup_pending:
        &'a mut bool,
    pub(in crate::services::discord::turn_bridge::stream_loop) last_edit_text: &'a mut String,
    pub(in crate::services::discord::turn_bridge::stream_loop) full_response: &'a mut String,
    pub(in crate::services::discord::turn_bridge::stream_loop) status_panel_dirty: &'a mut bool,
}
