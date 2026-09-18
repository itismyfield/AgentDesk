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
    /// #5948 (I18): the exclusive end of the source byte range already folded
    /// into the turn now being accumulated, for frames that name a
    /// [`StreamFrame::source_span`]. `None` means nothing is folded yet.
    ///
    /// Turn-scoped on purpose. `reset_turn` clears it, so the watermark only
    /// ever suppresses bytes inside one UNDELIVERED turn — which is exactly the
    /// rewind damage (`buffer` accumulating the same range twice, producing ONE
    /// delivery whose prose is doubled). A resend that arrives AFTER the turn
    /// was handed off reproduces the same body, not a doubled one, and that is
    /// the retry path the watcher's rewind exists to drive: suppressing it here
    /// would turn a failed POST into a silent loss, so this deliberately does
    /// not. Cross-delivery duplicates stay the send point's job (the delivered
    /// content fingerprint and the committed-range re-gate).
    turn_source_end: Option<u64>,
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
            turn_source_end: None,
        }
    }
}

impl SessionRelayParser {
    pub(super) fn ingest_verified_native_terminal(
        &mut self,
        frame: &StreamFrame,
        response: &str,
    ) -> Vec<SessionRelayDelivery> {
        self.buffer.clear();
        self.reset_turn();
        let mut terminal = frame.clone();
        terminal.payload = format!(
            "{}\n",
            serde_json::json!({"type": "result", "result": response})
        );
        // #5948 (I18): the payload below is SYNTHESISED here, not read from the
        // transcript, so the incoming frame's span does not describe it. Clearing
        // it keeps a synthetic body from seeding the watermark with a range whose
        // bytes were never folded — which would make the next genuine frame in
        // that range look like a replay and drop a real answer.
        terminal.source_span = None;
        self.ingest_frame(&terminal)
    }

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
        self.fold_frame_payload(frame);

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
        self.tool_state.set_provider(&frame.binding.provider);
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
            // A restarted native parser may lack tool calls before its cursor.
            // Only the producer's completed-turn fence (or ordered idle range)
            // can authorize native delivery; the sink replays that source range.
            if frame.terminal_consumed_end.is_none()
                && frame.relay_range.is_none()
                && super::super::tmux::tmux_output_stream::is_native_codex_payload(
                    &frame.binding.provider,
                    &frame.payload,
                )
            {
                self.reset_turn();
                continue;
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

    /// #5948 (I18): append the part of `frame.payload` this parser has not
    /// already folded into the turn it is accumulating, and advance the
    /// watermark.
    ///
    /// A frame without a `source_span` is folded whole — the sink refuses to
    /// guess. Guessing is what makes a dedupe lossy: content equality cannot
    /// separate "the watcher re-read these bytes" from "the model printed the
    /// same sentence twice", and only the second one must survive. Byte offsets
    /// separate them exactly, because repeated prose always occupies a strictly
    /// later range than the prose it repeats.
    fn fold_frame_payload(&mut self, frame: &StreamFrame) {
        if frame.payload.is_empty() {
            return;
        }
        // A span is honoured ONLY when it names exactly as many bytes as the
        // payload carries. That equality is the whole basis for treating the
        // overlap as a byte PREFIX of the payload; a span that disagrees is a
        // coordinate the sink cannot slice a body on, so the payload folds whole.
        // Trusting a disagreeing span in the other direction would let a span
        // sitting entirely behind the watermark erase a real payload.
        let Some((start, end)) = frame
            .source_span
            .filter(|(start, end)| end.saturating_sub(*start) == frame.payload.len() as u64)
        else {
            self.append_buffer_source(frame.payload.len(), frame.relay_source_stamp);
            self.buffer.push_str(&frame.payload);
            return;
        };

        let already_folded = self.turn_source_end.unwrap_or(start).max(start);
        let fresh = if already_folded >= end {
            // Every byte this frame names is already in the turn.
            ""
        } else {
            // `split_at_checked` only declines on a non-char boundary; fold the
            // whole payload then rather than cut a multi-byte character in half.
            let overlap = (already_folded - start) as usize;
            frame
                .payload
                .split_at_checked(overlap)
                .map_or(frame.payload.as_str(), |(_, fresh)| fresh)
        };

        if fresh.len() < frame.payload.len() {
            self.record_resend_suppressed(frame, start, end, frame.payload.len() - fresh.len());
        }
        if !fresh.is_empty() {
            self.append_buffer_source(fresh.len(), frame.relay_source_stamp);
            self.buffer.push_str(fresh);
        }
        self.turn_source_end = Some(self.turn_source_end.unwrap_or(end).max(end));
    }

    /// #5948 (I18): a dropped resend must never be silent — #5941 is the
    /// standing lesson that an unobserved suppression hides its own regression.
    /// `relay_resend_suppressed` is a relay root-cause counter, so it lands in
    /// the restart-safe `observability_events` stream that the hourly #3561
    /// operator alert job aggregates against its threshold.
    fn record_resend_suppressed(
        &self,
        frame: &StreamFrame,
        span_start: u64,
        span_end: u64,
        suppressed_bytes: usize,
    ) {
        tracing::warn!(
            provider = frame.binding.provider.as_str(),
            channel_id = %frame.binding.channel_id,
            tmux_session = %frame.session_name,
            sequence = frame.sequence,
            span_start,
            span_end,
            turn_source_end = ?self.turn_source_end,
            suppressed_bytes,
            "session-bound relay parser dropped resent source bytes from the open turn"
        );
        crate::services::observability::metrics::record_relay_resend_suppressed(
            frame.binding.channel_id.parse::<u64>().unwrap_or(0),
            frame.binding.provider.as_str(),
        );
    }

    /// #5948 (I18): the resend watermark, for tests that pin sink parser state
    /// across the two watcher rewind paths.
    #[cfg(test)]
    pub(in crate::services::discord) fn turn_source_end(&self) -> Option<u64> {
        self.turn_source_end
    }

    /// #5948 (I18): bytes still awaiting a turn terminator, for the same tests.
    #[cfg(test)]
    pub(in crate::services::discord) fn buffered_len(&self) -> usize {
        self.buffer.len()
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
        self.turn_source_end = None;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct SessionRelayDelivery {
    pub(super) provider: ProviderKind,
    pub(super) channel_id: u64,
    pub(super) session_name: String,
    pub(in crate::services::discord) response_text: String,
    pub(super) task_notification_kind: Option<TaskNotificationKind>,
    pub(super) task_notification_context:
        Option<super::super::task_notification_delivery::TaskNotificationContext>,
    pub(super) terminal_consumed_end: Option<u64>,
    pub(super) frame_turn_user_msg_id: u64,
    pub(super) frame_turn_started_at: String,
    pub(super) frame_turn_start_offset: Option<u64>,
    pub(super) relay_range: Option<(u64, u64)>,
    pub(super) relay_generation_mtime_ns: Option<i64>,
    pub(super) relay_source_stamp: Option<SourceStamp>,
}

#[cfg(test)]
mod resend_dedupe_tests;
