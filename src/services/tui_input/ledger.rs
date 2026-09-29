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

#[derive(Clone, Debug, Serialize, Deserialize)]
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

impl Ledger {
    pub fn open(runtime_root: &Path, channel_id: u64) -> io::Result<Self> {
        durable::supported()?;
        let dir = runtime_root
            .join("input_ledger")
            .join(channel_id.to_string());
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
            durable::sync_wal(&self.wal, &self.wal_path())?;
        }
        self.wal.seek(SeekFrom::End(0))?;
        Ok(())
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
        self.usable = false;
        durable::atomic_write(
            &self.dir.join("snapshot.json"),
            &serde_json::to_vec(&snapshot)?,
        )?;
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
