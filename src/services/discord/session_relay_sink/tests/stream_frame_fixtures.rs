//! `StreamFrame` builders for the sink's tests: a plain streaming frame, the
//! idle/catch-up variant that names a `relay_range`, and the terminal variant
//! that carries a commit fence.

use super::*;

pub(super) fn frame(binding: &MatchedChannel, payload: &str, sequence: u64) -> StreamFrame {
    StreamFrame {
        session_name: binding.expected_session_name.clone(),
        binding: binding.clone(),
        payload: payload.to_string(),
        sequence,
        terminal_consumed_end: None,
        turn_user_msg_id: 0,
        turn_started_at: String::new(),
        turn_start_offset: None,
        relay_range: None,
        relay_generation_mtime_ns: None,
        relay_source_stamp: None,
        source_span: None,
    }
}

pub(super) fn ranged_frame(
    binding: &MatchedChannel,
    payload: &str,
    sequence: u64,
    range_start: u64,
    range_end: u64,
) -> StreamFrame {
    let mut frame = frame(binding, payload, sequence);
    frame.relay_range = Some((range_start, range_end));
    frame.relay_generation_mtime_ns = Some(dr::current_generation_mtime_ns(
        &binding.expected_session_name,
    ));
    frame
}

pub(super) fn terminal_frame(
    binding: &MatchedChannel,
    payload: &str,
    sequence: u64,
    consumed_end: u64,
    turn_user_msg_id: u64,
    turn_started_at: &str,
) -> StreamFrame {
    terminal_frame_offset(
        binding,
        payload,
        sequence,
        consumed_end,
        turn_user_msg_id,
        turn_started_at,
        Some(0),
    )
}
