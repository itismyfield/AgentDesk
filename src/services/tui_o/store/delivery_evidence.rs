//! Bounded, read-only snapshots for delivery observers; never recover or repair the writer store.

use super::{CURSOR_DIR, LEDGER_FILE, OStore, ledger, rotation, spool};
use crate::services::tui_o::shadow::{SourceId, UnitKey};
use serde::de::DeserializeOwned;
use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Read},
    path::Path,
};
use tokio::time::Instant;

pub(crate) fn json<T: DeserializeOwned>(path: &Path, cap: u64) -> Option<T> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= cap)
        .then(|| serde_json::from_slice(&bytes).ok())
        .flatten()
}

pub(crate) fn source(
    root: &Path,
    channel: u64,
    path: &Path,
    file: (u64, u64),
) -> Option<spool::Cursor> {
    let store = OStore::existing(root)?;
    let init = store.read_init(channel).ok()??;
    let dir = store.channel_dir(channel);
    let mut sources: Vec<SourceId> = init.sources.into_iter().map(|s| s.source_id).collect();
    let boundary = dir.join(rotation::BOUNDARY_FILE);
    if boundary.exists() {
        let rotation: rotation::Rotation = json(&boundary, 1024 * 1024)?;
        sources.extend(rotation.links.into_values().map(|link| link.source));
    }
    let source = sources
        .into_iter()
        .find(|s| s.path == path && (s.dev, s.ino) == file)?;
    let cursor: spool::Cursor = json(
        &dir.join(CURSOR_DIR)
            .join(format!("{}.json", spool::source_key(&source))),
        64 * 1024,
    )?;
    (cursor.source == source).then_some(cursor)
}

pub(crate) fn stamp(root: &Path, channel: u64) -> Option<std::time::SystemTime> {
    let store = OStore::existing(root)?;
    std::fs::metadata(store.channel_dir(channel).join(LEDGER_FILE))
        .ok()?
        .modified()
        .ok()
}

// Candidates are scoped to one manifest; they never authorize a writer or replace ledger replay.
#[derive(Default)]
pub(crate) struct LocalEvidence {
    pieces: HashMap<(UnitKey, u32), (u64, ledger::PieceRecord)>,
    #[cfg(test)]
    pub retained_payload_bytes: usize,
    #[cfg(test)]
    pub max_line_bytes: usize,
}

impl LocalEvidence {
    pub(crate) fn latest_piece(
        &self,
        key: &UnitKey,
        index: u32,
    ) -> Option<(u64, &ledger::PieceRecord)> {
        self.pieces
            .get(&(key.clone(), index))
            .map(|(serial, record)| (*serial, record))
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static AFTER_SNAPSHOT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn local(
    root: &Path,
    channel: u64,
    requested: &[(UnitKey, u32)],
    deadline: Instant,
) -> Option<LocalEvidence> {
    const LINE_CAP: usize = 1024 * 1024;
    const PAYLOAD_CAP: usize = 2 * 1024 * 1024;
    if requested.is_empty()
        || requested.len() > 256
        || requested
            .iter()
            .map(|(key, _)| key.native_key.len())
            .sum::<usize>()
            > 64 * 1024
        || Instant::now() >= deadline
    {
        return None;
    }
    let store = OStore::existing(root)?;
    store.read_init(channel).ok()??;
    let path = store.channel_dir(channel).join(LEDGER_FILE);
    let file = File::open(&path).ok()?;
    let before = file.metadata().ok()?;
    let identity = crate::services::tui_o::shadow::capture::file_identity(&before);
    #[cfg(test)]
    AFTER_SNAPSHOT.with(|hook| {
        if let Some(hook) = hook.take() {
            hook();
        }
    });
    let mut reader = BufReader::new(file.take(before.len()));
    let mut line = Vec::new();
    let mut state = LocalEvidence::default();
    let mut next_serial = 0;
    let mut payload_bytes = 0;
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        line.clear();
        let read = reader
            .by_ref()
            .take(LINE_CAP as u64 + 1)
            .read_until(b'\n', &mut line)
            .ok()?;
        if read == 0 {
            break;
        }
        if line.len() > LINE_CAP || line.last() != Some(&b'\n') {
            return None;
        }
        #[cfg(test)]
        {
            state.max_line_bytes = state.max_line_bytes.max(line.len());
        }
        let parsed: ledger::LedgerLine = serde_json::from_slice(&line).ok()?;
        match parsed.entry {
            ledger::LedgerEntry::Prepared {
                serial,
                unit_key,
                piece_index,
                payload,
                anchor_id,
                epoch,
                local_retry,
            } => {
                if serial != next_serial {
                    return None;
                }
                next_serial = serial.checked_add(1)?;
                if !requested.contains(&(unit_key.clone(), piece_index)) {
                    continue;
                }
                let key = (unit_key.clone(), piece_index);
                if let Some((_, previous)) = state.pieces.remove(&key) {
                    payload_bytes -= previous.payload.len();
                }
                payload_bytes += payload.len();
                if payload_bytes > PAYLOAD_CAP {
                    return None;
                }
                state.pieces.insert(
                    key,
                    (
                        serial,
                        ledger::PieceRecord {
                            unit_key,
                            piece_index,
                            payload,
                            anchor_id,
                            epoch,
                            prepared_at: parsed.at,
                            outcome: None,
                            local_retry,
                            outcome_at: None,
                        },
                    ),
                );
            }
            entry => {
                let (serial, outcome) = match entry {
                    ledger::LedgerEntry::Posted { serial, msg_id } => {
                        (serial, ledger::PieceOutcome::Posted(msg_id))
                    }
                    ledger::LedgerEntry::Rejected { serial, status } => {
                        (serial, ledger::PieceOutcome::Rejected(status))
                    }
                    ledger::LedgerEntry::NotFound { serial } => {
                        (serial, ledger::PieceOutcome::NotFound)
                    }
                    ledger::LedgerEntry::NotSent { serial } => {
                        (serial, ledger::PieceOutcome::NotSent)
                    }
                    ledger::LedgerEntry::Ambiguous { serial, .. } => {
                        (serial, ledger::PieceOutcome::Ambiguous(Vec::new()))
                    }
                    ledger::LedgerEntry::Unresolved { serial, .. } => {
                        (serial, ledger::PieceOutcome::Unresolved(String::new()))
                    }
                    ledger::LedgerEntry::ExactEvidence { metadata } if !metadata.supported() => {
                        return None;
                    }
                    _ => continue,
                };
                if serial >= next_serial {
                    return None;
                }
                if let Some((_, record)) = state.pieces.values_mut().find(|(s, _)| *s == serial) {
                    if matches!(record.outcome, Some(ledger::PieceOutcome::Posted(_))) {
                        return None;
                    }
                    record.outcome = Some(outcome);
                    record.outcome_at = Some(parsed.at);
                }
            }
        }
    }
    let after = std::fs::metadata(&path).ok()?;
    if crate::services::tui_o::shadow::capture::file_identity(&after) != identity
        || after.len() < before.len()
        || (after.len() == before.len() && after.modified().ok()? != before.modified().ok()?)
        || Instant::now() >= deadline
    {
        return None;
    }
    #[cfg(test)]
    {
        state.retained_payload_bytes = payload_bytes;
    }
    Some(state)
}
