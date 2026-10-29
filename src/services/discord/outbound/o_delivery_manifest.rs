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
    io::{Read, Seek, SeekFrom},
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
    if start >= end || end - start > SOURCE_CAP as u64 || Instant::now() >= deadline {
        return None;
    }
    let source = delivery_evidence::source(root, query.channel, query.path, query.file)?.source;
    let mut file = File::open(query.path).ok()?;
    let matches = |meta: std::fs::Metadata| {
        shadow::capture::file_identity(&meta) == query.file && meta.len() >= end
    };
    if !matches(file.metadata().ok()?) {
        return None;
    }
    let window_start = start.saturating_sub(GUARD);
    let mut bytes = Vec::new();
    let mut digest = Sha256::new();
    digest.update(b"terminal-frame-v1");
    for (index, (from, through)) in [(0, start.min(GUARD)), (window_start, end)]
        .into_iter()
        .enumerate()
    {
        digest.update(from.to_le_bytes());
        digest.update(through.to_le_bytes());
        file.seek(SeekFrom::Start(from)).ok()?;
        let mut position = from;
        let mut chunk = [0; 8192];
        while position < through {
            if Instant::now() >= deadline {
                return None;
            }
            let remaining = (through - position).min(chunk.len() as u64);
            let read = file.read(&mut chunk[..remaining as usize]).ok()?;
            if read == 0 {
                return None;
            }
            #[cfg(test)]
            SOURCE_BYTES.fetch_add(read, std::sync::atomic::Ordering::Relaxed);
            digest.update(&chunk[..read]);
            if index == 1 {
                bytes.extend_from_slice(&chunk[..read]);
            }
            position += read as u64;
        }
    }
    if !matches(file.metadata().ok()?)
        || !matches(std::fs::metadata(query.path).ok()?)
        || Instant::now() >= deadline
    {
        return None;
    }
    let at = (start - window_start) as usize;
    if (start > 0 && bytes.get(at - 1) != Some(&b'\n')) || bytes.last() != Some(&b'\n') {
        return None;
    }
    let prefix = format!("terminal-frame-v1:{}", hex::encode(digest.finalize()));
    let plan = derive(query, source, prefix, bytes.get(at..)?, deadline, true)?;
    (Instant::now() < deadline).then_some(plan)
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
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if Instant::now() >= deadline {
            return None;
        }
        let value: serde_json::Value = serde_json::from_slice(line).ok()?;
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
        let completed = native_complete
            || crate::services::tui_turn_state::envelope_is_turn_end_terminator(
                query.provider,
                &value,
            );
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
