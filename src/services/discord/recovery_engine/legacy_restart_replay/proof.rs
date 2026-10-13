//! Strict raw proof for a legacy restart replay. It opens the rollout once, verifies the stored
//! session and native turn in that open file, and aligns the stored composed response with the
//! turn's decoded prefix. The open reader, decoder and residual text continue the same turn.
//!
//! EOF, pane readiness and raw markers are not terminal evidence here, and no record is
//! skipped: a partial or malformed record stops the proof instead of moving a cursor past it.

use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::Value;

use super::CoordinateHold;
use crate::services::agent_protocol::StreamMessage;
use crate::services::cluster::stream_relay::SourceFileIdentity;
use crate::services::codex_tui::rollout_tail::RolloutRecordDecoder;
use crate::services::discord::inflight::RawTurnWitness;

/// The durable facts a proof must find in the source; `composed` is the stored FROZEN + BODY.
pub(in crate::services::discord::recovery_engine) struct ProofRequest<'a> {
    pub(in crate::services::discord::recovery_engine) raw_path: &'a Path,
    pub(in crate::services::discord::recovery_engine) session_id: Option<&'a str>,
    pub(in crate::services::discord::recovery_engine) native_turn_id: Option<&'a str>,
    pub(in crate::services::discord::recovery_engine) composed: &'a str,
}

/// A verified alignment. `reader` sits right after the record that crossed the prefix and
/// `residual_text` is the part of that record's text beyond the prefix.
pub(in crate::services::discord::recovery_engine) struct ProofPlan {
    pub(in crate::services::discord::recovery_engine) witness: RawTurnWitness,
    pub(in crate::services::discord::recovery_engine) reader: BufReader<std::fs::File>,
    pub(in crate::services::discord::recovery_engine) decoder: RolloutRecordDecoder,
    pub(in crate::services::discord::recovery_engine) residual_text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TurnScan {
    Before,
    /// Inside the selected turn; `opener_run` while only its opening records were read.
    Selected {
        opener_run: bool,
    },
}

/// The turn a `task_started` or `turn_context` record opens: `Some(None)` when it names none.
/// Same selection rule as the idle transcript scan's prompt resolver.
fn opened_turn(record: &Value) -> Option<Option<&str>> {
    let opens = record.pointer("/payload/type").and_then(Value::as_str) == Some("task_started")
        || record.get("type").and_then(Value::as_str) == Some("turn_context");
    opens.then(|| record.pointer("/payload/turn_id").and_then(Value::as_str))
}

fn named(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

pub(in crate::services::discord::recovery_engine) fn prove(
    request: &ProofRequest<'_>,
) -> Result<ProofPlan, CoordinateHold> {
    use CoordinateHold::{AwaitEvidence, NeedsEvidence};
    let session_id = named(request.session_id).ok_or(NeedsEvidence("session_unknown"))?;
    let native_turn = named(request.native_turn_id).ok_or(NeedsEvidence("native_turn_unknown"))?;
    let file = match std::fs::File::open(request.raw_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(AwaitEvidence("source_missing"));
        }
        Err(_) => return Err(AwaitEvidence("source_unreadable")),
    };
    let metadata = file
        .metadata()
        .map_err(|_| AwaitEvidence("source_unreadable"))?;
    if !metadata.is_file() {
        return Err(NeedsEvidence("source_not_regular"));
    }
    let file_identity = match SourceFileIdentity::from_open_file(&file) {
        #[cfg(unix)]
        SourceFileIdentity::Unix { dev, ino } => (dev, ino),
        SourceFileIdentity::Unavailable => {
            return Err(NeedsEvidence("source_identity_unavailable"));
        }
    };
    let prefix = request.composed;
    let mut reader = BufReader::new(file);
    let mut decoder = RolloutRecordDecoder::default();
    let (mut scan, mut session_seen, mut named_turn, mut unopened_records) =
        (TurnScan::Before, false, false, false);
    let (mut offset, mut turn_start, mut composed) = (0_u64, 0_u64, String::new());
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|_| AwaitEvidence("source_unreadable"))?;
        if read == 0 || line.last() != Some(&b'\n') {
            return Err(match scan {
                _ if read != 0 => AwaitEvidence("partial_record"),
                TurnScan::Selected { .. } => AwaitEvidence("incomplete_prefix"),
                TurnScan::Before if unopened_records && !named_turn => {
                    NeedsEvidence("turns_unnamed")
                }
                TurnScan::Before => AwaitEvidence("turn_not_written"),
            });
        }
        let record_start = offset;
        offset += read as u64;
        let record: Value = std::str::from_utf8(&line)
            .ok()
            .and_then(|text| serde_json::from_str(text.trim()).ok())
            .ok_or(NeedsEvidence("malformed_record"))?;
        if record.get("type").and_then(Value::as_str) == Some("session_meta") {
            if record.pointer("/payload/id").and_then(Value::as_str) != Some(session_id) {
                return Err(NeedsEvidence("session_mismatch"));
            }
            session_seen = true;
            decoder.decode(&record);
            continue;
        }
        if let Some(opened) = opened_turn(&record) {
            named_turn |= opened.is_some();
            let selected = opened == Some(native_turn);
            scan = match (scan, selected) {
                (TurnScan::Before, true) if !session_seen => {
                    return Err(NeedsEvidence("session_unknown"));
                }
                (TurnScan::Before, true) => {
                    turn_start = record_start;
                    TurnScan::Selected { opener_run: true }
                }
                // task_started and turn_context naming the turn back to back augment one turn.
                (TurnScan::Selected { opener_run: true }, true) => scan,
                (TurnScan::Selected { .. }, true) => return Err(NeedsEvidence("ambiguous_turn")),
                // Another turn's start bounds the selected turn before the prefix was reached.
                (TurnScan::Selected { .. }, false) => {
                    return Err(NeedsEvidence("prefix_mismatch"));
                }
                (TurnScan::Before, false) => scan,
            };
            if selected {
                decoder.decode(&record);
            }
            continue;
        }
        if scan == TurnScan::Before {
            unopened_records = true;
            continue;
        }
        scan = TurnScan::Selected { opener_run: false };
        for message in decoder.decode(&record).unwrap_or_default() {
            if let StreamMessage::Text { content } = message {
                composed.push_str(&content);
            }
        }
        if composed.len() < prefix.len() {
            if !prefix.starts_with(composed.as_str()) {
                return Err(NeedsEvidence("prefix_mismatch"));
            }
            continue;
        }
        if !composed.starts_with(prefix) {
            return Err(NeedsEvidence("prefix_mismatch"));
        }
        return Ok(ProofPlan {
            witness: RawTurnWitness {
                path: request.raw_path.to_string_lossy().into_owned(),
                file_identity,
                session_id: session_id.to_owned(),
                native_turn_id: native_turn.to_owned(),
                turn_start,
                prefix_record_end: offset,
                prefix_len: prefix.len(),
            },
            reader,
            decoder,
            residual_text: composed.split_off(prefix.len()),
        });
    }
}
