//! A Herdr Claude turn read back from its transcript: its own terminal, or why none can be named.

use std::io::{Read, Seek, SeekFrom};

use super::{HerdrTurnLines, is_turn_output, is_turn_prompt};
use crate::services::agent_protocol::NativeTerminalKind;

/// A turn's provider terminal as `HerdrTurnLines` accepts it, and the record range it read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Terminal {
    pub(crate) kind: NativeTerminalKind,
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) result: String,
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
    let terminal = read_turn(file, submitted_at, len).ok()??;
    Some((terminal.start, terminal.kind, terminal.end))
}

/// `Ok(None)` while no record is stamped at or after the input, or no terminal follows it yet.
pub(crate) fn read_turn(
    file: &mut std::fs::File,
    submitted_at: chrono::DateTime<chrono::Utc>,
    len: u64,
) -> Result<Option<Terminal>, Unobserved> {
    let bytes = read_prefix(file, len)?;
    let records = complete_records(&bytes);
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
    let (local, _rx) = std::sync::mpsc::channel();
    let mut lines = HerdrTurnLines::default();
    for (record_start, line) in &records[at..] {
        lines.process(line, &local);
        if let Some(kind) = lines.terminal() {
            let end = record_start + line.len() as u64 + 1;
            let (result, _) = lines.result();
            return Ok(Some(Terminal {
                kind,
                start,
                end,
                result,
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
pub(crate) fn last_turn_closed(file: &mut std::fs::File, len: u64) -> Result<bool, Unobserved> {
    let bytes = read_prefix(file, len)?;
    let tail = bytes
        .rsplit(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    if !tail.trim_ascii().is_empty() {
        return Ok(false);
    }
    let records = complete_records(&bytes);
    let Some(last) = records.iter().rposition(|(_, line)| is_turn_prompt(line)) else {
        return Ok(false);
    };
    let (local, _rx) = std::sync::mpsc::channel();
    let mut lines = HerdrTurnLines::default();
    for (at, (_, line)) in records.iter().enumerate().skip(last) {
        lines.process(line, &local);
        if lines.terminal().is_some() {
            return Ok(!records[at + 1..].iter().any(|(_, l)| is_turn_output(l)));
        }
    }
    Ok(false)
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
