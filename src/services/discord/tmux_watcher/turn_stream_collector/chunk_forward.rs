//! Turning one decoded watcher read into a supervisor-relay forward: trim the
//! pre-turn prefix, name the absolute source byte range the remaining bytes came
//! from, and push them as either a terminal or a plain streaming frame. The
//! outer read and the streaming reads differ only in which buffer offsets they
//! carry, so both go through here rather than repeating the fence dispatch.
use super::*;

/// The bytes one decoded read contributes to the current turn, paired with the
/// provenance the sink needs to place them — the read's source authority with
/// the forwarded range folded in (#5948 I17).
pub(in crate::services::discord::tmux::tmux_watcher) struct ForwardedChunk<'a> {
    text: &'a str,
    source_authority: SupervisorFrameSourceAuthority,
}

/// #5948 (I17): name the absolute JSONL byte range the forwarded bytes came
/// from. `all_data` ends at `buffer_start_offset + buffer_len`, and the
/// forwarded text is a SUFFIX of the decoded chunk (the pre-turn skip only trims
/// a prefix), so the forwarded bytes end there too. A rewind re-reads from the
/// turn's data start and hands the sink these same offsets under a fresh relay
/// `sequence`; the offsets are what let the sink tell replay from genuinely new
/// output.
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
        // #3041 P1-3 (codex P1-3 issue 1): a single physical chunk may carry turn
        // A's result PLUS turn B's first bytes. `all_data` after the parse holds
        // turn B's leftover; split the decoded chunk at that boundary so the
        // TERMINAL frame carries only turn A's bytes and turn B's tail rides a
        // separate non-terminal frame (no black-hole, no shared-ACK reuse).
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
