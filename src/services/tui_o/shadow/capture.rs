//! Read-only transcript capture: complete lines only, identity and prefix checked each poll.

use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};

use sha2::{Digest, Sha256};

use super::{CaptureBatch, CaptureOutcome, CaptureSource, CapturedRecord, SourceAnomaly};
use super::{SourceAnomalyKind as Kind, SourceId};

/// Bytes re-read behind the cursor each poll to catch in-place rewrites.
const TAIL_GUARD_BYTES: u64 = 4096;

#[cfg(unix)]
pub fn file_identity(meta: &Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

/// Without dev/ino the identity is zero, so replacement is not detectable there.
#[cfg(not(unix))]
pub fn file_identity(_meta: &Metadata) -> (u64, u64) {
    (0, 0)
}

pub struct SourceCapture {
    source: SourceId,
    file: File,
    captured_through: u64,
    prefix: Sha256,
    /// Bytes after `captured_through` still waiting for their newline.
    partial: Vec<u8>,
    /// Last bytes before the read cursor.
    guard: Vec<u8>,
    skip_torn_line: bool,
    halted: Option<SourceAnomaly>,
}

fn read_at(mut file: &File, offset: u64, len: u64) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(len).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Streams `0..len` into a hasher and returns it with the trailing guard bytes.
fn hash_prefix(mut file: &File, len: u64) -> io::Result<(Sha256, Vec<u8>)> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    if io::copy(&mut file.take(len), &mut hasher)? < len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let guard_len = len.min(TAIL_GUARD_BYTES);
    Ok((hasher, read_at(file, len - guard_len, guard_len)?))
}

impl SourceCapture {
    /// Opens `source.path` read-only at `start_offset`; a torn first line is skipped.
    pub fn open(source: SourceId, start_offset: u64) -> io::Result<Self> {
        let file = File::open(&source.path)?;
        let meta = file.metadata()?;
        if file_identity(&meta) != (source.dev, source.ino) || start_offset > meta.len() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let (prefix, guard) = hash_prefix(&file, start_offset)?;
        Ok(Self {
            skip_torn_line: guard.last().is_some_and(|byte| *byte != b'\n'),
            source,
            file,
            captured_through: start_offset,
            prefix,
            partial: Vec::new(),
            guard,
            halted: None,
        })
    }

    pub fn captured_through(&self) -> u64 {
        self.captured_through
    }

    /// Hex sha256 of bytes `0..captured_through`.
    pub fn prefix_hash(&self) -> String {
        hex::encode(self.prefix.clone().finalize())
    }

    /// Rehashes `0..captured_through` from disk; polls only re-check the tail guard.
    pub fn verify_prefix(&self) -> io::Result<bool> {
        let (disk, _) = hash_prefix(&self.file, self.captured_through)?;
        Ok(hex::encode(disk.finalize()) == self.prefix_hash())
    }

    fn poll_inner(&mut self, max_bytes: u64) -> Result<CaptureBatch, (Kind, String)> {
        let io_err = |error: io::Error| (Kind::Unreadable, error.to_string());
        let len = self.file.metadata().map_err(io_err)?.len();
        let path_identity = std::fs::metadata(&self.source.path).map(|meta| file_identity(&meta));
        if path_identity.ok() != Some((self.source.dev, self.source.ino)) {
            return Err((Kind::Replaced, "path no longer names the open file".into()));
        }
        let cursor = self.captured_through + self.partial.len() as u64;
        if len < cursor {
            return Err((Kind::Shrunk, format!("len {len} < cursor {cursor}")));
        }
        let guard_len = self.guard.len() as u64;
        if read_at(&self.file, cursor - guard_len, guard_len).map_err(io_err)? != self.guard {
            return Err((
                Kind::PrefixMismatch,
                "bytes behind the cursor changed".into(),
            ));
        }
        let chunk = read_at(&self.file, cursor, (len - cursor).min(max_bytes)).map_err(io_err)?;
        if self.file.metadata().map_err(io_err)?.len() < cursor + chunk.len() as u64 {
            return Err((Kind::Shrunk, "shrank during read".into()));
        }
        self.guard.extend_from_slice(&chunk);
        let excess = self.guard.len().saturating_sub(TAIL_GUARD_BYTES as usize);
        self.guard.drain(..excess);
        self.partial.extend_from_slice(&chunk);
        let (mut records, mut consumed) = (Vec::new(), 0);
        while let Some(pos) = self.partial[consumed..].iter().position(|b| *b == b'\n') {
            let line = &self.partial[consumed..consumed + pos + 1];
            let start = self.captured_through + consumed as u64;
            self.prefix.update(line);
            consumed += line.len();
            if !std::mem::take(&mut self.skip_torn_line) {
                let (end, line) = (start + line.len() as u64, line[..pos].to_vec());
                records.push(CapturedRecord { start, end, line });
            }
        }
        self.partial.drain(..consumed);
        self.captured_through += consumed as u64;
        let (source, captured_through) = (self.source.clone(), self.captured_through);
        Ok(CaptureBatch {
            source,
            records,
            captured_through,
        })
    }
}

impl CaptureSource for SourceCapture {
    fn source(&self) -> &SourceId {
        &self.source
    }

    /// The first anomaly halts the source; later polls repeat it.
    fn poll(&mut self, max_bytes: u64) -> CaptureOutcome {
        if let Some(anomaly) = &self.halted {
            return CaptureOutcome::Anomaly(anomaly.clone());
        }
        let (kind, detail) = match self.poll_inner(max_bytes) {
            Ok(batch) => return CaptureOutcome::Batch(batch),
            Err(failure) => failure,
        };
        let (source, captured_through) = (self.source.clone(), self.captured_through);
        let anomaly = SourceAnomaly {
            source,
            kind,
            captured_through,
            detail,
        };
        self.halted = Some(anomaly.clone());
        CaptureOutcome::Anomaly(anomaly)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{MAX_READ_BYTES as MAX, binding_reader::source_id_for};
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    const FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/tui_o_shadow/capture_growth.jsonl"
    ));

    fn open_fixture(start: u64) -> (tempfile::TempDir, PathBuf, SourceCapture) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, FIXTURE).unwrap();
        let capture = SourceCapture::open(source_id_for("s", &path).unwrap(), start).unwrap();
        (dir, path, capture)
    }

    fn poll(capture: &mut SourceCapture, max: u64) -> Result<Vec<CapturedRecord>, Kind> {
        match capture.poll(max) {
            CaptureOutcome::Batch(batch) => Ok(batch.records),
            CaptureOutcome::Anomaly(anomaly) => Err(anomaly.kind),
        }
    }

    fn line_ends() -> Vec<u64> {
        let newlines = FIXTURE.iter().enumerate().filter(|(_, b)| **b == b'\n');
        newlines.map(|(i, _)| i as u64 + 1).collect()
    }

    #[test]
    fn capture_emits_complete_lines_only_and_tracks_the_prefix_hash() {
        let (_dir, path, mut capture) = open_fixture(0);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(br#"{"type":"assistant","#).unwrap();
        let first = poll(&mut capture, MAX).unwrap();
        assert_eq!(first.iter().map(|r| r.end).collect::<Vec<_>>(), line_ends());
        assert_eq!(
            first[1].line,
            FIXTURE.split(|b| *b == b'\n').nth(1).unwrap()
        );

        file.write_all(b"\"done\":1}\n").unwrap();
        let second = poll(&mut capture, MAX).unwrap();
        assert_eq!(second[0].line, br#"{"type":"assistant","done":1}"#);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(capture.prefix_hash(), hex::encode(Sha256::digest(&bytes)));
        assert!(capture.verify_prefix().unwrap());
    }

    #[test]
    fn capture_keeps_record_boundaries_under_small_budgets_and_mid_line_starts() {
        let (_dir, _path, mut capture) = open_fixture(0);
        let polls = (0..FIXTURE.len()).flat_map(|_| poll(&mut capture, 7).unwrap());
        assert_eq!(polls.map(|r| r.end).collect::<Vec<_>>(), line_ends());

        let (_dir, _path, mut mid) = open_fixture(3);
        let starts: Vec<u64> = poll(&mut mid, MAX)
            .unwrap()
            .iter()
            .map(|r| r.start)
            .collect();
        assert_eq!(starts, line_ends()[..line_ends().len() - 1]);
    }

    #[test]
    fn capture_halts_on_shrink_rewrite_and_replacement() {
        let (_dir, path, mut shrunk) = open_fixture(0);
        poll(&mut shrunk, MAX).unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(5).unwrap();
        assert_eq!(poll(&mut shrunk, MAX), Err(Kind::Shrunk));
        std::fs::write(&path, FIXTURE).unwrap();
        assert_eq!(
            poll(&mut shrunk, MAX),
            Err(Kind::Shrunk),
            "a halted source stays halted"
        );

        let (_dir, path, mut rewritten) = open_fixture(0);
        poll(&mut rewritten, MAX).unwrap();
        let mut bytes = FIXTURE.to_vec();
        bytes[FIXTURE.len() - 3] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(poll(&mut rewritten, MAX), Err(Kind::PrefixMismatch));

        #[cfg(unix)]
        {
            let (dir, path, mut replaced) = open_fixture(0);
            std::fs::write(dir.path().join("new.jsonl"), FIXTURE).unwrap();
            std::fs::rename(dir.path().join("new.jsonl"), &path).unwrap();
            assert_eq!(poll(&mut replaced, MAX), Err(Kind::Replaced));
        }
    }
}
