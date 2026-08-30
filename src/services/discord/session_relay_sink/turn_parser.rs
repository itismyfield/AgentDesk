use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::cluster::stream_relay::{SourceStamp, StreamFrame};
use crate::services::provider::ProviderKind;
use crate::services::session_backend::StreamLineState;
use std::collections::VecDeque;

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
    source_generation_mtime_ns: Option<i64>,
    buffer_source_segments: VecDeque<(usize, Option<SourceStamp>)>,
    relay_source_stamp: Option<Option<SourceStamp>>,
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
            source_generation_mtime_ns: None,
            buffer_source_segments: VecDeque::new(),
            relay_source_stamp: None,
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
        if let Some(generation) = frame.relay_generation_mtime_ns {
            if self
                .source_generation_mtime_ns
                .is_some_and(|current| current != generation)
            {
                self.buffer.clear();
                self.reset_turn();
            }
            self.source_generation_mtime_ns = Some(generation);
        }
        if !frame.payload.is_empty() {
            self.append_buffer_source(frame.payload.len(), frame.relay_source_stamp);
            self.buffer.push_str(&frame.payload);
        }

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
            let buffer_len_before = self.buffer.len();
            let response_before = self.full_response.clone();
            let outcome = process_watcher_lines(
                &mut self.buffer,
                &mut self.stream_state,
                &mut self.full_response,
                &mut self.tool_state,
            );
            let consumed_source_stamp =
                self.drain_buffer_source_prefix(buffer_len_before - self.buffer.len());
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
            if !self.full_response.is_empty() && self.full_response != response_before {
                Self::merge_source_stamp(&mut self.relay_source_stamp, consumed_source_stamp);
            }
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
                let source_generation_mtime_ns = self.source_generation_mtime_ns;
                let relay_source_stamp = self.relay_source_stamp.flatten();
                self.reset_turn();
                deliveries.push(SessionRelayDelivery {
                    provider: frame.binding.provider.clone(),
                    channel_id,
                    session_name: frame.session_name.clone(),
                    response_text,
                    task_notification_kind,
                    task_notification_context,
                    terminal_consumed_end: frame.terminal_consumed_end,
                    terminal_consumed_start: frame.terminal_consumed_start,
                    frame_turn_user_msg_id: frame.turn_user_msg_id,
                    frame_turn_started_at: frame.turn_started_at.clone(),
                    frame_turn_start_offset: frame.turn_start_offset,
                    relay_range: frame.relay_range,
                    relay_generation_mtime_ns: source_generation_mtime_ns,
                    relay_source_stamp,
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

    fn append_buffer_source(&mut self, len: usize, stamp: Option<SourceStamp>) {
        self.buffer_source_segments.push_back((len, stamp));
    }

    fn drain_buffer_source_prefix(&mut self, mut len: usize) -> Option<Option<SourceStamp>> {
        let mut aggregate = None;
        while len > 0 {
            let (segment_len, stamp) = self.buffer_source_segments.pop_front()?;
            let consumed = len.min(segment_len);
            Self::merge_source_stamp(&mut aggregate, Some(stamp));
            len -= consumed;
            if consumed < segment_len {
                self.buffer_source_segments
                    .push_front((segment_len - consumed, stamp));
            }
        }
        Some(aggregate.flatten())
    }

    fn merge_source_stamp(
        aggregate: &mut Option<Option<SourceStamp>>,
        contribution: Option<Option<SourceStamp>>,
    ) {
        if let Some(contribution) = contribution {
            *aggregate = Some(match (*aggregate, contribution) {
                (None, stamp) => stamp,
                (Some(Some(left)), Some(right)) if left == right => Some(left),
                _ => None,
            });
        }
    }

    pub(super) fn reset_turn(&mut self) {
        self.stream_state = StreamLineState::new();
        self.full_response.clear();
        self.tool_state = WatcherToolState::new();
        self.task_notification_kind = None;
        self.task_notification_context = None;
        self.assistant_text_seen = false;
        if self.buffer.is_empty() {
            self.buffer_source_segments.clear();
        }
        self.relay_source_stamp = None;
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
    pub(super) terminal_consumed_start: Option<u64>,
    pub(super) frame_turn_user_msg_id: u64,
    pub(super) frame_turn_started_at: String,
    pub(super) frame_turn_start_offset: Option<u64>,
    pub(super) relay_range: Option<(u64, u64)>,
    pub(super) relay_generation_mtime_ns: Option<i64>,
    pub(super) relay_source_stamp: Option<SourceStamp>,
}
