use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::blob::BlobPin;
use super::durable::{self, invalid};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub seq: u64,
    pub prev_crc: u32,
    pub kind: String,
    pub payload: Value,
    pub crc: u32,
}

impl Record {
    fn checksum(&self) -> io::Result<u32> {
        Ok(durable::crc(&serde_json::to_vec(&(
            self.seq,
            self.prev_crc,
            &self.kind,
            &self.payload,
        ))?))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub generation: u64,
    pub seq: u64,
    pub last_crc: u32,
    pub state: Value,
    crc: u32,
}

impl Snapshot {
    fn checksum(&self) -> io::Result<u32> {
        Ok(durable::crc(&serde_json::to_vec(&(
            self.generation,
            self.seq,
            self.last_crc,
            &self.state,
        ))?))
    }
}

// The channel actor owns this handle; errors after a write require reopening it.
pub struct Ledger {
    pub(super) dir: PathBuf,
    wal: File,
    generation: u64,
    seq: u64,
    last_crc: u32,
    usable: bool,
    snapshot: Option<Snapshot>,
    records: Vec<Record>,
}

/// What exists for a channel's ledger, judged without creating anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    Absent,
    Present,
    Unreadable,
}

pub fn dir(runtime_root: &Path, channel_id: u64) -> PathBuf {
    runtime_root
        .join("input_ledger")
        .join(channel_id.to_string())
}

#[cfg(test)]
thread_local! { pub(crate) static OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

impl Ledger {
    // Any directory, even an empty one, is history: a failed open may have left it behind.
    pub fn probe(runtime_root: &Path, channel_id: u64) -> Presence {
        let dir = dir(runtime_root, channel_id);
        match fs::symlink_metadata(&dir) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Presence::Absent,
            Ok(meta) if meta.is_dir() && fs::read_dir(&dir).is_ok() => Presence::Present,
            _ => Presence::Unreadable,
        }
    }

    pub fn open(runtime_root: &Path, channel_id: u64) -> io::Result<Self> {
        durable::supported()?;
        #[cfg(test)]
        OPENS.with(|opens| opens.set(opens.get() + 1));
        let dir = dir(runtime_root, channel_id);
        durable::ensure_dir(&dir)?;
        let snapshot_path = dir.join("snapshot.json");
        let snapshot: Option<Snapshot> = match durable::open_file(&snapshot_path, false) {
            Ok(file) => Some(serde_json::from_reader(file)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if let Some(snapshot) = &snapshot {
            if snapshot.crc != snapshot.checksum()? {
                return Err(invalid("snapshot checksum mismatch"));
            }
        }
        let generation = snapshot.as_ref().map_or(0, |s| s.generation);
        let path = dir.join(format!("wal.{generation}.jsonl"));
        let wal = Self::open_wal(&path)?;
        let mut ledger = Self {
            dir,
            wal,
            generation,
            seq: snapshot.as_ref().map_or(0, |s| s.seq),
            last_crc: snapshot.as_ref().map_or(0, |s| s.last_crc),
            usable: true,
            snapshot,
            records: Vec::new(),
        };
        ledger.replay()?;
        Ok(ledger)
    }

    fn open_wal(path: &Path) -> io::Result<File> {
        match durable::open_file(path, true) {
            Ok(file) => {
                durable::step("create", path)?;
                durable::sync_wal(&file, path)?;
                durable::sync_dir(path.parent().ok_or_else(|| invalid("missing parent"))?)?;
                Ok(file)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                durable::open_file(path, false)
            }
            Err(error) => Err(error),
        }
    }

    fn wal_path(&self) -> PathBuf {
        self.dir.join(format!("wal.{}.jsonl", self.generation))
    }

    fn replay(&mut self) -> io::Result<()> {
        let mut reader = BufReader::new(self.wal.try_clone()?);
        let mut line = Vec::new();
        let mut valid_len = 0u64;
        while reader.read_until(b'\n', &mut line)? != 0 {
            let record = serde_json::from_slice::<Record>(&line).ok();
            let Some(record) = record.filter(|r| {
                line.last() == Some(&b'\n')
                    && Some(r.seq) == self.seq.checked_add(1)
                    && r.prev_crc == self.last_crc
                    && r.checksum().ok() == Some(r.crc)
            }) else {
                break;
            };
            valid_len += line.len() as u64;
            self.seq = record.seq;
            self.last_crc = record.crc;
            self.records.push(record);
            line.clear();
        }
        if self.wal.metadata()?.len() != valid_len {
            self.wal.set_len(valid_len)?;
            durable::step("truncate", &self.wal_path())?;
        }
        durable::sync_wal(&self.wal, &self.wal_path())?;
        self.wal.seek(SeekFrom::End(0))?;
        Ok(())
    }

    pub fn usable(&self) -> bool {
        self.usable
    }

    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }
    pub fn records(&self) -> &[Record] {
        &self.records
    }

    pub fn append(&mut self, kind: &str, payload: Value, pins: &[BlobPin]) -> io::Result<&Record> {
        if !self.usable {
            return Err(invalid("ledger must be reopened after write failure"));
        }
        for pin in pins {
            self.read_blob(pin)?;
        }
        let mut record = Record {
            seq: self
                .seq
                .checked_add(1)
                .ok_or_else(|| invalid("sequence exhausted"))?,
            prev_crc: self.last_crc,
            kind: kind.to_owned(),
            payload,
            crc: 0,
        };
        record.crc = record.checksum()?;
        let mut bytes = serde_json::to_vec(&record)?;
        let decoded: Record = serde_json::from_slice(&bytes)?;
        if decoded != record || decoded.checksum()? != record.crc {
            return Err(invalid("record JSON does not round-trip through replay"));
        }
        bytes.push(b'\n');
        self.usable = false;
        self.wal.write_all(&bytes)?;
        durable::step("write", &self.wal_path())?;
        durable::sync_wal(&self.wal, &self.wal_path())?;
        self.seq = record.seq;
        self.last_crc = record.crc;
        self.records.push(record);
        self.usable = true;
        self.records
            .last()
            .ok_or_else(|| invalid("missing appended record"))
    }

    // The caller folds snapshot() and records() before supplying the replacement state.
    pub fn checkpoint(&mut self, state: Value) -> io::Result<()> {
        if !self.usable {
            return Err(invalid("ledger must be reopened after write failure"));
        }
        let mut snapshot = Snapshot {
            generation: self
                .generation
                .checked_add(1)
                .ok_or_else(|| invalid("generation exhausted"))?,
            seq: self.seq,
            last_crc: self.last_crc,
            state,
            crc: 0,
        };
        snapshot.crc = snapshot.checksum()?;
        let bytes = serde_json::to_vec(&snapshot)?;
        let decoded: Snapshot = serde_json::from_reader(bytes.as_slice())?;
        if decoded != snapshot || decoded.checksum()? != snapshot.crc {
            return Err(invalid("snapshot JSON does not round-trip through open"));
        }
        self.usable = false;
        durable::atomic_write(&self.dir.join("snapshot.json"), &bytes)?;
        let old = self.wal_path();
        let next = self.dir.join(format!("wal.{}.jsonl", snapshot.generation));
        self.wal = Self::open_wal(&next)?;
        self.generation = snapshot.generation;
        fs::remove_file(&old)?;
        durable::step("remove", &old)?;
        durable::sync_dir(&self.dir)?;
        self.snapshot = Some(snapshot);
        self.records.clear();
        self.usable = true;
        Ok(())
    }
}

/// One channel's ledger access; dropping a handle writes nothing, so a reopen only re-reads.
pub struct LedgerLease {
    root: PathBuf,
    channel: u64,
    ledger: Option<Ledger>,
    /// Set after an internal step error so the next access re-reads the durable state.
    pub needs_reopen: bool,
    #[cfg(test)]
    pub reopens: usize,
}

impl LedgerLease {
    pub(crate) fn new(root: &Path, channel: u64) -> Self {
        Self {
            root: root.to_owned(),
            channel,
            ledger: None,
            needs_reopen: false,
            #[cfg(test)]
            reopens: 0,
        }
    }

    pub fn get(&mut self) -> io::Result<&mut Ledger> {
        if self.needs_reopen || !self.ledger.as_ref().is_some_and(Ledger::usable) {
            return self.reopen();
        }
        self.ledger
            .as_mut()
            .ok_or_else(|| invalid("missing ledger"))
    }

    // The old handle is dropped before the new one opens, so at most one exists.
    pub fn reopen(&mut self) -> io::Result<&mut Ledger> {
        self.ledger = None;
        #[cfg(test)]
        {
            self.reopens += 1;
        }
        let ledger = Ledger::open(&self.root, self.channel)?;
        self.needs_reopen = false;
        Ok(self.ledger.insert(ledger))
    }

    pub fn into_ledger(mut self) -> io::Result<Ledger> {
        self.get()?;
        self.ledger.ok_or_else(|| invalid("missing ledger"))
    }

    pub(crate) fn from_ledger(root: &Path, channel: u64, ledger: Ledger) -> Self {
        let mut lease = Self::new(root, channel);
        lease.ledger = Some(ledger);
        lease
    }
}

#[derive(Debug)]
pub enum SlotError {
    Loaned,
    Io(io::Error),
}

/// A registered channel's only lease; while it is lent out no access can open another handle.
pub struct LedgerSlot {
    root: PathBuf,
    channel: u64,
    lease: Option<LedgerLease>,
}

impl LedgerSlot {
    pub(crate) fn new(root: &Path, channel: u64) -> Self {
        Self {
            root: root.to_owned(),
            channel,
            lease: Some(LedgerLease::new(root, channel)),
        }
    }

    pub fn loaned(&self) -> bool {
        self.lease.is_none()
    }

    pub fn get(&mut self) -> Result<&mut Ledger, SlotError> {
        let lease = self.lease.as_mut().ok_or(SlotError::Loaned)?;
        lease.get().map_err(SlotError::Io)
    }

    pub fn reopen(&mut self) -> Result<&mut Ledger, SlotError> {
        let lease = self.lease.as_mut().ok_or(SlotError::Loaned)?;
        lease.reopen().map_err(SlotError::Io)
    }

    pub fn lend(&mut self) -> Result<LedgerLease, SlotError> {
        self.lease.take().ok_or(SlotError::Loaned)
    }

    pub fn restore(&mut self, lease: LedgerLease) {
        if self.lease.is_none() {
            self.lease = Some(lease);
        }
    }

    // Only after the loan is proven finished: the borrowed handle no longer exists anywhere.
    pub fn restore_fresh(&mut self) {
        self.restore(LedgerLease::new(&self.root, self.channel));
    }

    pub fn restore_ledger(&mut self, ledger: Ledger) {
        self.restore(LedgerLease::from_ledger(&self.root, self.channel, ledger));
    }
}
