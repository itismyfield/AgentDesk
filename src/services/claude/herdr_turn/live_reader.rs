//! The live Herdr reader commits its timestamp boundary before streaming the turn.

use super::*;

/// Under settlement the transcript is read to this turn's own turn-end record, returned unsent;
/// offsets advance only over processed records. Idleness, EOF or a dead pane end no turn, but a
/// terminal its gone writer left without a newline (`ended`) is read and ends where the file does.
pub(super) fn read_to_provider_terminal(
    transcript: &str,
    start: u64,
    sender: &Sender<StreamMessage>,
    token: &Arc<CancelToken>,
    ended: impl Fn((u64, SourceFileIdentity)) -> Option<String>,
) -> Result<Option<ClaudeTurnTerminal>, String> {
    let Some(start) = start_offset::wait(transcript, start, token, &ended)? else {
        return Ok(None);
    };
    let mut lines = HerdrTurnLines::default();
    let opened = std::cell::Cell::new(SourceFileIdentity::Unavailable);
    let tail = std::cell::RefCell::new(None);
    // A buffer's raw end is sent only once a later read shows all its records were processed.
    let pending = std::cell::Cell::new(None::<u64>);
    let (offsets, relay) = (sender.clone(), sender.clone());
    let (local, local_rx) = std::sync::mpsc::channel();
    let mut process = |line: &str, lines: &mut HerdrTurnLines| {
        let processed = lines.process(line, &local);
        // The turn's Done is the typed frame sent after the read, never the stream's own.
        for message in local_rx.try_iter() {
            if !matches!(message, StreamMessage::Done { .. }) {
                let _ = relay.send(message);
            }
        }
        processed
    };
    let mut read = poll_output_file_until_result(
        transcript,
        start,
        Some(token.clone()),
        &mut lines,
        || {
            let at = (pending.get().unwrap_or(start), opened.get());
            let mut tail = tail.borrow_mut();
            *tail = ended(at).map(|found| (at.0, found));
            tail.is_none()
        },
        || false,
        |raw_end| {
            if let Some(processed) = pending.replace(Some(raw_end)) {
                let _ = offsets.send(StreamMessage::OutputOffset { offset: processed });
            }
        },
        &mut process,
        |lines| lines.terminal().is_some(),
        |_| false,
        |_| {},
        |file| opened.set(SourceFileIdentity::from_open_file(file)),
    )
    .map_err(|failure| failure.error)?;
    if let ReadOutputResult::SessionDied { offset } = read
        && let Some((from, tail)) = tail.take()
        && offset == from
    {
        process(&tail, &mut lines);
        read = ReadOutputResult::Completed {
            offset: offset + tail.len() as u64,
        };
    }
    let end = match read {
        ReadOutputResult::Completed { offset }
        | ReadOutputResult::Cancelled { offset }
        | ReadOutputResult::SessionDied { offset } => offset,
    };
    if let Some(raw_end) = pending.get() {
        let _ = sender.send(StreamMessage::OutputOffset {
            offset: raw_end.min(end),
        });
    }
    let (ReadOutputResult::Completed { .. }, Some(kind)) = (read, lines.terminal()) else {
        return Ok(None);
    };
    let (result, session_id) = lines.result();
    let file = crate::services::provider::cancel_token_claude_interrupt::opened_file_identity(
        Some(&opened.get()),
    );
    Ok(Some(ClaudeTurnTerminal {
        start,
        kind,
        end,
        result,
        session_id,
        file,
    }))
}
