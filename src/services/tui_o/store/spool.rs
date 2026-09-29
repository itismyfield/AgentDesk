//! Raw spool and capture cursors. Frames are fsynced before the cursor moves, so a restart
//! re-derives from the spool; recovery proves the segments cover every retained byte.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::ledger::{LedgerEntry, LedgerState};
use super::{
    CURSOR_DIR, ChannelStore, HaltReason, Initialized, SPOOL_DIR, StoreError, damage, durable,
};
use crate::services::discord::runtime_store::fsync_parent_dir;
use crate::services::tui_o::shadow::capture::file_identity;
use crate::services::tui_o::shadow::{CaptureBatch, CapturedRecord, IDENTITY_VERSION, SourceId};

/// A segment takes no more appends past this size; GC removes whole segments.
pub const SEGMENT_MAX_BYTES: u64 = 64 << 20;
/// Per-channel spool ceiling; reaching it pauses the source instead of dropping records.
pub const SPOOL_CAP_BYTES: u64 = 1 << 30;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub source: SourceId,
    pub captured_through: u64,
    /// Hex sha256 of source bytes `0..captured_through`.
    pub prefix_hash: String,
    pub retired: bool,
}

/// A spooled span: a complete record, or bytes capture consumed without one (a torn first line).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpoolFrame {
    Record(CapturedRecord),
    Skipped { start: u64, end: u64 },
}

impl SpoolFrame {
    pub fn start(&self) -> u64 {
        match self {
            Self::Record(record) => record.start,
            Self::Skipped { start, .. } => *start,
        }
    }

    pub fn end(&self) -> u64 {
        match self {
            Self::Record(record) => record.end,
            Self::Skipped { end, .. } => *end,
        }
    }

    /// One line: `r <start> <end> <raw>` or `s <start> <end>`; raw lines never hold a newline.
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Record(record) => {
                out.extend_from_slice(format!("r {} {} ", record.start, record.end).as_bytes());
                out.extend_from_slice(&record.line);
            }
            Self::Skipped { start, end } => {
                out.extend_from_slice(format!("s {start} {end}").as_bytes())
            }
        }
        out.push(b'\n');
    }

    fn decode(line: &[u8]) -> Option<Self> {
        let mut parts = line.splitn(4, |byte| *byte == b' ');
        let kind = parts.next()?;
        let number = |part: Option<&[u8]>| std::str::from_utf8(part?).ok()?.parse::<u64>().ok();
        let (start, end) = (number(parts.next())?, number(parts.next())?);
        match (kind, parts.next()) {
            (b"r", Some(raw)) if end.checked_sub(start)? == raw.len() as u64 + 1 => {
                let line = raw.to_vec();
                Some(Self::Record(CapturedRecord { start, end, line }))
            }
            (b"s", None) if end > start => Some(Self::Skipped { start, end }),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct SegmentHeader {
    source_id: SourceId,
    start_offset: u64,
    identity_version: u32,
}

#[derive(Clone, Debug)]
struct Segment {
    path: PathBuf,
    start: u64,
    end: u64,
    bytes: u64,
}

/// One source's cursor and retained segments; `origin` is where its spool began.
#[derive(Clone, Debug)]
pub(super) struct SourceSpool {
    cursor: Cursor,
    segments: Vec<Segment>,
    origin: u64,
}

impl SourceSpool {
    fn end(&self) -> u64 {
        self.segments
            .last()
            .map_or(self.cursor.captured_through, |segment| segment.end)
    }
}

/// File-name key; dev/ino keep distinct files apart even if a lossy path collides.
pub fn source_key(source: &SourceId) -> String {
    let SourceId {
        session_id,
        path,
        dev,
        ino,
    } = source;
    let identity = format!("{session_id}\0{}\0{dev}\0{ino}", path.to_string_lossy());
    hex::encode(&Sha256::digest(identity.as_bytes())[..16])
}

fn gap(detail: String) -> StoreError {
    StoreError::halt(HaltReason::SpoolGap, detail)
}

fn rejected(detail: &str) -> StoreError {
    StoreError::Rejected(detail.to_string())
}

fn write_cursor(dir: &Path, key: &str, cursor: &Cursor) -> Result<(), StoreError> {
    let path = dir.join(CURSOR_DIR).join(format!("{key}.json"));
    Ok(durable::replace(&path, &serde_json::to_vec(cursor)?)?)
}

fn listed(dir: &Path, suffix: &str) -> io::Result<Vec<(String, PathBuf)>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|name| name.to_str());
        if let Some(stem) = name.and_then(|name| name.strip_suffix(suffix)) {
            names.push((stem.to_string(), path.clone()));
        }
    }
    Ok(names)
}

struct SegmentScan {
    header: SegmentHeader,
    end: u64,
    committed_len: u64,
    torn: bool,
}

/// Streams a segment's frames; a last line without its newline is an unfinished append.
fn scan_segment(path: &Path, mut visit: impl FnMut(SpoolFrame)) -> Result<SegmentScan, StoreError> {
    let bad = |what: &str| damage(format!("{}: {what}", path.display()));
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line)?;
    let header: SegmentHeader = serde_json::from_slice(&line).map_err(|_| bad("header"))?;
    let (mut end, mut committed_len) = (header.start_offset, line.len() as u64);
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 || line.pop() != Some(b'\n') {
            let torn = read > 0;
            return Ok(SegmentScan {
                header,
                end,
                committed_len,
                torn,
            });
        }
        let frame = SpoolFrame::decode(&line).ok_or_else(|| bad("frame"))?;
        if frame.start() != end {
            return Err(gap(format!(
                "{}: frame at {} after {end}",
                path.display(),
                frame.start()
            )));
        }
        (end, committed_len) = (frame.end(), committed_len + read as u64);
        visit(frame);
    }
}

/// Checks spooled bytes past the cursor against the source; returns the prefix hash through `end`.
fn verify_tail(cursor: &Cursor, tail: &[SpoolFrame], end: u64) -> io::Result<Option<String>> {
    let (source, from) = (&cursor.source, cursor.captured_through);
    let Ok(mut file) = File::open(&source.path) else {
        return Ok(None);
    };
    let meta = file.metadata()?;
    if file_identity(&meta) != (source.dev, source.ino) || meta.len() < end {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    io::copy(&mut (&mut file).take(from), &mut hasher)?;
    if hex::encode(hasher.clone().finalize()) != cursor.prefix_hash {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    (&mut file).take(end - from).read_to_end(&mut bytes)?;
    hasher.update(&bytes);
    let span = |start: u64, stop: u64| bytes.get((start - from) as usize..(stop - from) as usize);
    let same = tail.iter().all(|frame| match frame {
        SpoolFrame::Record(record) => span(record.start, record.end)
            .is_some_and(|raw| raw.split_last() == Some((&b'\n', record.line.as_slice()))),
        SpoolFrame::Skipped { .. } => true,
    });
    Ok(same.then(|| hex::encode(hasher.finalize())))
}

/// Proves coverage of `[retained, cursor]`, finishes a logged GC, then settles a spool tail past the cursor.
/// `gc` is the source's logged GC chain; `violated` withholds every deletion.
fn recover_source(
    dir: &Path,
    key: &str,
    cursor: Cursor,
    origin: u64,
    gc: &[(u64, u64)],
    violated: bool,
    mut paths: Vec<(u64, PathBuf)>,
) -> Result<SourceSpool, StoreError> {
    paths.sort();
    if gc.first().is_some_and(|&(start, _)| start != origin) {
        return Err(damage(format!(
            "spool {key} GC chain does not start at {origin}"
        )));
    }
    let retained = gc.last().map_or(origin, |&(_, through)| through);
    let through = cursor.captured_through;
    let (mut expected, mut at_boundary) = (retained, through == retained);
    let (mut segments, mut tail, mut torn) = (Vec::new(), Vec::new(), None);
    let last = paths.len().saturating_sub(1);
    for (index, (start, path)) in paths.into_iter().enumerate() {
        if start < retained && !violated {
            // Only a segment matching a logged span exactly is deleted; its entry is durable.
            let span = (start, scan_segment(&path, |_| ())?.end);
            if gc.contains(&span) {
                fs::remove_file(&path)?;
                fsync_parent_dir(&path)?;
                continue;
            }
        }
        let mut bad_skip = false;
        let scan = scan_segment(&path, |frame| {
            at_boundary |= frame.end() == through;
            // A skip is only the torn first line of a source that began mid-file.
            bad_skip |= matches!(frame, SpoolFrame::Skipped { start, .. } if start != origin || origin == 0);
            if frame.start() >= through {
                tail.push(frame);
            }
        })?;
        let header = &scan.header;
        if header.start_offset != start
            || header.source_id != cursor.source
            || header.identity_version != IDENTITY_VERSION
            || bad_skip
        {
            return Err(damage(format!(
                "{}: header or frames disagree",
                path.display()
            )));
        }
        if start != expected {
            return Err(gap(format!(
                "spool {key} resumes at {start}, expected {expected}"
            )));
        }
        if scan.torn {
            if index != last {
                return Err(damage(format!(
                    "{}: unfinished frame mid-spool",
                    path.display()
                )));
            }
            torn = Some((path.clone(), scan.committed_len));
        }
        expected = scan.end;
        let bytes = scan.committed_len;
        segments.push(Segment {
            path,
            start,
            end: expected,
            bytes,
        });
    }
    if through < retained || through > expected || !at_boundary {
        return Err(gap(format!(
            "cursor {through} outside spool {key} [{retained}, {expected}]"
        )));
    }
    let mut cursor = cursor;
    if expected > through {
        let Some(prefix_hash) = verify_tail(&cursor, &tail, expected)? else {
            let detail = format!("spool {key} tail {through}..{expected} differs from the source");
            return Err(StoreError::halt(HaltReason::SpoolTailMismatch, detail));
        };
        cursor = Cursor {
            captured_through: expected,
            prefix_hash,
            ..cursor
        };
    }
    if let Some((path, len)) = torn {
        durable::truncate_synced(&path, len)?;
    }
    if cursor.captured_through != through {
        write_cursor(dir, key, &cursor)?;
    }
    Ok(SourceSpool {
        cursor,
        segments,
        origin,
    })
}

/// Rebuilds every source: init sources default to their delivery start, later ones to offset 0.
pub(super) fn recover_sources(
    dir: &Path,
    init: &Initialized,
    ledger: &LedgerState,
) -> Result<BTreeMap<String, SourceSpool>, StoreError> {
    let mut cursors = BTreeMap::new();
    for source in &init.sources {
        let cursor = Cursor {
            source: source.source_id.clone(),
            captured_through: source.delivery_start,
            prefix_hash: source.prefix_hash.clone(),
            retired: false,
        };
        cursors.insert(
            source_key(&source.source_id),
            (cursor, source.delivery_start),
        );
    }
    for (key, path) in listed(&dir.join(CURSOR_DIR), ".json")? {
        let cursor: Option<Cursor> = durable::read_json(&path)?;
        let cursor = cursor.filter(|cursor| source_key(&cursor.source) == key);
        let cursor =
            cursor.ok_or_else(|| damage(format!("{}: cursor unreadable", path.display())))?;
        let origin = cursors.get(&key).map_or(0, |(_, origin)| *origin);
        cursors.insert(key, (cursor, origin));
    }
    let mut segments: BTreeMap<String, Vec<(u64, PathBuf)>> = BTreeMap::new();
    for (stem, path) in listed(&dir.join(SPOOL_DIR), ".seg")? {
        let parsed = stem
            .rsplit_once('-')
            .and_then(|(key, start)| Some((key.to_string(), start.parse().ok()?)));
        let (key, start) =
            parsed.ok_or_else(|| damage(format!("{}: segment name", path.display())))?;
        if !cursors.contains_key(&key) {
            return Err(damage(format!(
                "{}: segment without a cursor",
                path.display()
            )));
        }
        segments.entry(key).or_default().push((start, path));
    }
    let mut spools = BTreeMap::new();
    for (key, (cursor, origin)) in cursors {
        let (gc, violated) = (
            ledger.gc_segments(&cursor.source),
            ledger.violation().is_some(),
        );
        let paths = segments.remove(&key).unwrap_or_default();
        let spool = recover_source(dir, &key, cursor, origin, gc, violated, paths)?;
        spools.insert(key, spool);
    }
    Ok(spools)
}

/// Frames a batch contiguously after the spool end. One leading skip is allowed: the torn first
/// line of a source that began mid-file, before anything was spooled.
fn frames_for(spool: &SourceSpool, batch: &CaptureBatch) -> Result<Vec<SpoolFrame>, StoreError> {
    let mut pos = spool.end();
    let at_origin = spool.segments.is_empty() && pos == spool.origin && spool.origin > 0;
    let mut frames = Vec::new();
    let ends = batch
        .records
        .iter()
        .map(|record| (record.start, Some(record)));
    for (start, record) in ends.chain([(batch.captured_through, None)]) {
        if start < pos {
            return Err(rejected("batch overlaps or runs behind the spool"));
        }
        if start > pos {
            if !(at_origin && frames.is_empty()) {
                return Err(rejected("batch skips bytes past the spool origin"));
            }
            frames.push(SpoolFrame::Skipped {
                start: pos,
                end: start,
            });
        }
        let Some(record) = record else { break };
        let whole = record.end == record.start + record.line.len() as u64 + 1;
        if !whole || record.line.contains(&b'\n') {
            return Err(rejected("malformed record"));
        }
        frames.push(SpoolFrame::Record(record.clone()));
        pos = record.end;
    }
    Ok(frames)
}

impl ChannelStore {
    pub fn cursor(&self, source: &SourceId) -> Option<&Cursor> {
        self.sources
            .get(&source_key(source))
            .map(|spool| &spool.cursor)
    }

    pub fn cursors(&self) -> impl Iterator<Item = &Cursor> {
        self.sources.values().map(|spool| &spool.cursor)
    }

    pub fn retained_segments(&self, source: &SourceId) -> usize {
        let spool = self.sources.get(&source_key(source));
        spool.map_or(0, |spool| spool.segments.len())
    }

    pub fn spool_bytes(&self) -> u64 {
        let segments = self.sources.values().flat_map(|spool| &spool.segments);
        segments.map(|segment| segment.bytes).sum()
    }

    /// Starts a source first seen after the switch at offset 0; an attached source is returned as is.
    pub fn attach_source(&mut self, source: &SourceId) -> Result<Cursor, StoreError> {
        self.mutate(|store| {
            let spool = match store.sources.entry(source_key(source)) {
                Entry::Occupied(slot) => slot.into_mut(),
                Entry::Vacant(slot) => {
                    let prefix_hash = hex::encode(Sha256::digest(b""));
                    let (source, retired) = (source.clone(), false);
                    let cursor = Cursor {
                        source,
                        captured_through: 0,
                        prefix_hash,
                        retired,
                    };
                    write_cursor(&store.dir, slot.key(), &cursor)?;
                    slot.insert(SourceSpool {
                        cursor,
                        segments: Vec::new(),
                        origin: 0,
                    })
                }
            };
            Ok(spool.cursor.clone())
        })
    }

    /// Spools a capture batch and fsyncs it, then moves the cursor; `prefix_hash` covers `0..captured_through`.
    pub fn append_spool(
        &mut self,
        batch: &CaptureBatch,
        prefix_hash: &str,
    ) -> Result<(), StoreError> {
        let used = self.spool_bytes();
        self.mutate(|store| {
            let key = source_key(&batch.source);
            let spool = store
                .sources
                .get_mut(&key)
                .ok_or_else(|| rejected("source not attached"))?;
            let frames = frames_for(spool, batch)?;
            if frames.is_empty() {
                return Ok(());
            }
            let mut bytes = Vec::new();
            frames.iter().for_each(|frame| frame.encode(&mut bytes));
            if used + bytes.len() as u64 > store.spool_cap {
                return Err(StoreError::SpoolFull);
            }
            if spool
                .segments
                .last()
                .is_none_or(|segment| segment.bytes >= store.segment_max)
            {
                let header = SegmentHeader {
                    source_id: batch.source.clone(),
                    start_offset: spool.end(),
                    identity_version: IDENTITY_VERSION,
                };
                let name = format!("{key}-{:020}.seg", header.start_offset);
                let path = store.dir.join(SPOOL_DIR).join(name);
                let mut line = serde_json::to_vec(&header)?;
                line.push(b'\n');
                durable::create_once(&path, &line)?;
                let (start, end, bytes) =
                    (header.start_offset, header.start_offset, line.len() as u64);
                spool.segments.push(Segment {
                    path,
                    start,
                    end,
                    bytes,
                });
            }
            let segment = spool
                .segments
                .last_mut()
                .ok_or_else(|| rejected("no open segment"))?;
            durable::append_synced(&segment.path, &bytes)?;
            (segment.end, segment.bytes) =
                (batch.captured_through, segment.bytes + bytes.len() as u64);
            let captured_through = batch.captured_through;
            let prefix_hash = prefix_hash.to_string();
            let cursor = Cursor {
                captured_through,
                prefix_hash,
                ..spool.cursor.clone()
            };
            write_cursor(&store.dir, &key, &cursor)?;
            spool.cursor = cursor;
            Ok(())
        })
    }

    pub fn set_retired(&mut self, source: &SourceId, retired: bool) -> Result<(), StoreError> {
        self.mutate(|store| {
            let key = source_key(source);
            let spool = store
                .sources
                .get_mut(&key)
                .ok_or_else(|| rejected("source not attached"))?;
            let cursor = Cursor {
                retired,
                ..spool.cursor.clone()
            };
            write_cursor(&store.dir, &key, &cursor)?;
            spool.cursor = cursor;
            Ok(())
        })
    }

    /// Streams retained frames up to the cursor, oldest first, for re-derivation.
    /// A read I/O error also stops the channel's writes until it is reopened.
    pub fn for_each_frame(
        &mut self,
        source: &SourceId,
        mut visit: impl FnMut(SpoolFrame),
    ) -> Result<(), StoreError> {
        self.mutate(|store| {
            let spool = store
                .sources
                .get(&source_key(source))
                .ok_or_else(|| rejected("source not attached"))?;
            let through = spool.cursor.captured_through;
            for segment in &spool.segments {
                scan_segment(&segment.path, |frame| {
                    if frame.end() <= through {
                        visit(frame);
                    }
                })?;
            }
            Ok(())
        })
    }

    /// Deletes the oldest segment the cursor has passed. The caller vouches that every unit in it
    /// is settled in the ledger and that its source is not boundary-pending.
    pub fn gc_oldest_segment(&mut self, source: &SourceId) -> Result<(), StoreError> {
        self.mutate(|store| {
            if store.ledger.violation().is_some() {
                return Err(rejected("ledger violation withholds GC"));
            }
            let key = source_key(source);
            let spool = store
                .sources
                .get(&key)
                .ok_or_else(|| rejected("source not attached"))?;
            let segment = spool
                .segments
                .first()
                .cloned()
                .ok_or_else(|| rejected("no segment"))?;
            if segment.end > spool.cursor.captured_through {
                return Err(rejected("cursor has not passed the segment"));
            }
            let (source, segment_start, through) = (source.clone(), segment.start, segment.end);
            store.write_ledger(LedgerEntry::SpoolGc {
                source,
                segment_start,
                through,
            })?;
            fs::remove_file(&segment.path)?;
            fsync_parent_dir(&segment.path)?;
            if let Some(spool) = store.sources.get_mut(&key) {
                spool.segments.remove(0);
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::{Seek, SeekFrom, Write};

    use chrono::Utc;

    use super::super::tests::{enabled, initialized};
    use super::super::{Halt, InitSource, OEra, OStore, STORE_DIR_NAME};
    use super::*;
    use crate::services::tui_o::shadow::binding_reader::source_id_for;
    use crate::services::tui_o::shadow::capture::SourceCapture;
    use crate::services::tui_o::shadow::{CaptureOutcome, CaptureSource};

    struct Fixture {
        runtime: tempfile::TempDir,
        path: PathBuf,
        source: SourceId,
        store: OStore,
        era: OEra,
    }

    impl Fixture {
        /// Channel 7 over a transcript holding `body`; `delivery_start` makes it an init source.
        fn new(body: &[u8], delivery_start: Option<u64>) -> Self {
            let runtime = tempfile::tempdir().unwrap();
            let path = runtime.path().join("t.jsonl");
            std::fs::write(&path, body).unwrap();
            let source = source_id_for("s1", &path).unwrap();
            let store = enabled(runtime.path());
            let init_source = |start: u64| InitSource {
                source_id: source.clone(),
                delivery_start: start,
                prefix_hash: hex::encode(Sha256::digest(&body[..start as usize])),
            };
            let sources: Vec<InitSource> = delivery_start.map(init_source).into_iter().collect();
            let init = |channel| Ok(initialized(channel, sources.clone()));
            let era = store.begin_era(&[7], Utc::now(), init).unwrap();
            Self {
                runtime,
                path,
                source,
                store,
                era,
            }
        }

        fn open(&self) -> Result<ChannelStore, Halt> {
            self.store.open_channel(&self.era, 7).map(Option::unwrap)
        }

        fn grow(&self, bytes: &[u8]) {
            let mut file = OpenOptions::new().append(true).open(&self.path).unwrap();
            file.write_all(bytes).unwrap();
        }

        fn channel_dir(&self) -> PathBuf {
            self.runtime.path().join(STORE_DIR_NAME).join("7")
        }

        fn segments(&self) -> Vec<PathBuf> {
            let mut paths: Vec<_> = listed(&self.channel_dir().join(SPOOL_DIR), ".seg")
                .unwrap()
                .into_iter()
                .map(|(_, path)| path)
                .collect();
            paths.sort();
            paths
        }

        fn cursor_path(&self) -> PathBuf {
            let name = format!("{}.json", source_key(&self.source));
            self.channel_dir().join(CURSOR_DIR).join(name)
        }
    }

    fn poll(capture: &mut SourceCapture) -> (CaptureBatch, String) {
        match capture.poll(1 << 20) {
            CaptureOutcome::Batch(batch) => (batch, capture.prefix_hash()),
            other => panic!("capture failed: {other:?}"),
        }
    }

    fn lines(channel: &mut ChannelStore, source: &SourceId) -> Vec<String> {
        let mut lines = Vec::new();
        channel
            .for_each_frame(source, |frame| {
                lines.push(match frame {
                    SpoolFrame::Record(record) => String::from_utf8(record.line).unwrap(),
                    SpoolFrame::Skipped { start, end } => format!("skip {start}..{end}"),
                })
            })
            .unwrap();
        lines
    }

    fn reason(result: Result<ChannelStore, Halt>) -> HaltReason {
        result.err().expect("expected a halt").reason
    }

    /// Attaches a new source and spools `L1`, then `L2` as a second batch.
    fn two_batches(fixture: &Fixture, channel: &mut ChannelStore) -> SourceCapture {
        channel.attach_source(&fixture.source).unwrap();
        let mut capture = SourceCapture::open(fixture.source.clone(), 0).unwrap();
        let (batch, hash) = poll(&mut capture);
        channel.append_spool(&batch, &hash).unwrap();
        fixture.grow(b"L2\n");
        let (batch, hash) = poll(&mut capture);
        channel.append_spool(&batch, &hash).unwrap();
        capture
    }

    #[test]
    fn spool_and_cursor_round_trip_from_a_mid_line_switch_point() {
        let fixture = Fixture::new(b"AAA\nBB", Some(6));
        let mut channel = fixture.open().unwrap();
        fixture.grow(b"B\nCC\n");
        let mut capture = SourceCapture::open(fixture.source.clone(), 6).unwrap();
        let (batch, hash) = poll(&mut capture);
        channel.append_spool(&batch, &hash).unwrap();
        channel.set_retired(&fixture.source, true).unwrap();
        let mut reopened = fixture.open().unwrap();
        let cursor = reopened.cursor(&fixture.source).unwrap();
        assert_eq!((cursor.captured_through, cursor.retired), (11, true));
        assert_eq!(cursor.prefix_hash, hash);
        assert_eq!(lines(&mut reopened, &fixture.source), ["skip 6..8", "CC"]);
    }

    #[test]
    fn a_crash_between_spool_fsync_and_cursor_move_is_recovered_by_recomputation() {
        let fixture = Fixture::new(b"L1\n", None);
        let mut channel = fixture.open().unwrap();
        channel.attach_source(&fixture.source).unwrap();
        let mut capture = SourceCapture::open(fixture.source.clone(), 0).unwrap();
        let (batch, hash) = poll(&mut capture);
        channel.append_spool(&batch, &hash).unwrap();
        let stale_cursor = std::fs::read(fixture.cursor_path()).unwrap();
        fixture.grow(b"L2\n");
        let (batch, hash) = poll(&mut capture);
        channel.append_spool(&batch, &hash).unwrap();
        std::fs::write(fixture.cursor_path(), stale_cursor).unwrap();
        durable::append_synced(&fixture.segments()[0], b"r 6 9 L").unwrap();
        let mut reopened = fixture.open().unwrap();
        let cursor = reopened.cursor(&fixture.source).unwrap().clone();
        assert_eq!((cursor.captured_through, cursor.prefix_hash), (6, hash));
        fixture.grow(b"L3\n");
        let (batch, hash) = poll(&mut capture);
        reopened.append_spool(&batch, &hash).unwrap();
        assert_eq!(
            lines(&mut fixture.open().unwrap(), &fixture.source),
            ["L1", "L2", "L3"]
        );
    }

    #[test]
    fn a_spool_tail_the_source_no_longer_holds_halts_the_channel() {
        let fixture = Fixture::new(b"L1\n", None);
        let mut channel = fixture.open().unwrap();
        channel.attach_source(&fixture.source).unwrap();
        let mut capture = SourceCapture::open(fixture.source.clone(), 0).unwrap();
        let (batch, hash) = poll(&mut capture);
        channel.append_spool(&batch, &hash).unwrap();
        let stale_cursor = std::fs::read(fixture.cursor_path()).unwrap();
        fixture.grow(b"L2\n");
        let (batch, hash) = poll(&mut capture);
        channel.append_spool(&batch, &hash).unwrap();
        std::fs::write(fixture.cursor_path(), &stale_cursor).unwrap();
        let mut file = OpenOptions::new().write(true).open(&fixture.path).unwrap();
        file.seek(SeekFrom::Start(3)).unwrap();
        file.write_all(b"X2\n").unwrap();
        assert_eq!(reason(fixture.open()), HaltReason::SpoolTailMismatch);
        assert_eq!(std::fs::read(fixture.cursor_path()).unwrap(), stale_cursor);
    }

    #[test]
    fn a_lost_spool_segment_halts_with_a_gap_instead_of_resuming() {
        let fixture = Fixture::new(b"L1\n", None);
        let mut channel = fixture.open().unwrap();
        channel.segment_max = 1;
        two_batches(&fixture, &mut channel);
        let segments = fixture.segments();
        assert_eq!(segments.len(), 2);
        for lost in &segments {
            let bytes = std::fs::read(lost).unwrap();
            std::fs::remove_file(lost).unwrap();
            assert_eq!(reason(fixture.open()), HaltReason::SpoolGap);
            std::fs::write(lost, bytes).unwrap();
        }
        assert_eq!(
            lines(&mut fixture.open().unwrap(), &fixture.source),
            ["L1", "L2"]
        );
    }

    #[test]
    fn gc_is_logged_before_the_delete_and_recovery_finishes_an_interrupted_one() {
        let fixture = Fixture::new(b"L1\n", None);
        let mut channel = fixture.open().unwrap();
        channel.segment_max = 1;
        two_batches(&fixture, &mut channel);
        let oldest = fixture.segments()[0].clone();
        let bytes = std::fs::read(&oldest).unwrap();
        channel.gc_oldest_segment(&fixture.source).unwrap();
        assert_eq!(channel.ledger().gc_through(&fixture.source), Some(3));
        std::fs::write(&oldest, bytes).unwrap();
        let mut reopened = fixture.open().unwrap();
        assert!(!oldest.exists());
        assert_eq!(lines(&mut reopened, &fixture.source), ["L2"]);
    }

    #[test]
    fn appends_that_would_skip_or_overflow_leave_spool_and_cursor_unchanged() {
        let fixture = Fixture::new(b"L1\n", None);
        let mut channel = fixture.open().unwrap();
        channel.attach_source(&fixture.source).unwrap();
        let mut capture = SourceCapture::open(fixture.source.clone(), 0).unwrap();
        let (batch, hash) = poll(&mut capture);
        channel.append_spool(&batch, &hash).unwrap();
        let before = channel.cursor(&fixture.source).cloned();
        let record = CapturedRecord {
            start: 9,
            end: 12,
            line: b"L4".to_vec(),
        };
        let (source, records) = (fixture.source.clone(), vec![record]);
        let gapped = CaptureBatch {
            source,
            records,
            captured_through: 12,
        };
        assert!(matches!(
            channel.append_spool(&gapped, "h"),
            Err(StoreError::Rejected(_))
        ));
        channel.spool_cap = channel.spool_bytes() + 4;
        fixture.grow(b"L2\n");
        let (batch, hash) = poll(&mut capture);
        assert!(matches!(
            channel.append_spool(&batch, &hash),
            Err(StoreError::SpoolFull)
        ));
        assert_eq!(channel.cursor(&fixture.source).cloned(), before);
        let mut reopened = fixture.open().unwrap();
        assert_eq!(reopened.cursor(&fixture.source).cloned(), before);
        assert_eq!(lines(&mut reopened, &fixture.source), ["L1"]);
    }

    #[test]
    fn reopening_sweeps_crash_aliases_so_gc_frees_the_segment() {
        let fixture = Fixture::new(b"L1\n", None);
        let mut channel = fixture.open().unwrap();
        channel.segment_max = 1;
        two_batches(&fixture, &mut channel);
        let oldest = fixture.segments()[0].clone();
        let alias = PathBuf::from(format!("{}.tmp", oldest.display()));
        fs::hard_link(&oldest, &alias).unwrap();
        let mut reopened = fixture.open().unwrap();
        assert!(!alias.exists());
        reopened.gc_oldest_segment(&fixture.source).unwrap();
        assert!(!oldest.exists() && !alias.exists());
    }

    #[test]
    fn a_spool_read_error_stops_writes_until_the_channel_is_reopened() {
        let fixture = Fixture::new(b"L1\n", None);
        let mut channel = fixture.open().unwrap();
        two_batches(&fixture, &mut channel);
        let segment = fixture.segments()[0].clone();
        let bytes = fs::read(&segment).unwrap();
        fs::remove_file(&segment).unwrap();
        fs::create_dir(&segment).unwrap();
        let read = channel.for_each_frame(&fixture.source, |_| ());
        assert!(matches!(read, Err(StoreError::Io(_))));
        fs::remove_dir(&segment).unwrap();
        fs::write(&segment, bytes).unwrap();
        let excluded = |reason: &str| LedgerEntry::Excluded {
            unit_key: crate::services::tui_o::shadow::UnitKey {
                channel_id: 7,
                provider: crate::services::tui_o::shadow::ShadowProvider::Claude,
                native_key: "u".into(),
                kind: crate::services::tui_o::shadow::UnitKind::Body,
            },
            reason: reason.into(),
        };
        assert!(matches!(
            channel.append_ledger(excluded("a")),
            Err(StoreError::Rejected(_))
        ));
        assert!(channel.set_retired(&fixture.source, true).is_err());
        fixture
            .open()
            .unwrap()
            .append_ledger(excluded("b"))
            .unwrap();
    }

    #[test]
    fn a_first_batch_may_skip_only_the_torn_first_line() {
        let fixture = Fixture::new(b"L1\nL2\nL3\n", None);
        let mut channel = fixture.open().unwrap();
        channel.attach_source(&fixture.source).unwrap();
        let record = |start: u64, line: &str| CapturedRecord {
            start,
            end: start + line.len() as u64 + 1,
            line: line.as_bytes().to_vec(),
        };
        let (source, records) = (
            fixture.source.clone(),
            vec![record(0, "L1"), record(6, "L3")],
        );
        let holed = CaptureBatch {
            source,
            records,
            captured_through: 9,
        };
        assert!(matches!(
            channel.append_spool(&holed, "h"),
            Err(StoreError::Rejected(_))
        ));
        let (source, records) = (fixture.source.clone(), vec![record(0, "L1")]);
        let trailing = CaptureBatch {
            source,
            records,
            captured_through: 6,
        };
        assert!(matches!(
            channel.append_spool(&trailing, "h"),
            Err(StoreError::Rejected(_))
        ));
        let (source, records) = (fixture.source.clone(), vec![record(0, "L1")]);
        let whole = CaptureBatch {
            source,
            records,
            captured_through: 3,
        };
        let hash = |end: usize| hex::encode(Sha256::digest(&b"L1\nL2\nL3\n"[..end]));
        channel.append_spool(&whole, &hash(3)).unwrap();
        // A mid-spool skip written behind the API is refused on recovery as well.
        durable::append_synced(&fixture.segments()[0], b"s 3 6\nr 6 9 L3\n").unwrap();
        let cursor = Cursor {
            captured_through: 9,
            prefix_hash: hash(9),
            ..channel.cursor(&fixture.source).cloned().unwrap()
        };
        fs::write(fixture.cursor_path(), serde_json::to_vec(&cursor).unwrap()).unwrap();
        assert_eq!(reason(fixture.open()), HaltReason::StoreDamage);
    }

    #[test]
    fn a_gc_entry_outside_the_logged_chain_deletes_nothing_and_halts() {
        let fixture = Fixture::new(b"L1\n", None);
        let mut channel = fixture.open().unwrap();
        channel.segment_max = 1;
        two_batches(&fixture, &mut channel);
        let source = fixture.source.clone();
        let forged = LedgerEntry::SpoolGc {
            source: source.clone(),
            segment_start: 0,
            through: 6,
        };
        assert!(matches!(
            channel.append_ledger(forged),
            Err(StoreError::Rejected(_))
        ));
        let ledger = fixture.channel_dir().join(super::super::LEDGER_FILE);
        let segments = fixture.segments();
        for (segment_start, through) in [(999, 6), (0, 6)] {
            let entry = LedgerEntry::SpoolGc {
                source: source.clone(),
                segment_start,
                through,
            };
            let line = serde_json::json!({ "at": Utc::now(), "entry": entry });
            let saved = fs::read(&ledger).unwrap();
            durable::append_synced(&ledger, format!("{line}\n").as_bytes()).unwrap();
            assert!(
                fixture.open().is_err(),
                "forged GC {segment_start}..{through} opened"
            );
            assert!(segments.iter().all(|segment| segment.exists()));
            fs::write(&ledger, saved).unwrap();
        }
        assert_eq!(lines(&mut fixture.open().unwrap(), &source), ["L1", "L2"]);
    }
}
