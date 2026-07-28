//! Task-notification tool arm and its child-session lifecycle updates.

use crate::services::agent_protocol::TaskNotificationKind;

use super::super::*;

pub(super) struct StreamTaskNotificationContext<'a> {
    pub(super) shared_owned: &'a SharedData,
    pub(super) channel_id: ChannelId,
    pub(super) single_message_panel_footer_mode: bool,
    pub(super) footer_owner:
        super::super::super::super::footer_view_reconciler::CompletionFooterOwner,
    pub(super) inflight_state: &'a mut InflightTurnState,
    pub(super) state_dirty: &'a mut bool,
    pub(super) status_panel_dirty: &'a mut bool,
    pub(super) spin_idx: &'a mut usize,
    pub(super) active_background_child_session_ids: &'a mut Vec<i64>,
    pub(super) transcript_events: &'a mut Vec<SessionTranscriptEvent>,
}

pub(super) async fn handle_stream_task_notification(
    tool_use_id: Option<String>,
    summary: String,
    status: String,
    kind: TaskNotificationKind,
    context: StreamTaskNotificationContext<'_>,
) {
    context.inflight_state.task_notification_kind =
        merge_task_notification_kind(context.inflight_state.task_notification_kind, kind);
    *context.state_dirty = true;
    record_placeholder_live_event(
        context.shared_owned,
        context.channel_id,
        super::super::super::super::placeholder_live_events::RecentPlaceholderEvent::task_notification(
            kind.as_str(),
            &status,
            &summary,
        ),
    );
    *context.status_panel_dirty |= record_status_panel_events(
        context.shared_owned,
        context.channel_id,
        super::super::super::super::placeholder_live_events::status_events_from_task_notification_with_tool_use_id(
            kind.as_str(),
            &status,
            &summary,
            tool_use_id.as_deref(),
        ),
    );
    if context.single_message_panel_footer_mode {
        let indicator =
            super::super::super::super::single_message_panel::single_message_panel_spinner_frame(
                *context.spin_idx,
            );
        *context.spin_idx = context.spin_idx.wrapping_add(1);
        refresh_bridge_footer(
            context.shared_owned,
            context.channel_id,
            context.footer_owner,
            indicator,
        )
        .await;
    }
    if task_notification_closes_background_child(kind, &status) {
        let close_status = if matches!(
            status.trim().to_ascii_lowercase().as_str(),
            "aborted" | "cancelled" | "canceled" | "failed" | "error"
        ) {
            "aborted"
        } else {
            "completed"
        };
        close_next_tracked_background_child(
            context.shared_owned.pg_pool.as_ref(),
            context.active_background_child_session_ids,
            close_status,
            "task notification",
        )
        .await;
        // #1670: `merge_task_notification_kind` is an absorb operator
        // (priority-max). Only release the closed child's kind once all
        // tracked children have closed; otherwise retain the absorbed kind.
        if context.active_background_child_session_ids.is_empty() {
            context.inflight_state.task_notification_kind =
                release_task_notification_kind(context.inflight_state.task_notification_kind, kind);
        }
    }
    push_transcript_event(
        context.transcript_events,
        SessionTranscriptEvent {
            kind: SessionTranscriptEventKind::Task,
            tool_name: None,
            summary: Some(summary.clone()),
            content: summary,
            status: Some("info".to_string()),
            is_error: false,
        },
    );
}
