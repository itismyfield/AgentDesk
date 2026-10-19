//! Bridge admission and the row pass re-read a Herdr Codex turn under the tail's acceptance.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use super::{
    NextRecord, RecordAcceptance, RelaySuppressionSender, RolloutParseState, herdr_terminal_kind,
    promote_own_task_complete_fallback_text,
};
use crate::services::agent_protocol::NativeTerminalKind;
use crate::services::claude::herdr_turn::EndedTurn;
use crate::services::provider::CancelToken;
use crate::services::provider::herdr_before_start::mutant;

/// A turn's first provider terminal as the live tail accepts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplayedTerminal {
    pub(crate) kind: NativeTerminalKind,
    pub(crate) end: u64,
    pub(crate) result: String,
}

impl super::RolloutRecordDecoder {
    /// Replays `file` from the turn's own `start` up to `len` under the tail's acceptance policy;
    /// `None` when no record before `len` ends the turn.
    pub(crate) fn replay_herdr_turn(
        file: &mut std::fs::File,
        start: u64,
        len: u64,
        actor: &Arc<CancelToken>,
    ) -> Option<ReplayedTerminal> {
        let mut state = replay(file, (start, len), actor)?;
        let kind = herdr_terminal_kind(&state)?;
        if kind == NativeTerminalKind::Completed {
            promote_own_task_complete_fallback_text(&mut state);
        }
        Some(ReplayedTerminal {
            kind,
            end: start.saturating_add(state.bytes_read),
            result: state.final_text,
        })
    }

    /// The same replay read for what ended the turn. `None` when the file cannot be read.
    pub(crate) fn replay_ended_herdr_turn(
        file: &mut std::fs::File,
        start: u64,
        len: u64,
        actor: &Arc<CancelToken>,
    ) -> Option<EndedTurn> {
        let state = replay(file, (start, len), actor)?;
        Some(match herdr_terminal_kind(&state) {
            Some(_) => EndedTurn::Terminal,
            None if state.mixed_turn => EndedTurn::Mixed,
            None => EndedTurn::NoTerminal,
        })
    }
}

fn replay(
    file: &mut std::fs::File,
    (start, len): (u64, u64),
    actor: &Arc<CancelToken>,
) -> Option<RolloutParseState> {
    let (mut bytes, want) = (Vec::new(), len.checked_sub(start)?);
    file.seek(SeekFrom::Start(start)).ok()?;
    file.take(want).read_to_end(&mut bytes).ok()?;
    // A file cut after its length was read reads short, and a short read is no read.
    if bytes.len() as u64 != want && !mutant("prb_short_read_ok") {
        return None;
    }
    let (tx, _rx) = std::sync::mpsc::channel();
    let sender = super::super::RelaySuppressionSender::new(&tx, None);
    let accept = RecordAcceptance::new(Some(actor));
    let mut state = RolloutParseState::default();
    let mut rest = bytes.as_slice();
    while let Some(pos) = rest.iter().position(|byte| *byte == b'\n') {
        if accept.accept_next_record(&state) == NextRecord::StopBefore {
            break;
        }
        let (line, tail) = rest.split_at(pos + 1);
        rest = tail;
        state.record(line.len());
        super::process_rollout_line_bytes(line, &sender, &mut state);
    }
    let mut partial = rest.to_vec();
    if mutant("prb_codex_tail_end_past_file") && whole(&partial) {
        partial.push(b'\n');
        super::super::try_process_complete_partial_line(&mut partial, &sender, &mut state, &accept);
    }
    take_whole_tail(&mut partial, (&sender, &mut state), &accept, || true);
    Some(state)
}

/// A final record without its newline, once `gone` says its writer is gone, counts as the tail's
/// last record when it parses whole; it ends where the file does.
pub(super) fn take_whole_tail(
    partial: &mut Vec<u8>,
    (sender, state): (&RelaySuppressionSender<'_>, &mut RolloutParseState),
    accept: &RecordAcceptance<'_>,
    gone: impl FnOnce() -> bool,
) -> bool {
    if !whole(partial) || accept.accept_next_record(state) == NextRecord::StopBefore || !gone() {
        return false;
    }
    let line = std::mem::take(partial);
    state.record(line.len());
    super::process_rollout_line_bytes(&line, sender, state);
    true
}

fn whole(partial: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(partial).is_ok()
}
