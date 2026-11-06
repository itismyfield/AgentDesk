//! Bounded, read-only snapshots for delivery observers; never recover or repair the writer store.

use super::{CURSOR_DIR, LEDGER_FILE, OStore, ledger, rotation, spool};
use crate::services::tui_o::shadow::{SourceId, UnitKey, capture};
use serde::de::DeserializeOwned;
use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex, PoisonError},
    time::SystemTime,
};
use tokio::time::Instant;

mod streaming;

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
    // A reboot may renumber the volume's dev while the writer keeps the source it stored first;
    // like the writer, two stored variants of one file are refused rather than guessed between.
    let mut stored = sources
        .into_iter()
        .filter(|s| s.path == path && capture::file_match(s, file) != capture::FileMatch::Other);
    let source = stored.next()?;
    if stored.any(|other| other != source) {
        return None;
    }
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
#[derive(Clone, Default)]
pub(crate) struct LocalEvidence {
    pieces: HashMap<(UnitKey, u32), (u64, ledger::PieceRecord)>,
    confirmations: HashMap<u64, Box<ledger::PostConfirmation>>,
    #[cfg(test)]
    pub retained_payload_bytes: usize,
    #[cfg(test)]
    pub max_line_bytes: usize,
    /// Ledger lines this observation parsed.
    #[cfg(test)]
    pub parsed_lines: usize,
}

impl LocalEvidence {
    pub(crate) fn confirmed_post(&self, key: &UnitKey, index: u32, bot_id: u64) -> Option<u64> {
        let (serial, piece) = self.latest_piece(key, index)?;
        let confirmation = self.confirmations.get(&serial)?;
        if !confirmation.matches(serial, piece, bot_id) {
            return None;
        }
        match piece.outcome {
            Some(ledger::PieceOutcome::Posted(id)) => Some(id),
            _ => None,
        }
    }

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

/// Where a completed observation of a ledger ended; the writer only appends and cuts only its last
/// complete line, so the next observation resumes here while that line reads the same.
#[derive(Clone)]
struct Resume {
    path: PathBuf,
    requested: Vec<(UnitKey, u32)>,
    identity: (u64, u64),
    end: u64,
    modified: SystemTime,
    last: (u64, Vec<u8>),
    next_serial: u64,
    payload_bytes: usize,
    state: LocalEvidence,
}

/// A few concurrent terminal frames per host; each entry holds at most the payload cap.
const RESUME_LIMIT: usize = 8;

static RESUMES: LazyLock<Mutex<Vec<Resume>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// The kept observation `file` still extends: the same file, not shorter, rewritten in place only
/// by growing, with its last complete line unchanged. A file without identity never resumes.
fn resume_point(
    file: &mut File,
    path: &Path,
    requested: &[(UnitKey, u32)],
    meta: &std::fs::Metadata,
) -> Option<Resume> {
    let identity = crate::services::tui_o::shadow::capture::file_identity(meta);
    if identity == (0, 0) {
        return None;
    }
    let kept = (RESUMES.lock().unwrap_or_else(PoisonError::into_inner))
        .iter()
        .find(|kept| kept.path == path && kept.requested == requested)?
        .clone();
    if kept.identity != identity
        || meta.len() < kept.end
        || (meta.len() == kept.end && meta.modified().ok()? != kept.modified)
    {
        return None;
    }
    let (start, line) = &kept.last;
    let mut read = vec![0; line.len()];
    file.seek(SeekFrom::Start(*start)).ok()?;
    file.read_exact(&mut read).ok()?;
    (read == *line).then_some(kept)
}

fn keep(resume: Resume) {
    let mut kept = RESUMES.lock().unwrap_or_else(PoisonError::into_inner);
    kept.retain(|old| old.path != resume.path || old.requested != resume.requested);
    if kept.len() >= RESUME_LIMIT {
        kept.remove(0);
    }
    kept.push(resume);
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
    let mut file = File::open(&path).ok()?;
    let before = file.metadata().ok()?;
    let identity = crate::services::tui_o::shadow::capture::file_identity(&before);
    #[cfg(test)]
    AFTER_SNAPSHOT.with(|hook| {
        if let Some(hook) = hook.take() {
            hook();
        }
    });
    let resumed = resume_point(&mut file, &path, requested, &before);
    let (start, mut state, mut next_serial, mut payload_bytes, mut last) = match resumed {
        Some(kept) => (
            kept.end,
            kept.state,
            kept.next_serial,
            kept.payload_bytes,
            kept.last,
        ),
        None => (0, LocalEvidence::default(), 0, 0, (0, Vec::new())),
    };
    #[cfg(test)]
    {
        state.parsed_lines = 0;
    }
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut reader = BufReader::new(file.take(before.len().checked_sub(start)?));
    let mut line = Vec::new();
    // An oversized row is skipped without recording where it ended, so that scan is not kept.
    let mut resumable = true;
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        let line_start = before.len() - reader.get_ref().limit() - reader.buffer().len() as u64;
        line.clear();
        let read = reader
            .by_ref()
            .take(LINE_CAP as u64 + 1)
            .read_until(b'\n', &mut line)
            .ok()?;
        if read == 0 {
            break;
        }
        if line.len() > LINE_CAP {
            streaming::notice(std::io::Cursor::new(&line).chain(&mut reader), deadline)?;
            resumable = false;
            continue;
        }
        if line.last() != Some(&b'\n') {
            return None;
        }
        #[cfg(test)]
        {
            state.max_line_bytes = state.max_line_bytes.max(line.len());
        }
        let parsed: ledger::LedgerLine = serde_json::from_slice(&line).ok()?;
        #[cfg(test)]
        {
            state.parsed_lines += 1;
        }
        last.0 = line_start;
        std::mem::swap(&mut last.1, &mut line);
        match parsed.entry {
            ledger::LedgerEntry::Prepared {
                serial,
                unit_key,
                piece_index,
                payload,
                anchor_id,
                epoch,
                local_retry,
                notice_origin,
            } => {
                if serial != next_serial {
                    return None;
                }
                next_serial = serial.checked_add(1)?;
                if !requested.contains(&(unit_key.clone(), piece_index)) {
                    continue;
                }
                let key = (unit_key.clone(), piece_index);
                if let Some((previous_serial, previous)) = state.pieces.remove(&key) {
                    state.confirmations.remove(&previous_serial);
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
                            notice_origin,
                            outcome_at: None,
                        },
                    ),
                );
            }
            entry => {
                let mut posted_confirmation = None;
                let (serial, outcome) = match entry {
                    ledger::LedgerEntry::Posted {
                        serial,
                        msg_id,
                        confirmation,
                    } => {
                        posted_confirmation = confirmation;
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
                    if let Some(confirmation) = posted_confirmation
                        .filter(|confirmation| confirmation.matches_piece(serial, record))
                    {
                        state.confirmations.insert(serial, confirmation);
                    }
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
    if let (true, Ok(modified)) = (resumable && identity != (0, 0), before.modified()) {
        keep(Resume {
            path,
            requested: requested.to_vec(),
            identity,
            end: before.len(),
            modified,
            last,
            next_serial,
            payload_bytes,
            state: state.clone(),
        });
    }
    Some(state)
}

#[cfg(all(test, unix))]
mod tests;
