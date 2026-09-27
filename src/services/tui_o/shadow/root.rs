//! The shadow's only write target: `<runtime_root>/o_shadow/`.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ShadowRecord, ShadowSink};

pub const SHADOW_DIR_NAME: &str = "o_shadow";
pub const RECORDS_FILE_NAME: &str = "records.jsonl";

/// Directory fixed to `<runtime_root>/o_shadow` at construction; no other path is writable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowRoot(PathBuf);

impl ShadowRoot {
    pub fn under(runtime_root: &Path) -> io::Result<Self> {
        let dir = runtime_root.join(SHADOW_DIR_NAME);
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn records_path(&self) -> PathBuf {
        self.0.join(RECORDS_FILE_NAME)
    }
}

/// One JSONL line: the record and when the shadow stored it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredRecord {
    pub at: DateTime<Utc>,
    pub record: ShadowRecord,
}

/// Append-only JSONL store under a `ShadowRoot`, stopped at a byte cap.
pub struct ShadowStore {
    root: ShadowRoot,
    file: File,
    written: u64,
    cap_bytes: u64,
    dropped_over_cap: u64,
}

impl ShadowStore {
    pub fn open(root: ShadowRoot, cap_bytes: u64) -> io::Result<Self> {
        let path = root.records_path();
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let written = file.metadata()?.len();
        let dropped_over_cap = 0;
        Ok(Self {
            root,
            file,
            written,
            cap_bytes,
            dropped_over_cap,
        })
    }

    pub fn root(&self) -> &ShadowRoot {
        &self.root
    }

    pub fn dropped_over_cap(&self) -> u64 {
        self.dropped_over_cap
    }

    pub fn read_records(root: &ShadowRoot) -> io::Result<Vec<ShadowRecord>> {
        let stored = Self::read_stored(root)?;
        Ok(stored.into_iter().map(|line| line.record).collect())
    }

    /// Reads every stored line; a torn final line from a crash is skipped.
    pub fn read_stored(root: &ShadowRoot) -> io::Result<Vec<StoredRecord>> {
        let file = match File::open(root.records_path()) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            opened => opened?,
        };
        let mut stored = Vec::new();
        for line in BufReader::new(file).lines() {
            stored.extend(serde_json::from_str::<StoredRecord>(&line?).ok());
        }
        Ok(stored)
    }
}

impl ShadowSink for ShadowStore {
    fn append(&mut self, record: &ShadowRecord) -> io::Result<()> {
        let stored = StoredRecord {
            at: Utc::now(),
            record: record.clone(),
        };
        let mut line = serde_json::to_vec(&stored).map_err(io::Error::other)?;
        line.push(b'\n');
        if self.written.saturating_add(line.len() as u64) > self.cap_bytes {
            self.dropped_over_cap += 1;
            return Err(io::Error::other("o_shadow disk cap reached"));
        }
        // One write per line keeps O_APPEND lines whole.
        self.file.write_all(&line)?;
        self.written += line.len() as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{DiffCause, DiffClass, DiffRecord, ShadowConfig};
    use super::*;

    #[test]
    fn shadow_root_is_fixed_under_runtime_root_and_the_flag_defaults_off() {
        let runtime = tempfile::tempdir().unwrap();
        let root = ShadowRoot::under(runtime.path()).unwrap();
        assert_eq!(root.path(), runtime.path().join("o_shadow"));
        assert!(root.path().is_dir());
        assert!(!serde_json::from_str::<ShadowConfig>("{}").unwrap().enabled);
    }

    #[test]
    fn store_appends_readable_jsonl_and_stops_at_the_cap() {
        let runtime = tempfile::tempdir().unwrap();
        let root = ShadowRoot::under(runtime.path()).unwrap();
        let (class, cause) = (DiffClass::LegacyExtra, DiffCause::ODefect);
        let diff = DiffRecord {
            channel_id: 1,
            unit_key: None,
            class,
            legacy_msg_ids: vec![7],
            cause,
        };
        let records = vec![
            ShadowRecord::Diff { diff },
            ShadowRecord::TapGap { dropped: 3 },
        ];
        let mut store = ShadowStore::open(root.clone(), 4096).unwrap();
        records
            .iter()
            .for_each(|record| store.append(record).unwrap());
        let text = std::fs::read_to_string(root.records_path()).unwrap();
        assert!(text.contains(r#""type":"diff""#) && text.contains(r#""cause":"O_defect""#));
        assert_eq!(ShadowStore::read_records(&root).unwrap(), records);

        let mut capped = ShadowStore::open(root.clone(), text.len() as u64 + 10).unwrap();
        assert!(capped.append(&records[0]).is_err());
        assert_eq!(capped.dropped_over_cap(), 1);
        assert_eq!(ShadowStore::read_records(&root).unwrap(), records);
    }
}
