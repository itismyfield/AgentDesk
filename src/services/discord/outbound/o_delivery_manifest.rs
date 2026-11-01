//! Immutable source-to-body manifests. A query never borrows the current inflight row.

use crate::services::{
    provider::ProviderKind,
    tui_o::{
        shadow::identity::{self, RecordFact, UnitContent},
        shadow::{self, CapturedRecord, ShadowProvider, SourceId, UnitKey, UnitKind},
        store::delivery_evidence,
        writer::pieces::{Derived, UnitDeriver},
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
};
use tokio::time::Instant;

pub(super) const BODY_CAP: usize = 1024 * 1024;
const SOURCE_CAP: usize = 2 * 1024 * 1024;
pub(super) const PREFIX_CAP: usize = 64 * 1024 * 1024;
#[cfg(test)]
pub(super) static SOURCE_BYTES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Piece {
    pub key: UnitKey,
    pub index: u32,
    pub payload: String,
    pub body_end: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Manifest {
    pub version: u32,
    pub source: SourceId,
    pub range: (u64, u64),
    pub prefix: String,
    pub body: String,
    pub pieces: Vec<Piece>,
    pub completed: bool,
    pub input: Option<String>,
}

pub(super) struct Query<'a> {
    pub provider: &'a ProviderKind,
    pub channel: u64,
    pub path: &'a Path,
    pub file: (u64, u64),
    pub range: (u64, u64),
}

#[cfg(test)]
pub(super) fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub(super) fn provider(kind: &ProviderKind) -> Option<ShadowProvider> {
    match kind {
        ProviderKind::Claude => Some(ShadowProvider::Claude),
        ProviderKind::Codex => Some(ShadowProvider::Codex),
        _ => None,
    }
}

pub(super) fn build(root: &Path, query: Query<'_>, deadline: Instant) -> Option<Manifest> {
    build_with_prefix_limit(root, query, deadline, PREFIX_CAP as u64)
}

/// Frame and fixed guards are read afresh; receipt readback proves the delivered body.
pub(super) fn build_terminal(root: &Path, query: Query<'_>, deadline: Instant) -> Option<Manifest> {
    const GUARD: u64 = 4096;
    let (start, end) = query.range;
    if start >= end || Instant::now() >= deadline {
        return None;
    }
    let source = delivery_evidence::source(root, query.channel, query.path, query.file)?.source;
    let path = query.path;
    let file_identity = query.file;
    let mut file = File::open(path).ok()?;
    let matches = |meta: std::fs::Metadata| {
        shadow::capture::file_identity(&meta) == file_identity && meta.len() >= end
    };
    if !matches(file.metadata().ok()?) {
        return None;
    }
    let window_start = start.saturating_sub(GUARD);
    let mut digest = Sha256::new();
    digest.update(b"terminal-frame-v1");
    let mut preceding = None;
    for (index, (from, through)) in [(0, start.min(GUARD)), (window_start, end)]
        .into_iter()
        .enumerate()
    {
        digest.update(from.to_le_bytes());
        digest.update(through.to_le_bytes());
        file.seek(SeekFrom::Start(from)).ok()?;
        let mut position = from;
        let guard_end = if index == 1 { start } else { through };
        let mut chunk = [0; 8192];
        while position < guard_end {
            if Instant::now() >= deadline {
                return None;
            }
            let remaining = (guard_end - position).min(chunk.len() as u64);
            let read = file.read(&mut chunk[..remaining as usize]).ok()?;
            if read == 0 {
                return None;
            }
            #[cfg(test)]
            SOURCE_BYTES.fetch_add(read, std::sync::atomic::Ordering::Relaxed);
            digest.update(&chunk[..read]);
            if index == 1 {
                preceding = chunk.get(read - 1).copied();
            }
            position += read as u64;
        }
    }
    if start > 0 && preceding != Some(b'\n') {
        return None;
    }
    // Tool output can make a short answer span a large frame; bound each record and the
    // derived body instead of rejecting the whole frame before consulting its receipts.
    let mut reader = BufReader::new((&mut file).take(end - start));
    let mut consumed = 0;
    let lines = std::iter::from_fn(|| {
        if consumed == end - start {
            return None;
        }
        if Instant::now() >= deadline {
            return Some(None);
        }
        let mut line = Vec::new();
        let read = reader
            .by_ref()
            .take(SOURCE_CAP as u64 + 1)
            .read_until(b'\n', &mut line)
            .ok()?;
        #[cfg(test)]
        SOURCE_BYTES.fetch_add(read, std::sync::atomic::Ordering::Relaxed);
        if read == 0 || read > SOURCE_CAP || line.last() != Some(&b'\n') {
            return Some(None);
        }
        consumed += read as u64;
        digest.update(&line);
        Some(Some(line))
    });
    let mut plan = derive_lines(query, source, String::new(), lines, deadline, true)?;
    if consumed != end - start
        || !matches(file.metadata().ok()?)
        || !matches(std::fs::metadata(path).ok()?)
        || Instant::now() >= deadline
    {
        return None;
    }
    plan.prefix = format!("terminal-frame-v1:{}", hex::encode(digest.finalize()));
    Some(plan)
}

/// Which producer composed a body, so a re-read joins its records by that producer's rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum BodyProducer {
    /// The watcher joins transcript body units across semantic chunk boundaries.
    Watcher,
    /// The headless bridge joins its producer's `Text` messages around `ToolUse` separators.
    Bridge,
}

#[cfg(test)]
thread_local! {
    /// Runs after each record `body_written_ms` reads, so a test can change the file mid-read.
    pub(super) static AFTER_BODY_RECORD: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
}

/// One range's body as its producer composed it. The bridge's tool separator form depends on its
/// stream ownership at each `ToolUse`, so both uniform forms are kept and either may match.
struct BodyComposer {
    producer: BodyProducer,
    kind: ShadowProvider,
    stream: crate::services::session_backend::StreamLineState,
    messages: (
        std::sync::mpsc::Sender<crate::services::agent_protocol::StreamMessage>,
        std::sync::mpsc::Receiver<crate::services::agent_protocol::StreamMessage>,
    ),
    bodies: [String; 2],
}

impl BodyComposer {
    /// Whether the record wrote body text; None when the bridge's reader could not take it.
    fn record(&mut self, line: &[u8], value: &serde_json::Value) -> Option<bool> {
        use crate::services::agent_protocol::StreamMessage;
        use crate::services::discord::turn_bridge::chunk_compose;
        let mut wrote = false;
        if self.producer == BodyProducer::Watcher {
            for fact in identity::classify(self.kind, value) {
                let RecordFact::Unit(_, UnitKind::Body, UnitContent::Payload(text)) = fact else {
                    continue;
                };
                let composed = &mut self.bodies[0];
                if !composed.is_empty()
                    && super::super::semantic_boundaries::semantic_chunk_separator_needed(
                        composed, &text,
                    )
                {
                    composed.push_str("\n\n");
                }
                composed.push_str(&text);
                wrote = true;
            }
            return Some(wrote);
        }
        let line = std::str::from_utf8(line).ok()?.trim_end_matches('\n');
        // Bridge reader `read_claude_tui_output_file_until_result_with_harvest` uses this parser.
        crate::services::session_backend::process_claude_transcript_line(
            line,
            &self.messages.0,
            &mut self.stream,
        );
        for message in self.messages.1.try_iter() {
            match message {
                StreamMessage::Text { content } if !content.is_empty() => {
                    for body in &mut self.bodies {
                        chunk_compose::compose_streamed_text_chunk(body, &content);
                    }
                    wrote = true;
                }
                StreamMessage::ToolUse { .. } => {
                    for (body, preserve) in self.bodies.iter_mut().zip([true, false]) {
                        chunk_compose::compose_tool_boundary_separator(body, preserve);
                    }
                }
                _ => {}
            }
        }
        Some(wrote)
    }

    fn composes(&self, body: &str) -> bool {
        match self.producer {
            BodyProducer::Watcher => self.bodies[0] == body,
            BodyProducer::Bridge => self.bodies.iter().any(|composed| composed == body),
        }
    }
}

/// When the range's last body record was written, only if the whole range, still that file at
/// that path, composes `body` the way `producer` did.
pub(super) fn body_written_ms(
    query: Query<'_>,
    body: &str,
    producer: BodyProducer,
    deadline: Instant,
) -> Option<i64> {
    let (start, end) = query.range;
    let kind = provider(query.provider)?;
    if start >= end || Instant::now() >= deadline {
        return None;
    }
    let matches = |meta: std::fs::Metadata| {
        shadow::capture::file_identity(&meta) == query.file && meta.len() >= end
    };
    let mut file = File::open(query.path).ok()?;
    if !matches(file.metadata().ok()?) {
        return None;
    }
    if start > 0 {
        let mut preceding = [0];
        file.seek(SeekFrom::Start(start - 1)).ok()?;
        file.read_exact(&mut preceding).ok()?;
        if preceding != [b'\n'] {
            return None;
        }
    }
    let mut composer = BodyComposer {
        producer,
        kind,
        stream: Default::default(),
        messages: std::sync::mpsc::channel(),
        bodies: Default::default(),
    };
    let mut reader = BufReader::new((&mut file).take(end - start));
    let mut line = Vec::new();
    let mut written = None;
    let mut consumed = 0;
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        line.clear();
        let read = reader
            .by_ref()
            .take(SOURCE_CAP as u64 + 1)
            .read_until(b'\n', &mut line)
            .ok()?;
        if read == 0 {
            break;
        }
        if read > SOURCE_CAP || line.last() != Some(&b'\n') {
            return None;
        }
        consumed += read as u64;
        let value: serde_json::Value = serde_json::from_slice(&line).ok()?;
        if composer.record(&line, &value)? {
            let at = chrono::DateTime::parse_from_rfc3339(value["timestamp"].as_str()?).ok()?;
            written = written.max(Some(at.timestamp_millis()));
        }
        #[cfg(test)]
        AFTER_BODY_RECORD.with(|hook| hook.borrow_mut().as_mut().map(|hook| hook()));
    }
    drop(reader);
    if consumed != end - start
        || !matches(file.metadata().ok()?)
        || !matches(std::fs::metadata(query.path).ok()?)
    {
        return None;
    }
    written.filter(|_| composer.composes(body))
}

fn build_with_prefix_limit(
    root: &Path,
    query: Query<'_>,
    deadline: Instant,
    prefix_limit: u64,
) -> Option<Manifest> {
    let cursor = delivery_evidence::source(root, query.channel, query.path, query.file)?;
    let (start, end) = query.range;
    if start >= end || end - start > SOURCE_CAP as u64 {
        return None;
    }
    let mut file = File::open(query.path).ok()?;
    if shadow::capture::file_identity(&file.metadata().ok()?) != query.file {
        return None;
    }
    let selection_start = start.saturating_sub(1);
    let mut bytes = Vec::new();
    let mut digest = Sha256::new();
    let mut chunk = [0; 8192];
    let mut position = 0_u64;
    while position < cursor.captured_through {
        if Instant::now() >= deadline || position >= prefix_limit {
            return None;
        }
        let remaining = (cursor.captured_through - position)
            .min(chunk.len() as u64)
            .min(prefix_limit - position);
        let read = file.read(&mut chunk[..remaining as usize]).ok()?;
        if read == 0 {
            return None;
        }
        #[cfg(test)]
        SOURCE_BYTES.fetch_add(read, std::sync::atomic::Ordering::Relaxed);
        digest.update(&chunk[..read]);
        let from = selection_start.max(position);
        let through = end.min(position + read as u64);
        if from < through {
            bytes.extend_from_slice(
                &chunk[(from - position) as usize..(through - position) as usize],
            );
        }
        position += read as u64;
    }
    if hex::encode(digest.finalize()) != cursor.prefix_hash || Instant::now() >= deadline {
        return None;
    }
    position = position.max(selection_start);
    file.seek(SeekFrom::Start(position)).ok()?;
    while position < end {
        if Instant::now() >= deadline {
            return None;
        }
        let remaining = (end - position).min(chunk.len() as u64);
        let read = file.read(&mut chunk[..remaining as usize]).ok()?;
        if read == 0 {
            return None;
        }
        #[cfg(test)]
        SOURCE_BYTES.fetch_add(read, std::sync::atomic::Ordering::Relaxed);
        bytes.extend_from_slice(&chunk[..read]);
        position += read as u64;
    }
    if shadow::capture::file_identity(&file.metadata().ok()?) != query.file
        || Instant::now() >= deadline
    {
        return None;
    }
    let selected = if start > 0 {
        if bytes.first() != Some(&b'\n') {
            return None;
        }
        bytes.get(1..)?
    } else {
        bytes.as_slice()
    };
    if selected.last() != Some(&b'\n') {
        return None;
    }
    derive(
        query,
        cursor.source,
        cursor.prefix_hash,
        selected,
        deadline,
        false,
    )
}

fn derive(
    query: Query<'_>,
    source: SourceId,
    prefix: String,
    bytes: &[u8],
    deadline: Instant,
    through_frame_end: bool,
) -> Option<Manifest> {
    derive_lines(
        query,
        source,
        prefix,
        bytes
            .split_inclusive(|byte| *byte == b'\n')
            .map(|line| Some(line.to_vec())),
        deadline,
        through_frame_end,
    )
}

fn derive_lines(
    query: Query<'_>,
    source: SourceId,
    prefix: String,
    lines: impl IntoIterator<Item = Option<Vec<u8>>>,
    deadline: Instant,
    through_frame_end: bool,
) -> Option<Manifest> {
    let interrupted = |value: &serde_json::Value| {
        value["type"] == "user"
            || value["is_error"] == true
            || value["subtype"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("error"))
    };
    let mut deriver = UnitDeriver::new(query.channel, provider(query.provider)?);
    let mut manifest = Manifest {
        version: shadow::IDENTITY_VERSION,
        source,
        range: query.range,
        prefix,
        body: String::new(),
        pieces: Vec::new(),
        completed: false,
        input: None,
    };
    let mut offset = query.range.0;
    let mut saw_body = false;
    let mut unit_spans = HashMap::new();
    let mut consumed = HashMap::new();
    for line in lines {
        let line = line?;
        if Instant::now() >= deadline {
            return None;
        }
        let value: serde_json::Value = serde_json::from_slice(&line).ok()?;
        if value["type"] == "event_msg" && value["payload"]["type"] == "turn_aborted" {
            return None;
        }
        let facts = identity::classify(provider(query.provider)?, &value);
        if manifest.completed {
            // A terminal frame may run past the turn end (turn_duration, attachments, session
            // metadata); it stays provable only while those records carry nothing to deliver.
            if interrupted(&value) || !facts.iter().all(|f| matches!(f, RecordFact::Idle(_))) {
                return None;
            }
            offset += line.len() as u64;
            continue;
        }
        for fact in &facts {
            match fact {
                RecordFact::TurnStart(_) if saw_body || manifest.input.is_some() => return None,
                RecordFact::Prompt(_, text) => {
                    if saw_body || manifest.input.is_some() {
                        return None;
                    }
                    manifest.input = Some(text.clone());
                }
                RecordFact::Blocked(_) => return None,
                RecordFact::Unit(native, UnitKind::Body, UnitContent::Payload(text)) => {
                    if !manifest.body.is_empty()
                        && super::super::semantic_boundaries::semantic_chunk_separator_needed(
                            &manifest.body,
                            text,
                        )
                    {
                        manifest.body.push_str("\n\n");
                    }
                    let start = manifest.body.len();
                    unit_spans.insert(
                        UnitKey {
                            channel_id: query.channel,
                            provider: provider(query.provider)?,
                            native_key: native.clone(),
                            kind: UnitKind::Body,
                        },
                        (start, text.clone()),
                    );
                    manifest.body.push_str(text);
                    saw_body = true;
                }
                _ => {}
            }
        }
        let record = CapturedRecord {
            start: offset,
            end: offset + line.len() as u64,
            line: line[..line.len() - 1].to_vec(),
        };
        for work in deriver.derive(&record) {
            match work {
                Derived::Blocked { .. } => return None,
                Derived::Piece(piece) if piece.unit_key.kind == UnitKind::Body => {
                    let (unit_start, text) = unit_spans.get(&piece.unit_key)?;
                    let through = consumed.entry(piece.unit_key.clone()).or_insert(0);
                    let end = piece_end(text.trim(), *through, &piece.payload)?;
                    *through = end;
                    manifest.pieces.push(Piece {
                        key: piece.unit_key,
                        index: piece.index,
                        payload: piece.payload,
                        body_end: unit_start + text.len() - text.trim_start().len() + end,
                    });
                }
                _ => {}
            }
        }
        let native_complete =
            value["type"] == "event_msg" && value["payload"]["type"] == "task_complete";
        // A Stop hook can precede late assistant text and injected notifications. Terminal
        // coverage follows the whole frame; input settlement still uses the first boundary.
        let soft_stop = through_frame_end
            && matches!(query.provider, ProviderKind::Claude)
            && value["type"] == "system"
            && matches!(
                value["subtype"].as_str(),
                Some("stop_hook_summary" | "turn_duration")
            );
        let completed = native_complete
            || (!soft_stop
                && crate::services::tui_turn_state::envelope_is_turn_end_terminator(
                    query.provider,
                    &value,
                ));
        if completed {
            manifest.completed = !interrupted(&value) && saw_body;
            if !manifest.completed {
                return None;
            }
        }
        if manifest.body.len() > BODY_CAP || manifest.pieces.len() > 256 {
            return None;
        }
        offset = record.end;
        if manifest.completed && !through_frame_end {
            manifest.range.1 = offset;
            break;
        }
    }
    (!manifest.body.trim().is_empty() && !manifest.pieces.is_empty()).then_some(manifest)
}

/// Recover raw coverage only when the canonical piece consists of that raw prefix and fence
/// wrappers. The split plan still owns payload generation; ambiguous shapes stay unknown.
fn piece_end(text: &str, start: usize, payload: &str) -> Option<usize> {
    let remaining = text.get(start..)?;
    let mut prefixes = vec![0];
    if payload.starts_with("```")
        && let Some(end) = payload.find('\n')
    {
        prefixes.push(end + 1);
    }
    let mut suffixes = vec![0];
    if payload.ends_with("\n```") {
        suffixes.push(4);
    }
    let mut lengths = Vec::new();
    for prefix in prefixes {
        for &suffix in &suffixes {
            if let Some(end) = payload.len().checked_sub(suffix)
                && let Some(raw) = payload.get(prefix..end)
                && !raw.is_empty()
                && remaining.starts_with(raw)
            {
                lengths.push(raw.len());
            }
        }
    }
    let length = lengths.into_iter().max()?;
    let end = start + length;
    Some(end + usize::from(text.get(end..)?.starts_with('\n')))
}
