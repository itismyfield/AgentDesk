//! Delivery ledger: every piece O prepares and what became of it, appended and fsynced per entry.
//! Replay rebuilds the anchor and outcomes; an ownership-invariant break is kept as a violation.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::ops::Range;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{StoreError, damage};
use crate::services::discord::runtime_store::fsync_parent_dir;
use crate::services::tui_o::shadow::{SourceId, UnitKey};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LedgerEntry {
    /// Written before the POST; `epoch` is the gateway ownership the POST was admitted under.
    Prepared {
        serial: u64,
        unit_key: UnitKey,
        piece_index: u32,
        payload: String,
        anchor_id: u64,
        epoch: u64,
    },
    Posted {
        serial: u64,
        msg_id: u64,
    },
    Rejected {
        serial: u64,
        status: u16,
    },
    NotFound {
        serial: u64,
    },
    Ambiguous {
        serial: u64,
        candidates: Vec<u64>,
    },
    Unresolved {
        serial: u64,
        reason: String,
    },
    Excluded {
        unit_key: UnitKey,
        reason: String,
    },
    /// Logged before a spool segment is deleted; `through` is the new retained start.
    SpoolGc {
        source: SourceId,
        segment_start: u64,
        through: u64,
    },
    /// An operator's start for a pending source; `excluded_range` is never posted. The writer
    /// moves the boundary to `Owed { from }` only once this entry is durable.
    BoundaryResolved {
        source: SourceId,
        from: u64,
        excluded_range: Range<u64>,
        operator: String,
        at: DateTime<Utc>,
    },
}

#[derive(Serialize, Deserialize)]
struct LedgerLine {
    at: DateTime<Utc>,
    entry: LedgerEntry,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PieceOutcome {
    Posted(u64),
    Rejected(u16),
    NotFound,
    Ambiguous(Vec<u64>),
    Unresolved(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PieceRecord {
    pub unit_key: UnitKey,
    pub piece_index: u32,
    pub payload: String,
    pub anchor_id: u64,
    pub epoch: u64,
    pub prepared_at: DateTime<Utc>,
    pub outcome: Option<PieceOutcome>,
}

/// Replayed ledger. `anchor` moves only on Posted; the first invariant break pauses the channel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LedgerState {
    anchor: u64,
    next_serial: u64,
    open_serial: Option<u64>,
    pieces: BTreeMap<u64, PieceRecord>,
    latest: HashMap<(UnitKey, u32), u64>,
    excluded: BTreeMap<UnitKey, String>,
    gc: HashMap<SourceId, Vec<(u64, u64)>>,
    resolved: HashMap<SourceId, u64>,
    violation: Option<String>,
}

impl LedgerState {
    pub fn anchor(&self) -> u64 {
        self.anchor
    }

    pub fn next_serial(&self) -> u64 {
        self.next_serial
    }

    /// The piece prepared without a recorded result; at most one exists while the invariant holds.
    pub fn unresolved(&self) -> Option<(u64, &PieceRecord)> {
        let serial = self.open_serial?;
        self.pieces.get(&serial).map(|piece| (serial, piece))
    }

    pub fn piece(&self, serial: u64) -> Option<&PieceRecord> {
        self.pieces.get(&serial)
    }

    /// Latest attempt for one piece of a unit.
    pub fn latest_piece(
        &self,
        unit_key: &UnitKey,
        piece_index: u32,
    ) -> Option<(u64, &PieceRecord)> {
        let serial = *self.latest.get(&(unit_key.clone(), piece_index))?;
        self.pieces.get(&serial).map(|piece| (serial, piece))
    }

    pub fn excluded(&self, unit_key: &UnitKey) -> Option<&str> {
        self.excluded.get(unit_key).map(String::as_str)
    }

    pub fn gc_through(&self, source: &SourceId) -> Option<u64> {
        self.gc_segments(source).last().map(|&(_, through)| through)
    }

    /// Logged GC spans `(segment_start, through)` in order; each starts where the previous ended.
    pub fn gc_segments(&self, source: &SourceId) -> &[(u64, u64)] {
        self.gc.get(source).map_or(&[], Vec::as_slice)
    }

    /// The start an operator resolved for a pending source; the first entry stands.
    pub fn boundary_resolved(&self, source: &SourceId) -> Option<u64> {
        self.resolved.get(source).copied()
    }

    /// Ownership evidence that pauses the channel: a serial out of order, two open pieces,
    /// anchor regression, or a result after Posted.
    pub fn violation(&self) -> Option<&str> {
        self.violation.as_deref()
    }

    fn violate(&mut self, detail: String) {
        self.violation.get_or_insert(detail);
    }

    pub(super) fn apply(&mut self, at: DateTime<Utc>, entry: LedgerEntry) {
        match entry {
            LedgerEntry::Prepared {
                serial,
                unit_key,
                piece_index,
                payload,
                anchor_id,
                epoch,
            } => {
                if serial != self.next_serial || self.open_serial.is_some() {
                    self.violate(format!("prepared serial {serial} out of order"));
                } else if anchor_id != self.anchor {
                    self.violate(format!(
                        "serial {serial} prepared against anchor {anchor_id}"
                    ));
                }
                self.next_serial = self.next_serial.max(serial.saturating_add(1));
                self.open_serial = Some(serial);
                self.latest.insert((unit_key.clone(), piece_index), serial);
                let piece = PieceRecord {
                    unit_key,
                    piece_index,
                    payload,
                    anchor_id,
                    epoch,
                    prepared_at: at,
                    outcome: None,
                };
                self.pieces.insert(serial, piece);
            }
            LedgerEntry::Posted { serial, msg_id } => {
                if msg_id <= self.anchor {
                    self.violate(format!("serial {serial} posted {msg_id} behind the anchor"));
                } else {
                    self.anchor = msg_id;
                }
                self.resolve(serial, PieceOutcome::Posted(msg_id));
            }
            LedgerEntry::Rejected { serial, status } => {
                self.resolve(serial, PieceOutcome::Rejected(status))
            }
            LedgerEntry::NotFound { serial } => self.resolve(serial, PieceOutcome::NotFound),
            LedgerEntry::Ambiguous { serial, candidates } => {
                self.resolve(serial, PieceOutcome::Ambiguous(candidates))
            }
            LedgerEntry::Unresolved { serial, reason } => {
                self.resolve(serial, PieceOutcome::Unresolved(reason))
            }
            LedgerEntry::Excluded { unit_key, reason } => {
                self.excluded.insert(unit_key, reason);
            }
            LedgerEntry::SpoolGc {
                source,
                segment_start,
                through,
            } => {
                let spans = self.gc.entry(source).or_default();
                let joins = spans.last().is_none_or(|&(_, last)| last == segment_start);
                spans.push((segment_start, through));
                if !joins || through <= segment_start {
                    self.violate(format!(
                        "SpoolGc {segment_start}..{through} breaks the GC chain"
                    ));
                }
            }
            LedgerEntry::BoundaryResolved { source, from, .. } => {
                self.resolved.entry(source).or_insert(from);
            }
        }
    }

    fn resolve(&mut self, serial: u64, outcome: PieceOutcome) {
        let Some(piece) = self.pieces.get_mut(&serial) else {
            return self.violate(format!("result for unprepared serial {serial}"));
        };
        if matches!(piece.outcome, Some(PieceOutcome::Posted(_))) {
            return self.violate(format!("serial {serial} has a result after Posted"));
        }
        piece.outcome = Some(outcome);
        if self.open_serial == Some(serial) {
            self.open_serial = None;
        }
    }
}

/// Creates the empty ledger before `init`; an existing one must still be empty then.
pub(super) fn create_empty(path: &Path) -> Result<(), StoreError> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file.sync_all()?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if std::fs::metadata(path)?.len() != 0 {
                return Err(damage("ledger has entries before init"));
            }
        }
        Err(error) => return Err(error.into()),
    }
    Ok(fsync_parent_dir(path)?)
}

pub(super) fn append(
    path: &Path,
    at: DateTime<Utc>,
    entry: &LedgerEntry,
) -> Result<(), StoreError> {
    let mut file = OpenOptions::new().append(true).open(path)?;
    append_to(&mut file, at, entry)
}

pub(super) fn append_to(
    file: &mut File,
    at: DateTime<Utc>,
    entry: &LedgerEntry,
) -> Result<(), StoreError> {
    let entry = entry.clone();
    let mut line = serde_json::to_vec(&LedgerLine { at, entry })?;
    line.push(b'\n');
    file.write_all(&line)?;
    Ok(file.sync_data()?)
}

/// The `from` of a durable `BoundaryResolved` for `source`, read without recovering the ledger.
/// An unfinished last line is refused: a line appended after it would become mid-file damage.
pub(super) fn resolution(file: &mut File, source: &SourceId) -> Result<Option<u64>, StoreError> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if bytes.last().is_some_and(|byte| *byte != b'\n') {
        let detail = "the ledger ends in an unfinished entry; start the writer to recover it";
        return Err(StoreError::Rejected(detail.into()));
    }
    for line in bytes.split(|byte| *byte == b'\n') {
        if let Ok(LedgerLine {
            entry: LedgerEntry::BoundaryResolved {
                source: s, from, ..
            },
            ..
        }) = serde_json::from_slice(line)
            && s == *source
        {
            return Ok(Some(from));
        }
    }
    Ok(None)
}

/// Replays the ledger from `initial_anchor`; an unfinished last line is cut, any other bad line is damage.
pub(super) fn recover(path: &Path, initial_anchor: u64) -> Result<LedgerState, StoreError> {
    recover_with_tail(path, initial_anchor, || {})
}

fn recover_with_tail(
    path: &Path,
    initial_anchor: u64,
    mut before_truncate: impl FnMut(),
) -> Result<LedgerState, StoreError> {
    let mut state = LedgerState {
        anchor: initial_anchor,
        ..LedgerState::default()
    };
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    // Refuse recovery while an operator is writing; replay and truncation share this handle.
    file.try_lock().map_err(io::Error::from)?;
    let mut reader = BufReader::new(file);
    let (mut offset, mut line) = (0u64, Vec::new());
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(state);
        }
        if line.last() != Some(&b'\n') {
            before_truncate();
            reader.get_ref().set_len(offset)?;
            reader.get_ref().sync_all()?;
            return Ok(state);
        }
        let parsed: LedgerLine = serde_json::from_slice(&line)
            .map_err(|error| damage(format!("ledger byte {offset}: {error}")))?;
        state.apply(parsed.at, parsed.entry);
        offset += read as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tui_o::shadow::{ShadowProvider, UnitKind};

    fn prepared(serial: u64, anchor_id: u64) -> LedgerEntry {
        let (provider, kind) = (ShadowProvider::Codex, UnitKind::Body);
        let unit_key = UnitKey {
            channel_id: 1,
            provider,
            native_key: format!("r{serial}"),
            kind,
        };
        LedgerEntry::Prepared {
            serial,
            unit_key,
            piece_index: 0,
            payload: "x".into(),
            anchor_id,
            epoch: 1,
        }
    }

    fn replay(entries: Vec<LedgerEntry>) -> LedgerState {
        let mut state = LedgerState {
            anchor: 10,
            ..LedgerState::default()
        };
        entries
            .into_iter()
            .for_each(|entry| state.apply(Utc::now(), entry));
        state
    }

    #[test]
    fn replay_flags_the_ownership_invariant_breaks_that_pause_a_channel() {
        use LedgerEntry::{Ambiguous, NotFound, Posted};
        let clean = replay(vec![
            prepared(0, 10),
            Posted {
                serial: 0,
                msg_id: 20,
            },
            prepared(1, 20),
            Ambiguous {
                serial: 1,
                candidates: vec![30, 31],
            },
            prepared(2, 20),
            NotFound { serial: 2 },
        ]);
        assert_eq!(
            (clean.violation(), clean.anchor(), clean.next_serial()),
            (None, 20, 3)
        );
        let breaks = [
            vec![
                prepared(0, 10),
                Posted {
                    serial: 0,
                    msg_id: 20,
                },
                Posted {
                    serial: 0,
                    msg_id: 21,
                },
            ],
            vec![
                prepared(0, 10),
                Posted {
                    serial: 0,
                    msg_id: 9,
                },
            ],
            vec![prepared(0, 10), prepared(1, 10)],
            vec![prepared(1, 10)],
            vec![prepared(0, 5)],
            vec![NotFound { serial: 4 }],
        ];
        for entries in breaks {
            let state = replay(entries.clone());
            assert!(state.violation().is_some(), "no violation for {entries:?}");
        }
    }
}

#[cfg(test)]
#[path = "ledger_lock_tests.rs"]
mod lock_tests;
