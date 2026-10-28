//! Reads a Codex rollout's tail: which native turn is open and whether Enter would steer it, and
//! which turn recorded an injected nonce as user input.

use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

const MAX_SCAN_BYTES: u64 = 64 * 1024 * 1024;
const SCAN_BLOCK_BYTES: u64 = 64 * 1024;

/// The open native turn, as Enter in the composer would meet it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TurnVerdict {
    /// A model or manual compaction turn that can accept composer input.
    KnownModelTurn(String),
    /// A shell/review turn or a sandbox other than full access is outside this input path.
    NonSteerable(String),
    /// Not proven either way.
    Unknown,
    /// The last turn has ended, or none ever started.
    NotBusy,
}

pub(crate) struct TurnRead {
    pub verdict: TurnVerdict,
    pub reason: &'static str,
    pub bytes_read: u64,
}

#[derive(Default)]
struct Open {
    turn: String,
    root: String,
    context: bool,
    full_access: bool,
    user: bool,
    compaction: bool,
    closed: bool,
    unreadable: bool,
}

/// Review mode as the window shows it; an entry without a name proves no review closed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Review {
    #[default]
    Clear,
    Active(String),
    Unknown,
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// Each verdict opens its own EOF snapshot and retains no read position between calls.
pub(crate) fn read_turn(path: &Path) -> Option<TurnVerdict> {
    read_turn_detail(path).map(|read| read.verdict)
}

pub(crate) fn read_turn_detail(path: &Path) -> Option<TurnRead> {
    let began = std::time::Instant::now();
    let read = (|| {
        let mut file = std::fs::File::open(path).ok()?;
        let metadata = file.metadata().ok()?;
        scan(path, &mut file, &metadata, |_| {}).ok()
    })();
    tracing::debug!(
        bytes_read = read.as_ref().map(|read| read.bytes_read),
        elapsed_micros = began.elapsed().as_micros() as u64,
        reason = read
            .as_ref()
            .map_or("transcript_unavailable", |read| read.reason),
        "Codex turn scan"
    );
    read
}

fn source_current(path: &Path, before: &std::fs::Metadata) -> bool {
    let Ok(after) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
            return false;
        }
    }
    after.is_file() && after.len() >= before.len()
}

fn scan(
    path: &Path,
    file: &mut std::fs::File,
    metadata: &std::fs::Metadata,
    mut after_block: impl FnMut(u64),
) -> std::io::Result<TurnRead> {
    let eof = metadata.len();
    let floor = eof.saturating_sub(MAX_SCAN_BYTES);
    let mut at = eof;
    let mut bytes_read = 0;
    let mut chunks = Vec::new();
    let mut fragments: Vec<Vec<u8>> = Vec::new();
    let mut sealed = false;
    let mut latest_start = None;
    let mut review_seed = None;
    let unknown = |reason, bytes_read| TurnRead {
        verdict: TurnVerdict::Unknown,
        reason,
        bytes_read,
    };
    while at > floor {
        let begin = at.saturating_sub(SCAN_BLOCK_BYTES).max(floor);
        let mut chunk = vec![0; (at - begin) as usize];
        file.seek(SeekFrom::Start(begin))?;
        let mut filled = 0;
        while filled < chunk.len() {
            let read = file.read(&mut chunk[filled..])?;
            bytes_read += read as u64;
            if read == 0 {
                return Ok(unknown("source_changed", bytes_read));
            }
            filled += read;
        }
        after_block(bytes_read);
        let mut edge = chunk.len();
        for newline in (0..chunk.len())
            .rev()
            .filter(|index| chunk[*index] == b'\n')
        {
            if sealed {
                inspect_reverse(
                    &chunk[newline + 1..edge],
                    &fragments,
                    begin + newline as u64 + 1,
                    &mut latest_start,
                    &mut review_seed,
                );
            }
            fragments.clear();
            sealed = true;
            edge = newline;
        }
        if sealed {
            fragments.push(chunk[..edge].to_vec());
        }
        chunks.push(chunk);
        at = begin;
        if at == floor && sealed {
            inspect_reverse(&[], &fragments, at, &mut latest_start, &mut review_seed);
        }
        if latest_start.is_some() && review_seed.is_some() {
            break;
        }
    }
    if !source_current(path, metadata) {
        return Ok(unknown("source_changed", bytes_read));
    }
    let Some(start) = latest_start else {
        return Ok(TurnRead {
            verdict: if at == 0 {
                TurnVerdict::NotBusy
            } else {
                TurnVerdict::Unknown
            },
            reason: if at == 0 {
                "not_busy"
            } else {
                "turn_scan_limit"
            },
            bytes_read,
        });
    };
    // Join each block once; a JSON line spanning many blocks never causes repeated prepends.
    drop(fragments);
    let mut bytes = Vec::with_capacity(bytes_read as usize);
    for chunk in chunks.into_iter().rev() {
        bytes.extend_from_slice(&chunk);
    }
    let parse_start = if at == 0 {
        0
    } else {
        review_seed.map_or(start, |seed| seed.min(start))
    };
    let mut open = None;
    let mut review = if at == 0 {
        Review::Clear
    } else {
        Review::Unknown
    };
    for line in bytes[(parse_start - at) as usize..]
        .split_inclusive(|byte| *byte == b'\n')
        .filter(|line| line.ends_with(b"\n"))
    {
        match serde_json::from_slice::<Value>(line) {
            Ok(record) => apply(&record, &mut open, &mut review),
            Err(_) => {
                if let Some(open) = open.as_mut() {
                    open.unreadable = true;
                }
            }
        }
    }
    let Some(open) = open else {
        return Ok(unknown("turn_unknown", bytes_read));
    };
    let turn = open.turn.clone();
    let (verdict, reason) = if open.unreadable {
        (TurnVerdict::Unknown, "malformed_record")
    } else if open.closed {
        (TurnVerdict::NotBusy, "not_busy")
    } else if review == Review::Unknown {
        (
            TurnVerdict::Unknown,
            if review_seed.is_none() && at > 0 {
                "review_seed_unknown"
            } else {
                "turn_unknown"
            },
        )
    } else if open.turn != open.root
        || review != Review::Clear
        || (open.context && !open.full_access)
    {
        (TurnVerdict::NonSteerable(turn), "not_steerable")
    } else if open.compaction {
        (TurnVerdict::KnownModelTurn(turn), "known_compact_turn")
    } else if !open.context {
        (TurnVerdict::NonSteerable(turn), "not_steerable")
    } else if !open.user {
        (TurnVerdict::Unknown, "turn_unknown")
    } else {
        (TurnVerdict::KnownModelTurn(turn), "known_model_turn")
    };
    Ok(TurnRead {
        verdict,
        reason,
        bytes_read,
    })
}

fn inspect_reverse(
    first: &[u8],
    fragments: &[Vec<u8>],
    offset: u64,
    latest_start: &mut Option<u64>,
    review_seed: &mut Option<u64>,
) {
    let line: Vec<u8> = first
        .iter()
        .copied()
        .chain(
            fragments
                .iter()
                .rev()
                .flat_map(|fragment| fragment.iter().copied()),
        )
        .collect();
    let Ok(record) = serde_json::from_slice::<Value>(&line) else {
        return;
    };
    let Some(payload) = record.get("payload") else {
        return;
    };
    if text(&record, "type") != Some("event_msg") {
        return;
    }
    if latest_start.is_none() && text(payload, "type") == Some("task_started") {
        *latest_start = Some(offset);
    }
    if review_seed.is_none()
        && text(payload, "type") == Some("item_completed")
        && payload.pointer("/item/type").and_then(Value::as_str) == Some("EnteredReviewMode")
        && crate::services::agent_protocol::codex_payload_turn_id(payload)
            .is_some_and(|id| !id.trim().is_empty())
    {
        *review_seed = Some(offset);
    }
}

fn apply(record: &Value, open: &mut Option<Open>, review: &mut Review) {
    let Some(payload) = record.get("payload") else {
        return;
    };
    let turn_id = crate::services::agent_protocol::codex_payload_turn_id(payload);
    match (text(record, "type"), text(payload, "type")) {
        (Some("event_msg"), Some("task_started")) => {
            // A start that does not name its root cannot prove a model turn of its own.
            let root = text(payload, "root_turn_id").filter(|root| !root.trim().is_empty());
            let next = turn_id.zip(root).map(|(turn, root)| Open {
                turn: turn.to_string(),
                root: root.to_string(),
                ..Open::default()
            });
            *open = Some(next.unwrap_or(Open {
                unreadable: true,
                ..Open::default()
            }));
        }
        (Some("turn_context"), _) => {
            if let Some(open) = open.as_mut().filter(|open| turn_id == Some(&open.turn)) {
                open.context = true;
                let sandbox = payload.get("sandbox_policy");
                open.full_access =
                    sandbox.and_then(|policy| text(policy, "type")) == Some("danger-full-access");
            }
        }
        (Some("event_msg"), Some("item_started" | "item_completed")) => {
            let item = payload.get("item").and_then(|item| text(item, "type"));
            match item.filter(|_| text(payload, "type") == Some("item_completed")) {
                Some("EnteredReviewMode") => {
                    *review = turn_id.map_or(Review::Unknown, |id| Review::Active(id.to_string()));
                }
                // Only the open review's own exit ends it.
                Some("ExitedReviewMode") if ends(review, turn_id) => *review = Review::Clear,
                _ => {}
            }
            let Some(open) = open.as_mut().filter(|open| turn_id == Some(&open.turn)) else {
                return;
            };
            if item == Some("UserMessage") && text(payload, "type") == Some("item_completed") {
                open.user = true;
            }
            if item == Some("ContextCompaction") {
                open.compaction = true;
            }
        }
        (Some("compacted"), _) => {
            if let Some(open) = open
                .as_mut()
                .filter(|open| turn_id.is_none_or(|turn| turn == open.turn))
            {
                open.compaction = true;
            }
        }
        (Some("event_msg"), Some("task_complete" | "turn_aborted")) => {
            if ends(review, turn_id) {
                *review = Review::Clear;
            }
            // An unnamed end may be this turn's, so the turn reads as Unknown.
            if let Some(open) = open.as_mut() {
                match turn_id {
                    Some(turn) if turn == open.turn || turn == open.root => open.closed = true,
                    Some(_) => {}
                    None => open.unreadable = true,
                }
            }
        }
        _ => {}
    }
}

fn ends(review: &Review, turn_id: Option<&str>) -> bool {
    matches!(review, Review::Active(open) if turn_id == Some(open.as_str()))
}

/// The turn a complete user record after `offset` carrying the nonce names, the first one found.
/// The outer Some is the sighting; the inner value is the turn id when the record names one.
pub(crate) fn submitted(path: &Path, offset: u64, nonce: &str) -> Option<Option<String>> {
    #[cfg(test)]
    if let Some(source) = crate::services::tui_input::human_frame::history::v2::scoped_source() {
        let source = source.ok()?;
        if source.provider != "codex"
            || crate::services::codex_tui::rollout_index::read_rollout_session_meta(path)?
                .id
                .as_deref()
                != Some(&source.session_id)
        {
            return None;
        }
    }

    let mut file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() < offset {
        return None;
    }
    file.seek(SeekFrom::Start(offset)).ok()?;
    let marker = format!(" · {nonce}]");
    let allows_legacy =
        crate::services::tui_input::human_frame::history::allows_legacy(nonce, path);
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    let mut position = offset;
    while reader
        .read_until(b'\n', &mut line)
        .is_ok_and(|read| read > 0)
    {
        let start = position;
        position += line.len() as u64;
        if line.ends_with(b"\n")
            && let Ok(record) = serde_json::from_slice::<Value>(&line)
            && let Some((input, turn)) = user_input(&record)
        {
            if let Some(receipt) = super::observe_native_record(path, start, &record) {
                if receipt.attempt.id == nonce {
                    return Some(receipt.native_turn);
                }
                line.clear();
                continue;
            }
            let legacy = !input.starts_with("[User: ")
                || input
                    .split(']')
                    .next()
                    .is_some_and(|header| header.contains(" · 📱 v1 mode="));
            if allows_legacy && legacy && input.contains(&marker) {
                return Some(turn);
            }
            // The UserMessage completion owns a native item id; its paired response item does not.
            if text(&record, "type") != Some("event_msg") {
                line.clear();
                continue;
            }
            let exact_text = record
                .pointer("/payload/item/content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks.iter().all(|block| {
                        matches!(text(block, "type"), Some("input_text" | "text"))
                            && text(block, "text").is_some()
                    })
                });
            if !exact_text {
                line.clear();
                continue;
            }
            let Some(row) = record
                .pointer("/payload/item/id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
            else {
                line.clear();
                continue;
            };
            let ledger = crate::services::tui_input::human_frame::history::registered;
            if ledger(nonce, path, &input)
                && crate::services::tui_input::human_frame::history::receipt(
                    nonce,
                    path,
                    start,
                    "codex",
                    Some(row),
                    &input,
                ) == Some(true)
            {
                return Some(turn);
            }
        }
        line.clear();
    }
    None
}

/// The text and turn of a user message record; other records never count.
fn user_input(record: &Value) -> Option<(String, Option<String>)> {
    let payload = record.get("payload")?;
    let joined = |content: &Value| -> String {
        let blocks = content.as_array().into_iter().flatten();
        let texts = blocks.filter_map(|block| text(block, "text"));
        texts.collect::<Vec<_>>().join("\n")
    };
    match (text(record, "type")?, text(payload, "type")?) {
        ("response_item", "message") if text(payload, "role") == Some("user") => {
            let meta = payload.get("internal_chat_message_metadata_passthrough");
            let turn = meta.and_then(|meta| text(meta, "turn_id"));
            Some((joined(payload.get("content")?), turn.map(str::to_string)))
        }
        ("event_msg", "item_completed") => {
            let item = payload.get("item")?;
            if text(item, "type")? != "UserMessage" {
                return None;
            }
            let turn = text(payload, "turn_id").map(str::to_string);
            Some((joined(item.get("content")?), turn))
        }
        _ => None,
    }
}

/// Collect receipts without holding the composer lock or deciding whether injection succeeded.
pub(super) fn observe(
    request: &super::Request<'_>,
    offset: u64,
    frame: &str,
    timing: &super::Timing,
) {
    let path = request.rollout.to_path_buf();
    let session = request.session.to_string();
    let nonce = request.nonce.to_string();
    let target_turn = request.target_turn.to_string();
    let frame = frame.to_string();
    let window = timing.confirm_window;
    let poll = timing.confirm_poll;
    let _ = std::thread::Builder::new()
        .name("codex-receipt".into())
        .spawn(move || {
            let deadline = std::time::Instant::now() + window;
            loop {
                if std::time::Instant::now() >= deadline {
                    tracing::debug!(%nonce, "Codex injection receipt not observed within diagnostic window");
                    return;
                }
                if let Some(observed_turn) = submitted(&path, offset, &nonce) {
                    if let Some(row) = crate::services::tui_input::human_frame::history::receipt_row(
                        &nonce, &path,
                    ) {
                        crate::services::tui_prompt_dedupe::observe_codex_prompt_in_turn_at(
                            &session,
                            &frame,
                            Some(&row),
                            observed_turn.as_deref(),
                            chrono::Utc::now(),
                        );
                    }
                    let joined = observed_turn.as_deref() == Some(target_turn.as_str());
                    tracing::debug!(%nonce, ?observed_turn, joined, "Codex injection receipt diagnostic");
                    return;
                }
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                std::thread::sleep(poll.min(left));
            }
        });
}

#[cfg(test)]
#[path = "scan_tests.rs"]
mod scan_tests;
