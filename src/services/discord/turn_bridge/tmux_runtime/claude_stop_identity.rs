//! Identity of the native user turn an Escape targets.

use crate::services::provider::CancelToken;
use crate::services::provider::native_stop::NativeUserPin;
use std::io::{BufRead, Seek};
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

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

/// The token whose own native input the identity is, and how far its later rows were judged.
#[derive(Clone)]
struct PinnedTurn {
    token: Weak<CancelToken>,
    session: String,
    native_session: String,
    judged: Arc<Mutex<u64>>,
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
    BusyChild,
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
                judged: Arc::new(Mutex::new(pin.range.1)),
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

    /// Reads only the rows past the last judged one, so a recheck under a lock stays small.
    fn pinned_still_current(&self, pinned: &PinnedTurn) -> bool {
        let Some(token) = pinned.token.upgrade() else {
            return false;
        };
        let mut judged = pinned.judged.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(mut file) = std::fs::File::open(&self.output_path) else {
            return false;
        };
        let Ok(metadata) = file.metadata() else {
            return false;
        };
        if transcript_file_identity(&metadata) != self.file_identity
            || metadata.len() < *judged
            || file.seek(std::io::SeekFrom::Start(*judged)).is_err()
        {
            return false;
        }
        let mut reader = std::io::BufReader::new(file);
        let (mut line, mut offset) = (Vec::new(), *judged);
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) if line.ends_with(b"\n") => {}
                Ok(_) => return false,
                Err(_) => return false,
            }
            let record = serde_json::from_slice(&line).unwrap_or(serde_json::Value::Null);
            match later_row(
                &record,
                Some(&token),
                &pinned.session,
                &pinned.native_session,
                Path::new(&self.output_path),
            ) {
                Later::Neutral | Later::BusyChild => offset += line.len() as u64,
                Later::Other | Later::Undecided => return false,
            }
        }
        *judged = offset;
        true
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
    while offset < end {
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
        match later_row(&record, None, session, native_session, path) {
            Later::Neutral => {}
            Later::BusyChild => owned = true,
            Later::Other => clean = false,
            Later::Undecided => return Err("owned-input row ownership unreadable".into()),
        }
    }
    Ok((owned, clean))
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
    if record["isSidechain"] == true {
        return Later::Neutral;
    }
    if !record.is_object() || record["type"].as_str().is_none() {
        return Later::Undecided;
    }
    if record
        .get("sessionId")
        .is_some_and(|id| id.as_str() != Some(native_session))
    {
        return Later::Other;
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
            Later::BusyChild
        }
        RowOwner::Command(id) => {
            match history::discord_owner_attempt(session, native_session, transcript, &id) {
                Ok(true) => Later::BusyChild,
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
