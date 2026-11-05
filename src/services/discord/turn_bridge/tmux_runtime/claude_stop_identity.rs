//! Identity of the native user turn an Escape targets.

use crate::services::provider::CancelToken;
use crate::services::provider::native_stop::NativeUserPin;
use std::io::{BufRead, Seek};
use std::path::Path;
use std::sync::{Arc, Weak};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ClaudeStopTurnIdentity {
    output_path: String,
    file_identity: (u64, u64),
    user_line_start: u64,
    user_line_end: u64,
    user_line_hash: u64,
    user_entry_id: Option<String>,
    pinned: Option<PinnedTurn>,
}

/// The token whose own native input the identity is.
#[derive(Clone)]
struct PinnedTurn {
    token: Weak<CancelToken>,
    session: String,
    native_session: String,
}

impl PartialEq for PinnedTurn {
    fn eq(&self, other: &Self) -> bool {
        self.session == other.session
            && self.native_session == other.native_session
            && Weak::ptr_eq(&self.token, &other.token)
    }
}

impl Eq for PinnedTurn {}

impl std::fmt::Debug for PinnedTurn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedTurn")
            .field("session", &self.session)
            .finish()
    }
}

/// What a row after the pinned input says about whether the pinned turn is still the one running.
enum Later {
    Neutral,
    BusyChild(String),
    Other,
    Undecided,
}

impl ClaudeStopTurnIdentity {
    pub(super) fn capture(output_path: &str) -> Option<Self> {
        latest_claude_user_turn_identity(Path::new(output_path))
    }

    /// The token's verified native input; later rows must be neutral or its own busy inputs.
    pub(super) fn pinned(pin: &NativeUserPin, token: &Arc<CancelToken>) -> Self {
        Self {
            output_path: pin.output_path.clone(),
            file_identity: pin.source_file,
            user_line_start: pin.range.0,
            user_line_end: pin.range.1,
            user_line_hash: 0,
            user_entry_id: Some(pin.uuid.clone()),
            pinned: Some(PinnedTurn {
                token: Arc::downgrade(token),
                session: pin.tmux_session.clone(),
                native_session: pin.session_id.clone(),
            }),
        }
    }

    pub(super) fn still_current(&self) -> bool {
        match &self.pinned {
            Some(pinned) => self.pinned_still_current(pinned),
            None => {
                latest_claude_user_turn_identity(Path::new(&self.output_path)).as_ref()
                    == Some(self)
            }
        }
    }

    /// Rebuilds pairing from the root so an earlier enqueue survives every stop recheck.
    fn pinned_still_current(&self, pinned: &PinnedTurn) -> bool {
        let Some(token) = pinned.token.upgrade() else {
            return false;
        };
        let Ok(metadata) = std::fs::metadata(&self.output_path) else {
            return false;
        };
        input_rows(
            Path::new(&self.output_path),
            self.file_identity,
            &pinned.session,
            &pinned.native_session,
            self.user_line_end,
            metadata.len(),
            Some(&token),
        )
        .is_ok_and(|(_, clean)| clean)
    }
}

/// All human inputs after the opener must belong to durable Discord busy attempts.
pub(super) fn owned_input_rows(
    path: &Path,
    source_file: (u64, u64),
    session: &str,
    native_session: &str,
    opener_end: u64,
    end: u64,
) -> Result<(bool, bool), String> {
    input_rows(
        path,
        source_file,
        session,
        native_session,
        opener_end,
        end,
        None,
    )
}

struct QueuedInput {
    offset: u64,
    body: String,
    command: bool,
    absorbed: bool,
    withdrawn: bool,
}

impl QueuedInput {
    fn ran(&self) -> bool {
        self.command || self.absorbed
    }
}

fn input_rows(
    path: &Path,
    source_file: (u64, u64),
    session: &str,
    native_session: &str,
    opener_end: u64,
    end: u64,
    token: Option<&CancelToken>,
) -> Result<(bool, bool), String> {
    use crate::services::tui_input::human_frame::history::{self, RowOwner};
    use crate::services::tui_o::writer::input_facts::{QueueRecord, classify_queued_record};
    let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if transcript_file_identity(&metadata) != source_file
        || opener_end > end
        || metadata.len() < end
    {
        return Err("owned-input transcript source changed".into());
    }
    file.seek(std::io::SeekFrom::Start(opener_end))
        .map_err(|error| error.to_string())?;
    let mut reader = std::io::BufReader::new(file);
    let (mut line, mut offset) = (Vec::new(), opener_end);
    let (mut owned, mut clean) = (false, true);
    let (mut queued, mut reserved) = (Vec::<QueuedInput>::new(), Vec::new());
    while offset < end {
        let start = offset;
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| error.to_string())?;
        offset += read as u64;
        if read == 0 || offset > end || !line.ends_with(b"\n") {
            return Err("owned-input transcript record incomplete".into());
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let record = serde_json::from_slice(&line).map_err(|error| error.to_string())?;
        let later = if let Some(boundary) = row_boundary(&record, native_session) {
            boundary
        } else {
            match classify_queued_record(&record) {
                Some(QueueRecord::Enqueue(body)) => {
                    queued.push(QueuedInput {
                        offset: start,
                        body,
                        command: false,
                        absorbed: false,
                        withdrawn: false,
                    });
                    Later::Neutral
                }
                Some(QueueRecord::Absorbed(body)) => {
                    let at = queued
                        .iter()
                        .position(|input| {
                            input.body == body
                                && input.command
                                && !input.absorbed
                                && !input.withdrawn
                        })
                        .or_else(|| {
                            queued.iter().position(|input| {
                                input.body == body && !input.ran() && !input.withdrawn
                            })
                        });
                    let Some(at) = at else {
                        return Err("Claude absorption has no corresponding enqueue".into());
                    };
                    queued[at].absorbed = true;
                    Later::Neutral
                }
                Some(QueueRecord::Human(body)) => queued_command(&mut queued, body)
                    .unwrap_or_else(|| later_row(&record, token, session, native_session, path)),
                Some(QueueRecord::Command(body))
                    if record["attachment"]["commandMode"] == "prompt"
                        && record["attachment"]["origin"]["kind"].as_str().is_none() =>
                {
                    queued_command(&mut queued, body)
                        .unwrap_or_else(|| later_row(&record, token, session, native_session, path))
                }
                Some(QueueRecord::Withdraw(body) | QueueRecord::Delivered(body)) => {
                    if let Some(input) = queued
                        .iter_mut()
                        .find(|input| input.body == body && !input.ran() && !input.withdrawn)
                    {
                        input.withdrawn = true;
                    }
                    Later::Neutral
                }
                Some(QueueRecord::WithdrawAll) => {
                    for input in &mut queued {
                        if !input.ran() {
                            input.withdrawn = true;
                        }
                    }
                    Later::Neutral
                }
                None if record["type"] == "queue-operation" => Later::Undecided,
                _ => later_row(&record, token, session, native_session, path),
            }
        };
        match later {
            Later::Neutral => {}
            Later::BusyChild(id) => {
                owned = true;
                reserved.push(id);
            }
            Later::Other => clean = false,
            Later::Undecided => return Err("owned-input row ownership unreadable".into()),
        }
    }
    if queued.iter().any(QueuedInput::ran) {
        let enqueues: Vec<_> = queued
            .iter()
            .map(|input| (input.offset, input.body.clone()))
            .collect();
        let owners = history::discord_enqueue_owners(
            session,
            native_session,
            path,
            opener_end,
            &enqueues,
            &reserved,
        )?;
        for (input, owner) in queued.iter().zip(owners) {
            if !input.ran() {
                continue;
            }
            match owner {
                RowOwner::Command(_) => owned = true,
                RowOwner::Unread => return Err("Claude absorption owner is ambiguous".into()),
                RowOwner::None | RowOwner::User(_) => clean = false,
            }
        }
    }
    Ok((owned, clean))
}

fn queued_command(queued: &mut [QueuedInput], body: String) -> Option<Later> {
    let at = queued
        .iter()
        .position(|input| {
            input.body == body && input.absorbed && !input.command && !input.withdrawn
        })
        .or_else(|| {
            queued
                .iter()
                .position(|input| input.body == body && !input.ran() && !input.withdrawn)
        })?;
    queued[at].command = true;
    Some(Later::Neutral)
}

fn row_boundary(record: &serde_json::Value, native_session: &str) -> Option<Later> {
    if record["isSidechain"] == true {
        return Some(Later::Neutral);
    }
    if !record.is_object() || record["type"].as_str().is_none() {
        return Some(Later::Undecided);
    }
    record
        .get("sessionId")
        .is_some_and(|id| id.as_str() != Some(native_session))
        .then_some(Later::Other)
}

/// A later user opens another turn; only an exact registered busy command joins this one.
fn later_row(
    record: &serde_json::Value,
    token: Option<&CancelToken>,
    session: &str,
    native_session: &str,
    transcript: &Path,
) -> Later {
    use crate::services::tui_input::human_frame::history::{self, RowOwner};
    if let Some(boundary) = row_boundary(record, native_session) {
        return boundary;
    }
    if record["type"] == "user"
        && crate::services::tui_o::shadow::identity::record_is_compact_summary(record)
    {
        return Later::Neutral;
    }
    // A tool result's envelope may carry hook context beside it; that text is no new input.
    let blocks = record["message"]["content"]
        .as_array()
        .into_iter()
        .flatten();
    if record["type"] == "user"
        && blocks
            .into_iter()
            .any(|block| block["type"] == "tool_result")
    {
        return Later::Neutral;
    }
    if record["type"] == "user" && record["isMeta"] != true {
        return Later::Other;
    }
    let command = &record["attachment"];
    if record["type"] != "attachment"
        || command["type"] != "queued_command"
        || command["origin"]["kind"]
            .as_str()
            .is_some_and(|kind| kind != "human")
    {
        return Later::Neutral;
    }
    match command["commandMode"].as_str() {
        Some("prompt") => {}
        Some(_) => return Later::Neutral,
        None => return Later::Undecided,
    }
    let (Some(prompt), Some(row)) = (command["prompt"].as_str(), record["uuid"].as_str()) else {
        return Later::Other;
    };
    match history::row_owner("claude", session, &[prompt.to_string()], row) {
        RowOwner::Command(id) | RowOwner::User(id)
            if token.is_some_and(|token| token.native_stop.is_busy_child(&id)) =>
        {
            Later::BusyChild(id)
        }
        RowOwner::Command(id) => {
            match history::discord_owner_attempt(session, native_session, transcript, &id) {
                Ok(true) => Later::BusyChild(id),
                Ok(false) => Later::Other,
                Err(_) => Later::Undecided,
            }
        }
        RowOwner::Unread => Later::Undecided,
        _ => Later::Other,
    }
}

#[cfg(unix)]
fn transcript_file_identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn transcript_file_identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    let created_ns = metadata
        .created()
        .ok()
        .and_then(|created| created.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0);
    (created_ns, 0)
}

fn latest_claude_user_turn_identity(path: &Path) -> Option<ClaudeStopTurnIdentity> {
    use std::hash::{Hash, Hasher};

    const MAX_TAIL_BYTES: u64 = 256 * 1024;

    let mut file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let len = metadata.len();
    let file_identity = transcript_file_identity(&metadata);
    let start = len.saturating_sub(MAX_TAIL_BYTES);
    file.seek(std::io::SeekFrom::Start(start)).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    let mut offset = start;
    if start > 0 {
        offset = offset.saturating_add(reader.read_line(&mut line).ok()? as u64);
    }

    let mut latest = None;
    loop {
        line.clear();
        let line_start = offset;
        let bytes_read = reader.read_line(&mut line).ok()?;
        if bytes_read == 0 {
            return latest;
        }
        offset = offset.saturating_add(bytes_read as u64);
        if !line.ends_with('\n') {
            return latest;
        }
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let Some((_prompt, user_entry_id)) =
            crate::services::tui_prompt_dedupe::extract_claude_transcript_user_prompt_with_entry_id(
                &json,
            )
        else {
            continue;
        };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        line.as_bytes().hash(&mut hasher);
        latest = Some(ClaudeStopTurnIdentity {
            output_path: path.display().to_string(),
            file_identity,
            user_line_start: line_start,
            user_line_end: offset,
            user_line_hash: hasher.finish(),
            user_entry_id,
            pinned: None,
        });
    }
}
