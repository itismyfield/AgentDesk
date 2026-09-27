//! The shadow's only write target: `<runtime_root>/o_shadow/`.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use super::{ShadowRecord, ShadowSink};

pub const SHADOW_DIR_NAME: &str = "o_shadow";
pub const RECORDS_FILE_NAME: &str = "records.jsonl";

/// Directory fixed to `<runtime_root>/o_shadow` at construction; no other path is writable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowRoot(PathBuf);

impl ShadowRoot {
    pub fn under(runtime_root: &Path) -> io::Result<Self> {
        let _ = runtime_root;
        todo!()
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn records_path(&self) -> PathBuf {
        self.0.join(RECORDS_FILE_NAME)
    }
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
        let _ = (root, cap_bytes);
        todo!()
    }

    pub fn root(&self) -> &ShadowRoot {
        &self.root
    }

    pub fn dropped_over_cap(&self) -> u64 {
        self.dropped_over_cap
    }

    pub fn read_records(root: &ShadowRoot) -> io::Result<Vec<ShadowRecord>> {
        let _ = root;
        todo!()
    }
}

impl ShadowSink for ShadowStore {
    fn append(&mut self, record: &ShadowRecord) -> io::Result<()> {
        let _ = (record, &self.file, self.written, self.cap_bytes);
        todo!()
    }
}
