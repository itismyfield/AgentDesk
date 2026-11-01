//! Read-only transcript capture: complete lines only, identity and prefix checked each poll.

use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::SystemTime;

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub len: u64,
    pub dev: u64,
    pub ino: u64,
    pub modified: SystemTime,
    pub change: (i64, i64),
}

impl Fingerprint {
    pub fn of(meta: &Metadata) -> io::Result<Self> {
        let (dev, ino) = file_identity(meta);
        #[cfg(unix)]
        let change = {
            use std::os::unix::fs::MetadataExt;
            (meta.ctime(), meta.ctime_nsec())
        };
        #[cfg(not(unix))]
        let change = (0, 0);
        Ok(Self {
            len: meta.len(),
            dev,
            ino,
            modified: meta.modified()?,
            change,
        })
    }

    pub fn path(source: &SourceId) -> Result<Self, (Kind, String)> {
        let unreadable = |e: io::Error| (Kind::Unreadable, e.to_string());
        let meta = match std::fs::metadata(&source.path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(match std::fs::symlink_metadata(&source.path) {
                    Err(entry) if entry.kind() == io::ErrorKind::NotFound => {
                        (Kind::Missing, e.to_string())
                    }
                    Ok(_) => (
                        Kind::Replaced,
                        "pathname entry no longer resolves to the source".into(),
                    ),
                    Err(entry) => unreadable(entry),
                });
            }
            Err(e) => return Err(unreadable(e)),
        };
        if file_match(source, file_identity(&meta)) == FileMatch::Other {
            return Err((Kind::Replaced, "path names a different inode".into()));
        }
        Self::of(&meta).map_err(unreadable)
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
    #[cfg(all(test, unix))]
    self::io_trace::record_source(file, "source_capture", bytes.len() as u64);
    #[cfg(test)]
    self::io_trace::record(offset, bytes.len() as u64, false);
    Ok(bytes)
}

/// Streams `0..len` into a hasher and returns it with the trailing guard bytes.
fn hash_prefix(mut file: &File, len: u64) -> io::Result<(Sha256, Vec<u8>)> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let copied = io::copy(&mut file.take(len), &mut hasher)?;
    #[cfg(all(test, unix))]
    self::io_trace::record_source(file, "source_prefix", copied);
    #[cfg(test)]
    self::io_trace::record(0, copied, true);
    if copied < len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let guard_len = len.min(TAIL_GUARD_BYTES);
    Ok((hasher, read_at(file, len - guard_len, guard_len)?))
}

fn update_guard(guard: &mut Vec<u8>, chunk: &[u8]) {
    guard.extend_from_slice(chunk);
    let excess = guard.len().saturating_sub(TAIL_GUARD_BYTES as usize);
    guard.drain(..excess);
}

impl SourceCapture {
    /// Opens `source.path` read-only at `start_offset`; a torn first line is skipped. A renumbered
    /// dev passes, as in each poll's `Fingerprint::path`; the capture keeps the stored source.
    pub fn open(source: SourceId, start_offset: u64) -> io::Result<Self> {
        let file = File::open(&source.path)?;
        let meta = file.metadata()?;
        let replaced = file_match(&source, file_identity(&meta)) == FileMatch::Other;
        if replaced || start_offset > meta.len() {
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

    /// Bytes read so far, including a buffered line still missing its newline.
    pub fn read_through(&self) -> u64 {
        self.captured_through + self.partial.len() as u64
    }

    /// Hex sha256 of bytes `0..captured_through`.
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

    pub fn fingerprint(&self) -> io::Result<Fingerprint> {
        Fingerprint::of(&self.file.metadata()?)
    }

    /// Checks the retained FD even when its pathname has been unlinked.
    pub fn verify_eof(&self, through: u64, hash: &str) -> io::Result<Fingerprint> {
        let invalid = |detail| io::Error::new(io::ErrorKind::InvalidData, detail);
        if self.captured_through != through
            || self.read_through() != through
            || self.prefix_hash() != hash
        {
            return Err(invalid("capture differs from the drained durable cursor"));
        }
        let before = self.fingerprint()?;
        if before.len != through
            || (before.dev, before.ino) != self.opened
            || !self.verify_prefix()?
        {
            return Err(invalid("open source is not at its verified EOF"));
        }
        if before != self.fingerprint()? {
            return Err(invalid("metadata changed during EOF validation"));
        }
        Ok(before)
    }

    fn poll_inner(
        &mut self,
        max_bytes: u64,
        retained: bool,
    ) -> Result<CaptureBatch, (Kind, String)> {
        let io_err = |error: io::Error| (Kind::Unreadable, error.to_string());
        let len = self.file.metadata().map_err(io_err)?.len();
        let path = if retained {
            Fingerprint::of(&self.file.metadata().map_err(io_err)?).map_err(io_err)?
        } else {
            Fingerprint::path(&self.source)?
        };
        if (path.dev, path.ino) != self.opened {
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
        update_guard(&mut self.guard, &chunk);
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

impl SourceCapture {
    /// Verified ownership retries path races; rotated sources still prove the retained FD and prefix.
    pub(crate) fn poll_verified(&mut self, max_bytes: u64, retained: bool) -> CaptureOutcome {
        if self
            .halted
            .as_ref()
            .is_some_and(|anomaly| matches!(anomaly.kind, Kind::Missing | Kind::Replaced))
        {
            self.halted = None;
        }
        self.poll_checked(max_bytes, retained)
    }

    fn poll_checked(&mut self, max_bytes: u64, retained: bool) -> CaptureOutcome {
        if let Some(anomaly) = &self.halted {
            return CaptureOutcome::Anomaly(anomaly.clone());
        }
        let (kind, detail) = match self.poll_inner(max_bytes, retained) {
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

impl CaptureSource for SourceCapture {
    fn source(&self) -> &SourceId {
        &self.source
    }

    fn poll(&mut self, max_bytes: u64) -> CaptureOutcome {
        self.poll_checked(max_bytes, false)
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
    #[cfg(unix)]
    #[test]
    fn readers_s1a_access_error_is_unreadable_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source");
        std::fs::write(&path, b"line\n").unwrap();
        let source = source_id_for("session", &path).unwrap();
        let mut capture = SourceCapture::open(source, 0).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&path, &path).unwrap();
        assert!(
            std::fs::metadata(&path).is_err(),
            "fixture really fails to stat"
        );
        assert_eq!(poll(&mut capture, MAX), Err(Kind::Unreadable));
    }
    #[test]
    #[cfg(unix)]
    fn readers_s1a_missing_is_exact_and_first_anomaly_is_sticky() {
        use std::os::unix::fs::symlink;
        let (dir, path, mut capture) = open_fixture(0);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(poll(&mut capture, MAX), Err(Kind::Missing));
        let mut capture = {
            std::fs::write(&path, FIXTURE).unwrap();
            SourceCapture::open(source_id_for("s", &path).unwrap(), 0).unwrap()
        };
        std::fs::rename(&path, dir.path().join("retained")).unwrap();
        symlink(dir.path().join("absent"), &path).unwrap();
        assert_eq!(poll(&mut capture, MAX), Err(Kind::Replaced));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(poll(&mut capture, MAX), Err(Kind::Replaced));
    }

    #[test]
    fn readers_s1a_open_fd_verification_rejects_full_prefix_change_and_partial() {
        let (_dir, path, mut capture) = open_fixture(0);
        poll(&mut capture, MAX).unwrap();
        let through = capture.captured_through();
        let hash = capture.prefix_hash();
        assert!(capture.verify_eof(through, &hash).is_ok());
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"partial")
            .unwrap();
        poll(&mut capture, MAX).unwrap();
        assert!(capture.verify_eof(through, &hash).is_err());
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 1;
        std::fs::write(&path, &bytes[..through as usize]).unwrap();
        assert!(!capture.verify_prefix().unwrap());
    }
    #[test]
    #[cfg(unix)]
    fn readers_s1a_permission_denied_is_never_missing() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, path, mut capture) = open_fixture(0);
        let prior = std::fs::metadata(dir.path()).unwrap().permissions();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o0)).unwrap();
        let raw = std::fs::metadata(&path);
        let result = poll(&mut capture, MAX);
        std::fs::set_permissions(dir.path(), prior).unwrap();
        assert_eq!(raw.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(result, Err(Kind::Unreadable));
    }
}

pub mod observation;

#[cfg(test)]
pub(crate) mod io_trace {
    use std::cell::RefCell;
    #[cfg(unix)]
    pub(crate) fn record_source(file: &std::fs::File, tag: &'static str, len: u64) {
        crate::services::codex::actual_submit_tests::source_io::read(file, tag, len);
    }
    thread_local! {
        static READS: RefCell<Vec<(u64, u64, bool)>> = const { RefCell::new(Vec::new()) };
    }
    #[test]
    fn s0o_read_range_probe_records_actual_seek_reads() {
        use std::io::Write;
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"abcdef").unwrap();
        take();
        assert_eq!(super::read_at(&file, 2, 3).unwrap(), b"cde");
        assert_eq!(take(), vec![(2, 5, false)]);
    }
    struct ReadHook {
        at: u64,
        action: Box<dyn FnOnce()>,
    }
    thread_local! {
        static HOOK: RefCell<Option<ReadHook>> = const { RefCell::new(None) };
    }
    pub(crate) fn after_read_at(at: u64, action: impl FnOnce() + 'static) {
        HOOK.with_borrow_mut(|hook| {
            *hook = Some(ReadHook {
                at,
                action: Box::new(action),
            })
        });
    }
    pub(crate) fn record(start: u64, len: u64, hash: bool) {
        READS.with_borrow_mut(|reads| reads.push((start, start + len, hash)));
        let action = HOOK.with_borrow_mut(|hook| {
            if len > 0 && hook.as_ref().is_some_and(|h| h.at == start) {
                hook.take()
            } else {
                None
            }
        });
        if let Some(hook) = action {
            (hook.action)();
        }
    }
    pub(crate) fn take() -> Vec<(u64, u64, bool)> {
        READS.with_borrow_mut(std::mem::take)
    }
}
