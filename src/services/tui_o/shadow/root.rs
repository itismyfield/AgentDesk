//! The shadow's only write target: `<runtime_root>/o_shadow/`.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ShadowRecord, ShadowSink};

pub const SHADOW_DIR_NAME: &str = "o_shadow";
pub const RECORDS_FILE_NAME: &str = "records.jsonl";

/// `<runtime_root>/o_shadow`, accepted only while it is a real directory, not a symlink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowRoot(PathBuf);

fn real_dir(dir: &Path) -> io::Result<()> {
    let is_dir = std::fs::symlink_metadata(dir)?.file_type().is_dir();
    is_dir
        .then_some(())
        .ok_or_else(|| io::Error::other("o_shadow is not a real directory"))
}

impl ShadowRoot {
    /// Creates only `o_shadow` itself; a symlink or non-directory there is refused.
    pub fn under(runtime_root: &Path) -> io::Result<Self> {
        let dir = runtime_root.join(SHADOW_DIR_NAME);
        match std::fs::DirBuilder::new().create(&dir) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => Err(error),
            _ => real_dir(&dir).map(|()| Self(dir)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn records_path(&self) -> PathBuf {
        self.0.join(RECORDS_FILE_NAME)
    }
}

/// Parsed records plus the 1-based numbers of lines that did not parse.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoredLog {
    pub records: Vec<StoredRecord>,
    pub damaged_lines: Vec<usize>,
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
    /// Opens the records file without following symlinks and seals a torn last line.
    pub fn open(root: ShadowRoot, cap_bytes: u64) -> io::Result<Self> {
        real_dir(root.path())?;
        let path = root.records_path();
        if std::fs::symlink_metadata(&path).is_ok_and(|meta| !meta.file_type().is_file()) {
            return Err(io::Error::other("records.jsonl is not a regular file"));
        }
        let mut options = OpenOptions::new();
        options.read(true).append(true).create(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NOFOLLOW);
        let mut file = options.open(path)?;
        let mut written = file.metadata()?.len();
        let mut last = [b'\n'];
        if written > 0 {
            file.seek(SeekFrom::End(-1))?;
            file.read_exact(&mut last)?;
        }
        // A crash can leave a torn last line; a newline keeps later appends parseable.
        if last[0] != b'\n' {
            file.write_all(b"\n")?;
            written += 1;
        }
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

    /// Strict read: any damaged line is an error; `read_stored` reports damage instead.
    pub fn read_records(root: &ShadowRoot) -> io::Result<Vec<ShadowRecord>> {
        let log = Self::read_stored(root)?;
        if let Some(line) = log.damaged_lines.first() {
            let detail = format!("records line {line} does not parse");
            return Err(io::Error::new(io::ErrorKind::InvalidData, detail));
        }
        Ok(log.records.into_iter().map(|line| line.record).collect())
    }

    /// Reads every line and lists the ones that do not parse, such as a sealed torn tail.
    pub fn read_stored(root: &ShadowRoot) -> io::Result<StoredLog> {
        let mut log = StoredLog::default();
        if !root.records_path().exists() {
            return Ok(log);
        }
        for (index, line) in BufReader::new(File::open(root.records_path())?)
            .lines()
            .enumerate()
        {
            match serde_json::from_str::<StoredRecord>(&line?) {
                Ok(stored) => log.records.push(stored),
                Err(_) => log.damaged_lines.push(index + 1),
            }
        }
        Ok(log)
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

    #[test]
    fn store_seals_a_torn_tail_so_new_appends_survive_and_reports_the_damage() {
        let runtime = tempfile::tempdir().unwrap();
        let root = ShadowRoot::under(runtime.path()).unwrap();
        std::fs::write(root.records_path(), br#"{"at":"#).unwrap();
        let record = ShadowRecord::TapGap { dropped: 42 };
        let mut store = ShadowStore::open(root.clone(), 4096).unwrap();
        store.append(&record).unwrap();
        let log = ShadowStore::read_stored(&root).unwrap();
        assert_eq!((log.records.len(), log.damaged_lines), (1, vec![1]));
        assert_eq!(log.records[0].record, record);
        assert!(ShadowStore::read_records(&root).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn shadow_root_refuses_symlinks_and_leaves_outside_files_untouched() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let (outside, absent) = (dir.path().join("outside"), dir.path().join("absent"));
        std::fs::write(&outside, b"sentinel\n").unwrap();
        for (runtime, target) in [("linked", &outside), ("dangling", &absent)] {
            std::fs::create_dir(dir.path().join(runtime)).unwrap();
            let root = ShadowRoot::under(&dir.path().join(runtime)).unwrap();
            symlink(target, root.records_path()).unwrap();
            assert!(ShadowStore::open(root, 4096).is_err());
        }
        std::fs::create_dir(dir.path().join("rooted")).unwrap();
        symlink(dir.path(), dir.path().join("rooted/o_shadow")).unwrap();
        assert!(ShadowRoot::under(&dir.path().join("rooted")).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"sentinel\n");
        assert!(!absent.exists());
    }
}
