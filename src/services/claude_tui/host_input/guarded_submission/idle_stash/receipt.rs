//! The first complete user record after a pinned transcript offset decides the receipt.

use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub(super) struct Receipt {
    file: File,
    path: PathBuf,
    identity: (u64, u64),
    offset: u64,
    prefix: [u8; 32],
}

impl Receipt {
    pub(super) fn before(path: &Path) -> Option<Self> {
        let mut file = File::open(path).ok()?;
        let metadata = file.metadata().ok()?;
        let offset = metadata.len();
        if offset > 0 {
            file.seek(SeekFrom::Start(offset - 1)).ok()?;
            let mut last = [0];
            file.read_exact(&mut last).ok()?;
            if last != [b'\n'] {
                return None;
            }
        }
        let prefix = fingerprint(&mut file, offset)?;
        Some(Self {
            file,
            path: path.into(),
            identity: (metadata.dev(), metadata.ino()),
            offset,
            prefix,
        })
    }
    fn length(&self) -> Option<u64> {
        let metadata = std::fs::metadata(&self.path).ok()?;
        let held = self.file.metadata().ok()?;
        ((metadata.dev(), metadata.ino()) == self.identity && held.len() >= self.offset)
            .then_some(held.len())
    }
    pub(super) fn unchanged(&self) -> bool {
        self.length() == Some(self.offset)
    }
    pub(super) fn scan(&self, expected: &str) -> Option<bool> {
        if self.length().is_none() {
            return Some(false);
        }
        let mut file = self.file.try_clone().ok()?;
        if fingerprint(&mut file, self.offset)? != self.prefix {
            return Some(false);
        }
        file.seek(SeekFrom::Start(self.offset)).ok()?;
        let mut reader = std::io::BufReader::new(file);
        let mut line = Vec::new();
        loop {
            if reader.read_until(b'\n', &mut line).ok()? == 0 {
                return None;
            }
            if !line.ends_with(b"\n") {
                return None;
            }
            let Ok(record) = serde_json::from_slice::<serde_json::Value>(&line) else {
                return Some(false);
            };
            if record.get("type").and_then(|v| v.as_str()) == Some("user")
                && record.get("isMeta").and_then(|v| v.as_bool()) != Some(true)
            {
                return Some(record.pointer("/message/content").is_some_and(|content| {
                    match content {
                        serde_json::Value::String(text) => {
                            crate::services::claude_tui::busy_inject::exact_receipt(text, expected)
                        }
                        serde_json::Value::Array(blocks) => {
                            !blocks.is_empty()
                                && blocks
                                    .iter()
                                    .all(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"))
                                && blocks
                                    .iter()
                                    .map(|b| b.get("text").and_then(|v| v.as_str()))
                                    .collect::<Option<Vec<_>>>()
                                    .is_some_and(|rows| {
                                        crate::services::claude_tui::busy_inject::exact_receipt(
                                            &rows.join("\n"),
                                            expected,
                                        )
                                    })
                        }
                        _ => false,
                    }
                }));
            }
            line.clear();
        }
    }
}

fn fingerprint(file: &mut File, offset: u64) -> Option<[u8; 32]> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut remaining = offset;
    let mut buffer = [0; 8192];
    let mut hash = Sha256::new();
    while remaining > 0 {
        let wanted = remaining.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..wanted]).ok()?;
        hash.update(&buffer[..wanted]);
        remaining -= wanted as u64;
    }
    Some(hash.finalize().into())
}
