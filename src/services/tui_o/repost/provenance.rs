//! Re-post provenance beside, not in, the delivery ledger: which serials were on originals and how
//! far their admission got. A synced entry counts; a torn tail is cut, other damage refuses replay.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::identity::{PieceKey, marker, payload_sha256, piece_of};
use super::send::RepostIds;
use crate::services::discord::runtime_store::fsync_parent_dir;
use crate::services::tui_o::shadow::UnitKey;
use crate::services::tui_o::store::ledger::PieceRecord;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ProvenanceEntry {
    /// Re-post turned on. Serials below `frontier` were prepared before and are never re-posted.
    Activated { generation: u64, frontier: u64 },
    /// An original reached its POST under the bounded transport while re-post was on.
    Intent {
        serial: u64,
        unit_key: UnitKey,
        piece_index: u32,
        payload_sha256: String,
        generation: u64,
        nonce: String,
    },
    /// The original's result went uncertain; written before its PostgreSQL admission.
    AdmissionPending { serial: u64 },
    /// The admission is in PostgreSQL.
    Admitted { serial: u64 },
    /// Uncertain pieces reported once as never re-posted.
    BacklogReported { generation: u64, serials: Vec<u64> },
}

impl ProvenanceEntry {
    /// The intent for `record` about to be posted, or `None` when no re-post may ever cover it.
    pub(crate) fn intent(serial: u64, record: &PieceRecord, generation: u64) -> Option<Self> {
        let key = piece_of(record)?;
        let ids = RepostIds::for_piece(&marker(&key))?;
        Some(Self::Intent {
            serial,
            unit_key: key.unit().clone(),
            piece_index: key.piece_index(),
            payload_sha256: payload_sha256(&record.payload),
            generation,
            nonce: ids.nonce().to_owned(),
        })
    }
}

#[derive(Serialize, Deserialize)]
struct ProvenanceLine {
    at: DateTime<Utc>,
    entry: ProvenanceEntry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Activation {
    pub(crate) generation: u64,
    pub(crate) frontier: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Intent {
    pub(crate) key: Option<PieceKey>,
    pub(crate) payload_sha256: String,
}

/// The replayed sidecar.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Provenance {
    activation: Option<Activation>,
    intents: BTreeMap<u64, Intent>,
    pending: BTreeSet<u64>,
    admitted: BTreeSet<u64>,
    reported: BTreeSet<u64>,
}

impl Provenance {
    /// The latest activation; none means re-post never ran here.
    pub(crate) fn activation(&self) -> Option<Activation> {
        self.activation
    }

    pub(crate) fn intent(&self, serial: u64) -> Option<&Intent> {
        self.intents.get(&serial)
    }

    /// Serials whose admission was started and not seen through.
    pub(crate) fn pending(&self) -> impl Iterator<Item = u64> + '_ {
        self.pending.difference(&self.admitted).copied()
    }

    pub(crate) fn started(&self, serial: u64) -> bool {
        self.pending.contains(&serial) || self.admitted.contains(&serial)
    }

    pub(crate) fn reported(&self, serial: u64) -> bool {
        self.reported.contains(&serial)
    }

    fn apply(&mut self, entry: ProvenanceEntry) {
        match entry {
            ProvenanceEntry::Activated {
                generation,
                frontier,
            } => {
                self.activation = Some(Activation {
                    generation,
                    frontier,
                })
            }
            ProvenanceEntry::Intent {
                serial,
                unit_key,
                piece_index,
                payload_sha256,
                ..
            } => {
                let key = PieceKey::new(unit_key, piece_index);
                let intent = Intent {
                    key,
                    payload_sha256,
                };
                self.intents.insert(serial, intent);
            }
            ProvenanceEntry::AdmissionPending { serial } => {
                self.pending.insert(serial);
            }
            ProvenanceEntry::Admitted { serial } => {
                self.admitted.insert(serial);
            }
            ProvenanceEntry::BacklogReported { serials, .. } => self.reported.extend(serials),
        }
    }
}

/// The sidecar file of one channel and its replayed state.
#[derive(Debug)]
pub(crate) struct ProvenanceLog {
    path: PathBuf,
    state: Provenance,
}

impl ProvenanceLog {
    /// Opens or creates the sidecar. A damaged line is an error: without the file nothing proves
    /// a piece was an on original, so nothing may be re-posted.
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        fsync_parent_dir(path)?;
        let state = replay(&mut file)?;
        Ok(Self {
            path: path.to_owned(),
            state,
        })
    }

    pub(crate) fn state(&self) -> &Provenance {
        &self.state
    }

    /// Counts the entry only once it is synced.
    pub(crate) fn append(&mut self, entry: ProvenanceEntry) -> io::Result<()> {
        let line = ProvenanceLine {
            at: Utc::now(),
            entry: entry.clone(),
        };
        let mut bytes = serde_json::to_vec(&line).map_err(io::Error::other)?;
        bytes.push(b'\n');
        let mut file = OpenOptions::new().append(true).open(&self.path)?;
        file.write_all(&bytes)?;
        file.sync_data()?;
        self.state.apply(entry);
        Ok(())
    }

    /// Records an observed off-to-on switch; pieces prepared before `frontier` stay out.
    pub(crate) fn activate(&mut self, frontier: u64) -> io::Result<Activation> {
        let generation = self.state.activation.map_or(1, |last| last.generation + 1);
        self.append(ProvenanceEntry::Activated {
            generation,
            frontier,
        })?;
        Ok(Activation {
            generation,
            frontier,
        })
    }
}

fn replay(file: &mut File) -> io::Result<Provenance> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let complete = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |end| end + 1);
    if complete < bytes.len() {
        file.set_len(complete as u64)?;
        file.sync_all()?;
    }
    let mut state = Provenance::default();
    for (number, line) in bytes[..complete]
        .split_inclusive(|byte| *byte == b'\n')
        .enumerate()
    {
        let line: ProvenanceLine = serde_json::from_slice(line).map_err(|error| {
            let detail = format!("re-post provenance line {}: {error}", number + 1);
            io::Error::new(io::ErrorKind::InvalidData, detail)
        })?;
        state.apply(line.entry);
    }
    Ok(state)
}
