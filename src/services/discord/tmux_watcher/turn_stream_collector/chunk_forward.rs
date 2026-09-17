//! Turning one decoded watcher read into a supervisor-relay forward: trim the
//! pre-turn prefix, name the source byte range the rest came from, and push it
//! as either a terminal or a plain streaming frame.
use super::*;

/// The bytes one decoded read contributes to the current turn, paired with the
/// read's source authority carrying the forwarded range (#5948 I17).
pub(in crate::services::discord::tmux::tmux_watcher) struct ForwardedChunk<'a> {
    text: &'a str,
    source_authority: SupervisorFrameSourceAuthority,
}

/// #5948 (I17): `all_data` ends at `buffer_start_offset + buffer_len`, and the
/// forwarded text is a SUFFIX of the decoded chunk, so the forwarded bytes end
/// there too. A rewind replays those same offsets under a fresh relay
/// `sequence`, which is what lets the sink tell replay from genuinely new output.
pub(in crate::services::discord::tmux::tmux_watcher) fn forwarded_chunk<'a>(
    decoded_text: &'a str,
    buffer_len: usize,
    buffer_start_offset: u64,
    pre_turn_bytes_skipped: usize,
    source_authority: impl Into<SupervisorFrameSourceAuthority>,
) -> ForwardedChunk<'a> {
    let text = watcher_forward_text_after_pre_turn_skip(
        decoded_text,
        buffer_len.saturating_sub(decoded_text.len()),
        pre_turn_bytes_skipped,
    );
    let source_span = (!text.is_empty()).then(|| {
        let end = buffer_start_offset.saturating_add(buffer_len as u64);
        (end.saturating_sub(text.len() as u64), end)
    });
    ForwardedChunk {
        text,
        source_authority: source_authority_with_span(source_authority, source_span),
    }
}

/// Forward `chunk` for the turn `turn_identity` pins, as a TERMINAL frame when
/// `terminal` names a commit fence and as a plain streaming frame otherwise.
pub(in crate::services::discord::tmux::tmux_watcher) fn forward_turn_chunk_to_supervisor_relay(
    tmux_session_name: &str,
    chunk: &ForwardedChunk<'_>,
    leftover_len: usize,
    registry: &Arc<RelayProducerRegistry>,
    cached_producer: &mut Option<RelayProducer>,
    turn_identity: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
    terminal: Option<crate::services::cluster::stream_relay::TerminalCommitFence>,
) -> SupervisorRelayForward {
    match terminal {
        // #3041 P1-3 (codex P1-3 issue 1): one physical chunk may carry turn A's
        // result PLUS turn B's first bytes, so split at the leftover boundary and
        // let turn B's tail ride a separate non-terminal frame (no black-hole).
        Some(fence) => forward_terminal_chunk_with_trailing_to_supervisor_relay(
            tmux_session_name,
            chunk.text,
            leftover_len,
            registry,
            cached_producer,
            fence,
            chunk.source_authority,
        ),
        None => forward_chunk_to_supervisor_relay_for_turn(
            tmux_session_name,
            chunk.text,
            registry,
            cached_producer,
            turn_identity,
            chunk.source_authority,
        ),
    }
}
