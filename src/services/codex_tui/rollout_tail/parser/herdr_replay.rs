//! Bridge admission re-reads a Herdr Codex turn with the tail's own record acceptance.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use super::{
    NextRecord, RecordAcceptance, RolloutParseState, herdr_terminal_kind,
    promote_own_task_complete_fallback_text,
};
use crate::services::agent_protocol::NativeTerminalKind;
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
        super::super::try_process_complete_partial_line(&mut partial, &sender, &mut state, &accept);
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
}
