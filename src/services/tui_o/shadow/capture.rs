//! Read-only transcript capture: complete lines only, identity and prefix checked each poll.

use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};

use sha2::{Digest, Sha256};

use super::{CaptureBatch, CaptureOutcome, CaptureSource, CapturedRecord, SourceAnomaly};
use super::{SourceAnomalyKind as Kind, SourceId};

/// Bytes re-read behind the cursor each poll to catch in-place rewrites.
const TAIL_GUARD_BYTES: u64 = 4096;
/// A record still missing its newline past this size halts the source instead of growing memory.
pub const MAX_PARTIAL_BYTES: usize = 16 * 1024 * 1024;

#[cfg(unix)]
pub fn file_identity(meta: &Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let (dev, ino) = (meta.dev(), meta.ino());
    #[cfg(test)]
    let dev = renumber::dev(dev, ino);
    (dev, ino)
}

/// Without dev/ino the identity is zero, so replacement is not detectable there.
#[cfg(not(unix))]
pub fn file_identity(_meta: &Metadata) -> (u64, u64) {
    (0, 0)
}

/// How a live file compares with a stored source: a reboot may renumber the volume's dev while
/// the inode stays, so `Renumbered` is the same file only once its bytes prove it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileMatch {
    Exact,
    Renumbered,
    Other,
}

pub fn file_match(source: &SourceId, (dev, ino): (u64, u64)) -> FileMatch {
    match (ino == source.ino, dev == source.dev) {
        (true, true) => FileMatch::Exact,
        (true, false) => FileMatch::Renumbered,
        (false, _) => FileMatch::Other,
    }
}

/// Whether two sources name one file, the volume's dev aside.
pub fn same_file(a: &SourceId, b: &SourceId) -> bool {
    (&a.session_id, &a.path, a.ino) == (&b.session_id, &b.path, b.ino)
}

/// Renumbers chosen files' dev as a reboot would, keyed by their real (dev, ino).
#[cfg(all(test, unix))]
pub(crate) mod renumber {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::{LazyLock, Mutex};

    static SHIFTED: LazyLock<Mutex<HashMap<(u64, u64), u64>>> = LazyLock::new(Default::default);

    pub(crate) fn dev(dev: u64, ino: u64) -> u64 {
        let shifted = SHIFTED.lock().unwrap_or_else(|e| e.into_inner());
        shifted.get(&(dev, ino)).map_or(dev, |shift| dev ^ shift)
    }

    /// Live stats of `path`'s inode report a dev XORed with `shift` until the guard drops. Keep
    /// the inode alive meanwhile, so no other test's new file reuses it.
    pub(crate) fn shift(path: &Path, shift: u64) -> Guard {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).unwrap();
        let key = (meta.dev(), meta.ino());
        SHIFTED.lock().unwrap().insert(key, shift);
        Guard(key)
    }

    pub(crate) struct Guard((u64, u64));

    impl Drop for Guard {
        fn drop(&mut self) {
            SHIFTED
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.0);
        }
    }
}

pub struct SourceCapture {
    source: SourceId,
    file: File,
    /// The open file's own identity; a renumbered source keeps its stored dev, this the live one.
    opened: (u64, u64),
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
        Self::opened(source, file, &meta, start_offset)
    }

    /// Reopens a stored source at its cursor. A renumbered dev is accepted like an exact one, as
    /// the bytes before `through` must hash to `prefix_hash` either way (`InvalidData` if not).
    pub fn reopen(source: SourceId, through: u64, prefix_hash: &str) -> io::Result<Self> {
        let invalid = |detail: String| io::Error::new(io::ErrorKind::InvalidInput, detail);
        let file = File::open(&source.path)?;
        let meta = file.metadata()?;
        let (dev, ino) = file_identity(&meta);
        if file_match(&source, (dev, ino)) == FileMatch::Other {
            let detail = format!("source replaced: inode {ino}, cursor names {}", source.ino);
            return Err(invalid(detail));
        }
        if through > meta.len() {
            let detail = format!("source holds {} bytes, cursor at {through}", meta.len());
            return Err(invalid(detail));
        }
        let renumbered = dev != source.dev;
        let opened = Self::opened(source, file, &meta, through)?;
        if opened.prefix_hash() != prefix_hash {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "source bytes before the cursor changed",
            ));
        }
        if renumbered {
            let (path, old_dev) = (opened.source.path.display(), opened.source.dev);
            tracing::info!(%path, old_dev, new_dev = dev, "[tui_o] source volume renumbered");
        }
        Ok(opened)
    }

    fn opened(source: SourceId, file: File, meta: &Metadata, start: u64) -> io::Result<Self> {
        let (prefix, guard) = hash_prefix(&file, start)?;
        Ok(Self {
            skip_torn_line: guard.last().is_some_and(|byte| *byte != b'\n'),
            source,
            file,
            opened: file_identity(meta),
            captured_through: start,
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
    /// Bytes read so far, including a buffered line still missing its newline.
    pub fn read_through(&self) -> u64 {
        self.captured_through + self.partial.len() as u64
    }

    pub fn prefix_hash(&self) -> String {
        hex::encode(self.prefix.clone().finalize())
    }

    /// The open file's length now, even after its path is renamed.
    pub fn file_len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
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
        if path_identity.ok() != Some(self.opened) {
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
        let last_newline = chunk.iter().rposition(|b| *b == b'\n');
        let rest = last_newline.map_or(self.partial.len() + chunk.len(), |at| chunk.len() - at - 1);
        if rest > MAX_PARTIAL_BYTES {
            return Err((
                Kind::Oversized,
                format!("record without newline passed {rest} bytes"),
            ));
        }
        // A line spanning polls is re-read once before emission; the tail guard only covers 4 KiB.
        if last_newline.is_some() && !self.partial.is_empty() {
            let disk = read_at(&self.file, self.captured_through, self.partial.len() as u64);
            if disk.map_err(io_err)? != self.partial {
                return Err((Kind::PrefixMismatch, "buffered partial line changed".into()));
            }
        }
        self.guard.extend_from_slice(&chunk);
        let excess = self.guard.len().saturating_sub(TAIL_GUARD_BYTES as usize);
        self.guard.drain(..excess);
        let mut search = self.partial.len();
        self.partial.extend_from_slice(&chunk);
        let (mut records, mut consumed) = (Vec::new(), 0);
        while let Some(pos) = self.partial[search..].iter().position(|b| *b == b'\n') {
            let line = &self.partial[consumed..search + pos + 1];
            let start = self.captured_through + consumed as u64;
            self.prefix.update(line);
            (consumed, search) = (search + pos + 1, search + pos + 1);
            if !std::mem::take(&mut self.skip_torn_line) {
                let (end, line) = (start + line.len() as u64, line[..line.len() - 1].to_vec());
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

    #[test]
    fn capture_rereads_a_buffered_partial_line_before_emitting_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial.jsonl");
        let mut bytes = vec![b'a'; 2 * TAIL_GUARD_BYTES as usize];
        std::fs::write(&path, &bytes).unwrap();
        let mut capture = SourceCapture::open(source_id_for("p", &path).unwrap(), 0).unwrap();
        assert_eq!(poll(&mut capture, MAX), Ok(vec![]));
        bytes[0] = b'b';
        bytes.push(b'\n');
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(poll(&mut capture, MAX), Err(Kind::PrefixMismatch));
    }

    #[test]
    fn capture_halts_instead_of_buffering_an_oversized_partial_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.jsonl");
        std::fs::write(&path, vec![b'a'; MAX_PARTIAL_BYTES]).unwrap();
        let mut capture = SourceCapture::open(source_id_for("h", &path).unwrap(), 0).unwrap();
        let budget = 2 * MAX_PARTIAL_BYTES as u64;
        assert_eq!(poll(&mut capture, budget), Ok(vec![]));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"a").unwrap();
        assert_eq!(poll(&mut capture, budget), Err(Kind::Oversized));
    }
}
