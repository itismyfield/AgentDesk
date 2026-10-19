//! Bridge admission re-reads a Herdr Codex turn with the tail's own record acceptance.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use super::{
    NextRecord, RecordAcceptance, RolloutParseState, herdr_terminal_kind,
    promote_own_task_complete_fallback_text,
};
use crate::services::agent_protocol::NativeTerminalKind;
use crate::services::claude::herdr_turn::EndedTurn;
use crate::services::provider::CancelToken;

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
        let mut state = replay(file, (start, len), actor, false)?;
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

    /// The same replay once the writer is gone: a final record without its newline counts when
    /// it parses whole, and is no record otherwise. `None` when the file cannot be read.
    pub(crate) fn replay_ended_herdr_turn(
        file: &mut std::fs::File,
        start: u64,
        len: u64,
        actor: &Arc<CancelToken>,
    ) -> Option<EndedTurn> {
        let state = replay(file, (start, len), actor, true)?;
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
    ended: bool,
) -> Option<RolloutParseState> {
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(start)).ok()?;
    file.take(len.checked_sub(start)?)
        .read_to_end(&mut bytes)
        .ok()?;
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
    // The tail flushes a final record that has no newline yet the same way.
    let mut partial = rest.to_vec();
    let whole = serde_json::from_slice::<serde_json::Value>(&partial).is_ok();
    if ended && whole {
        partial.push(b'\n');
    } else if ended && !partial.trim_ascii().is_empty() && mutant("prb_torn_tail_unobserved") {
        return None;
    }
    super::super::try_process_complete_partial_line(&mut partial, &sender, &mut state, &accept);
    Some(state)
}

fn mutant(name: &str) -> bool {
    crate::services::provider::herdr_before_start::mutant(name)
}
