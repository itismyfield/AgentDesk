//! A Herdr Claude turn read back from its transcript: its own terminal, or why none can be named.

use std::io::{Read, Seek, SeekFrom};

use super::{HerdrTurnLines, is_turn_output, is_turn_prompt};
use crate::services::agent_protocol::{NativeTerminalKind, StreamMessage};

/// A turn's provider terminal as `HerdrTurnLines` accepts it, and the record range it read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Terminal {
    pub(crate) kind: NativeTerminalKind,
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) result: String,
    /// The turn's streamed output in order, as the live bridge receives it.
    pub(crate) output: Vec<TurnOutput>,
}

/// A streamed assistant text chunk, or a tool use between chunks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TurnOutput {
    Text(String),
    Tool,
}

/// The transcript could not be read, or another turn's records share the turn's range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Unobserved;

/// Admission's re-read of a Claude turn from its descriptor: from the first record stamped at or
/// after the input, as the reader starts, to the first terminal `HerdrTurnLines` accepts.
pub(crate) fn replay_herdr_turn(
    file: &mut std::fs::File,
    submitted_at: chrono::DateTime<chrono::Utc>,
    len: u64,
) -> Option<(u64, NativeTerminalKind, u64)> {
    let bytes = read_prefix(file, len).ok()?;
    let terminal = turn_in(&complete_records(&bytes), submitted_at).ok()??;
    Some((terminal.start, terminal.kind, terminal.end))
}

/// The row end's one read: `None` unless the last turn closed, else the input's turn if it reads.
pub(crate) fn closed_turn(
    file: &mut std::fs::File,
    submitted_at: Option<chrono::DateTime<chrono::Utc>>,
    len: u64,
) -> Option<Option<Terminal>> {
    let bytes = read_prefix(file, len).ok()?;
    let records = complete_records(&bytes);
    if !last_turn_closed(&bytes, &records) {
        return None;
    }
    Some(submitted_at.and_then(|at| turn_in(&records, at).ok().flatten()))
}

/// What the row pass reads of a Herdr turn from its own start once its provider is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EndedTurn {
    Terminal,
    NoTerminal,
    /// Another turn's records share the turn's range.
    Mixed,
}

/// The row pass's read once the writer is gone: a torn tail that parses whole is the last record,
/// any other is none, so only an unreadable transcript leaves the turn unobserved.
pub(crate) fn ended_turn(
    file: &mut std::fs::File,
    submitted_at: chrono::DateTime<chrono::Utc>,
    len: u64,
) -> Result<EndedTurn, Unobserved> {
    let bytes = read_prefix(file, len)?;
    let mut records = complete_records(&bytes);
    let cut = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    let tail = std::str::from_utf8(&bytes[cut..]).unwrap_or_default();
    if serde_json::from_str::<serde_json::Value>(tail).is_ok() {
        records.push((cut as u64, tail.trim()));
    } else if !tail.trim().is_empty()
        && crate::services::provider::herdr_before_start::mutant("prb_torn_tail_unobserved")
    {
        return Err(Unobserved);
    }
    Ok(match turn_in(&records, submitted_at) {
        Ok(Some(_)) => EndedTurn::Terminal,
        Ok(None) => EndedTurn::NoTerminal,
        Err(Unobserved) => EndedTurn::Mixed,
    })
}

/// `Ok(None)` while no record is stamped at or after the input, or no terminal follows it yet.
fn turn_in(
    records: &[(u64, &str)],
    submitted_at: chrono::DateTime<chrono::Utc>,
) -> Result<Option<Terminal>, Unobserved> {
    let stamped = |line: &str| {
        let json = serde_json::from_str::<serde_json::Value>(line).ok()?;
        let stamp = json.get("timestamp")?.as_str()?;
        chrono::DateTime::parse_from_rfc3339(stamp).ok()
    };
    let Some((start, at)) = records.iter().enumerate().find_map(|(at, (start, line))| {
        stamped(line)
            .filter(|stamp| *stamp >= submitted_at)
            .map(|_| (*start, at))
    }) else {
        return Ok(None);
    };
    let (local, rx) = std::sync::mpsc::channel();
    let (mut lines, mut output) = (HerdrTurnLines::default(), Vec::new());
    for (record_start, line) in &records[at..] {
        lines.process(line, &local);
        output.extend(rx.try_iter().filter_map(|message| match message {
            StreamMessage::Text { content } => Some(TurnOutput::Text(content)),
            StreamMessage::ToolUse { .. } => Some(TurnOutput::Tool),
            _ => None,
        }));
        if let Some(kind) = lines.terminal() {
            let end = record_start + line.len() as u64 + 1;
            let (result, _) = lines.result();
            return Ok(Some(Terminal {
                kind,
                start,
                end,
                result,
                output,
            }));
        }
    }
    if lines.mixed {
        return Err(Unobserved);
    }
    Ok(None)
}

/// Whether the transcript's last prompt has its terminal and no turn output follows it; a torn
/// tail is a record still being written, so it is never closed.
fn last_turn_closed(bytes: &[u8], records: &[(u64, &str)]) -> bool {
    let tail = bytes
        .rsplit(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    if !tail.trim_ascii().is_empty() {
        return false;
    }
    let Some(last) = records.iter().rposition(|(_, line)| is_turn_prompt(line)) else {
        return false;
    };
    let (local, _rx) = std::sync::mpsc::channel();
    let mut lines = HerdrTurnLines::default();
    for (at, (_, line)) in records.iter().enumerate().skip(last) {
        lines.process(line, &local);
        if lines.terminal().is_some() {
            return !records[at + 1..].iter().any(|(_, l)| is_turn_output(l));
        }
    }
    false
}

fn read_prefix(file: &mut std::fs::File, len: u64) -> Result<Vec<u8>, Unobserved> {
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(0)).map_err(|_| Unobserved)?;
    file.take(len)
        .read_to_end(&mut bytes)
        .map_err(|_| Unobserved)?;
    Ok(bytes)
}

/// Each newline-terminated record's start offset and trimmed text; a torn tail is not a record.
fn complete_records(bytes: &[u8]) -> Vec<(u64, &str)> {
    let mut records = Vec::new();
    let mut offset = 0u64;
    for raw in bytes.split_inclusive(|byte| *byte == b'\n') {
        if raw.last() != Some(&b'\n') {
            break;
        }
        let line = &raw[..raw.len() - 1];
        if let Ok(text) = std::str::from_utf8(line)
            && !text.trim().is_empty()
        {
            records.push((offset, text));
        }
        offset += raw.len() as u64;
    }
    records
}
