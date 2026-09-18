//! Turning one decoded watcher read into a supervisor-relay forward: trim the
//! pre-turn prefix, name the source byte range the rest came from, and push it
//! as either a terminal or a plain streaming frame.
use super::*;

/// The bytes one decoded read contributes to the current turn, paired with the
/// read's source authority carrying the forwarded range (#5948 I18).
pub(in crate::services::discord::tmux::tmux_watcher) struct ForwardedChunk<'a> {
    text: &'a str,
    source_authority: SupervisorFrameSourceAuthority,
}

/// #5948 (I18): `all_data` ends at `buffer_start_offset + buffer_len`, and the
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

#[cfg(test)]
mod tests {
    use super::super::super::loop_poll_prologue::WatcherSourceAuthority;
    use super::super::super::utf8_chunk_decoder::Utf8ChunkDecoder;
    use super::*;
    use crate::services::cluster::session_matcher::{MatchedChannel, expected_rollout_path_for};
    use crate::services::cluster::stream_relay::{SourceFileIdentity, StreamFrame};
    use crate::services::discord::session_relay_sink::turn_parser::SessionRelayParser;
    use crate::services::observability::metrics;
    use crate::services::provider::ProviderKind;

    fn authority() -> WatcherSourceAuthority {
        WatcherSourceAuthority {
            source_file: SourceFileIdentity::Unavailable,
            generation_mtime_ns: 0,
            reset_incarnation: 0,
            source_stamp: None,
        }
    }

    fn binding(channel: &str) -> MatchedChannel {
        let session = ProviderKind::Claude.build_tmux_session_name(channel);
        MatchedChannel {
            channel_id: channel.to_string(),
            agent_id: format!("agent-{channel}"),
            provider: ProviderKind::Claude,
            expected_session_name: session.clone(),
            expected_rollout_path: expected_rollout_path_for(&session),
        }
    }

    /// The frame `supervisor_relay` mints from a forwarded chunk.
    fn frame(binding: &MatchedChannel, chunk: &ForwardedChunk<'_>, sequence: u64) -> StreamFrame {
        StreamFrame {
            session_name: binding.expected_session_name.clone(),
            binding: binding.clone(),
            payload: chunk.text.to_string(),
            sequence,
            terminal_consumed_end: None,
            turn_user_msg_id: 0,
            turn_started_at: String::new(),
            turn_start_offset: None,
            relay_range: None,
            relay_generation_mtime_ns: None,
            relay_source_stamp: None,
            source_span: chunk.source_authority.source_span,
        }
    }

    fn assistant(text: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({
                "type": "assistant",
                "message": {"content": [{"type": "text", "text": text}]}
            })
        )
    }

    fn suppressed_for(channel_id: u64) -> u64 {
        metrics::global()
            .snapshot()
            .into_iter()
            .find(|row| row.channel_id == channel_id)
            .map_or(0, |row| row.relay_resend_suppressed)
    }

    /// `all_data` holds 4 bytes from an earlier read plus this 6-byte read,
    /// anchored at 1000; 5 pre-turn bytes are skipped, 4 of them from the
    /// earlier read, so the forwarded suffix is `[1005, 1010)`.
    #[test]
    fn forwarded_span_names_the_suffix_the_buffer_ends_with() {
        let chunk = forwarded_chunk("abcdef", 10, 1_000, 5, authority());
        assert_eq!(chunk.text, "bcdef");
        assert_eq!(chunk.source_authority.source_span, Some((1_005, 1_010)));

        let empty = forwarded_chunk("abcdef", 10, 1_000, 10, authority());
        assert_eq!(empty.text, "");
        assert_eq!(empty.source_authority.source_span, None);
    }

    /// #5979 (I18): the terminal-delivery rewind in `tmux_watcher.rs` resets
    /// `current_offset` and empties `all_data` but never the decoder. With a
    /// split scalar buffered from the abandoned read, the replay must still be
    /// anchored at the rewind offset so its span is in FILE coordinates and the
    /// sink recognises the already-folded prefix.
    #[test]
    fn rewind_replay_keeps_file_coordinates_and_the_sink_suppresses_the_resend() {
        let binding = binding("597901");
        let before = suppressed_for(597_901);
        let head = assistant("alpha");
        let tail = format!(
            "{}{}\n",
            assistant("안녕"),
            serde_json::json!({"type": "result", "result": "done"})
        );
        let transcript = format!("{head}{tail}");
        let bytes = transcript.as_bytes();
        let cut = head.len() + tail.find('안').expect("korean text") + 1;
        let base = 4_096u64;

        let mut decoder = Utf8ChunkDecoder::default();
        let mut parser = SessionRelayParser::default();
        let mut all_data = String::new();

        // Read 1 ends one byte into `안`; the collector anchors the empty buffer
        // at the decoded start.
        let first = decoder.decode_source_for_buffer(&bytes[..cut], base, authority(), &all_data);
        let anchor = first.start_offset.expect("decoded text");
        all_data.push_str(&first.text);
        let first_len = all_data.len() as u64;
        let chunk = forwarded_chunk(&first.text, all_data.len(), anchor, 0, authority());
        assert_eq!(
            chunk.source_authority.source_span,
            Some((base, base + first_len))
        );
        assert!(parser.ingest_frame(&frame(&binding, &chunk, 1)).is_empty());
        assert_eq!(parser.turn_source_end(), Some(base + first_len));

        // Rewind: cursor back to `base`, buffer emptied, decoder untouched; the
        // collector's refill of the empty buffer is what drops the stale tail.
        all_data.clear();
        let replay = decoder.decode_source_for_buffer(bytes, base, authority(), &all_data);
        let anchor = replay.start_offset.expect("decoded text");
        assert_eq!(
            anchor, base,
            "the abandoned read's tail re-anchored the replay"
        );
        all_data.push_str(&replay.text);
        let chunk = forwarded_chunk(&replay.text, all_data.len(), anchor, 0, authority());
        assert_eq!(
            chunk.source_authority.source_span,
            Some((base, base + transcript.len() as u64))
        );

        let deliveries = parser.ingest_frame(&frame(&binding, &chunk, 2));
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].response_text.matches("alpha").count(), 1);
        assert_eq!(deliveries[0].response_text.matches("안녕").count(), 1);
        assert_eq!(suppressed_for(597_901) - before, 1);
    }
}
