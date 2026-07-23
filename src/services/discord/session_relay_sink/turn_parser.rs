use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::cluster::stream_relay::StreamFrame;
use crate::services::provider::ProviderKind;
use crate::services::session_backend::StreamLineState;

use super::super::tmux::{WatcherToolState, process_watcher_lines};
use super::task_notification_context;

pub(in crate::services::discord) struct SessionRelayParser {
    buffer: String,
    stream_state: StreamLineState,
    full_response: String,
    tool_state: WatcherToolState,
    task_notification_kind: Option<TaskNotificationKind>,
    pub(super) task_notification_context:
        Option<super::super::task_notification_delivery::TaskNotificationContext>,
    assistant_text_seen: bool,
    frames_observed: u64,
    last_sequence: u64,
}

impl Default for SessionRelayParser {
    fn default() -> Self {
        Self {
            buffer: String::new(),
            stream_state: StreamLineState::new(),
            full_response: String::new(),
            tool_state: WatcherToolState::new(),
            task_notification_kind: None,
            task_notification_context: None,
            assistant_text_seen: false,
            frames_observed: 0,
            last_sequence: 0,
        }
    }
}

impl SessionRelayParser {
    pub(in crate::services::discord) fn ingest_frame(
        &mut self,
        frame: &StreamFrame,
    ) -> Vec<SessionRelayDelivery> {
        self.frames_observed = self.frames_observed.saturating_add(1);
        self.last_sequence = frame.sequence;
        self.buffer.push_str(&frame.payload);

        let channel_id = match frame.binding.channel_id.parse::<u64>() {
            Ok(channel_id) => channel_id,
            Err(error) => {
                tracing::warn!(
                    channel_id = %frame.binding.channel_id,
                    error = %error,
                    "session-bound relay sink skipped frame with invalid channel id"
                );
                return Vec::new();
            }
        };

        let mut deliveries = Vec::new();
        loop {
            let outcome = process_watcher_lines(
                &mut self.buffer,
                &mut self.stream_state,
                &mut self.full_response,
                &mut self.tool_state,
            );
            if let Some(kind) = outcome.task_notification_kind {
                self.task_notification_kind =
                    task_notification_context::merge_task_notification_kind(
                        self.task_notification_kind,
                        kind,
                    );
            }
            if let Some(context) = outcome.task_notification_context {
                self.task_notification_context =
                    super::super::task_notification_delivery::merge_context(
                        self.task_notification_context.take(),
                        context,
                    );
            }
            self.assistant_text_seen |= outcome.assistant_text_seen;
            if !outcome.found_result {
                break;
            }

            let task_kind_allows_delivery = task_notification_context::allows_delivery(
                self.task_notification_kind,
                self.assistant_text_seen,
            );
            let has_user_visible_response =
                !self.full_response.trim().is_empty() && task_kind_allows_delivery;
            if has_user_visible_response {
                let response_text = std::mem::take(&mut self.full_response);
                let task_notification_kind = self.task_notification_kind.take();
                let task_notification_context = self.task_notification_context.take();

                // The parser owns one turn only until it recognizes that turn's terminal
                // record. Hand the completed response off and clear turn-local state before
                // any asynchronous Discord delivery starts. A replacement relay may enqueue
                // the next turn while the previous POST is still in flight; retaining the
                // completed response until POST completion would seed the next response with
                // the previous turn's prose.
                self.reset_turn();
                deliveries.push(SessionRelayDelivery {
                    provider: frame.binding.provider.clone(),
                    channel_id,
                    session_name: frame.session_name.clone(),
                    response_text,
                    task_notification_kind,
                    task_notification_context,
                    terminal_consumed_end: frame.terminal_consumed_end,
                    frame_turn_user_msg_id: frame.turn_user_msg_id,
                    frame_turn_started_at: frame.turn_started_at.clone(),
                    frame_turn_start_offset: frame.turn_start_offset,
                });
                break;
            } else {
                self.reset_turn();
            }
            if self.buffer.trim().is_empty() {
                break;
            }
        }

        deliveries
    }

    pub(super) fn reset_turn(&mut self) {
        self.stream_state = StreamLineState::new();
        self.full_response.clear();
        self.tool_state = WatcherToolState::new();
        self.task_notification_kind = None;
        self.task_notification_context = None;
        self.assistant_text_seen = false;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct SessionRelayDelivery {
    pub(super) provider: ProviderKind,
    pub(super) channel_id: u64,
    pub(super) session_name: String,
    pub(super) response_text: String,
    pub(super) task_notification_kind: Option<TaskNotificationKind>,
    pub(super) task_notification_context:
        Option<super::super::task_notification_delivery::TaskNotificationContext>,
    pub(super) terminal_consumed_end: Option<u64>,
    pub(super) frame_turn_user_msg_id: u64,
    pub(super) frame_turn_started_at: String,
    pub(super) frame_turn_start_offset: Option<u64>,
}
