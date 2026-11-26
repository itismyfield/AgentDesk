//! The v2 input hold: what the write that made it knew, never the prompt text itself.
use super::{InputRun, hold_path};
use crate::services::claude_tui::host_input::StopCause;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SendOutcome {
    /// The write has not recorded its result: the paste and the Enter may have gone.
    InFlight,
    EnterRefused,
    CancelledBeforeEnter,
    PasteSendFailed,
    EnterSendFailed,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UnsentProof {
    None,
    SendOutcome,
    ExactTextSeen,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CleanupPhase {
    None,
    Clearing,
    Cleared,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct HoldRecord {
    pub(crate) schema: u8,
    pub(crate) recorded_at: String,
    pub(crate) provider: String,
    pub(crate) logical_key: String,
    pub(crate) execution_nonce: String,
    pub(crate) hold_id: String,
    pub(crate) frame_sha256: String,
    pub(crate) screen_exact: bool,
    pub(crate) transcript: String,
    pub(crate) file: Option<(u64, u64)>,
    pub(crate) offset_before: u64,
    pub(crate) started_at: String,
    pub(crate) send_outcome: SendOutcome,
    /// When the final outcome was recorded; the windows run from it, else from `recorded_at`.
    pub(crate) outcome_at: Option<String>,
    pub(crate) unsent_proof: UnsentProof,
    pub(crate) cleanup_phase: CleanupPhase,
    pub(crate) cleanup_at: Option<String>,
    /// The revision of the readiness reservation this process holds for the input, if any.
    pub(crate) reservation: Option<u64>,
    pub(crate) unclear_passes: u32,
    pub(crate) notice_intent_at: Option<String>,
    pub(crate) notice_enqueued_at: Option<String>,
}

/// A hold as read: a pre-deploy v1 file holds only a time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Held {
    V1 { hold_id: String },
    V2(Box<HoldRecord>),
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// A frame the screen can show without loss: no tab, control byte, or row ending in blanks.
pub(crate) fn screen_exact(frame: &str) -> bool {
    !frame.is_empty()
        && frame.lines().count() == frame.split('\n').count()
        && frame
            .split('\n')
            .all(|line| line == line.trim_end() && !line.chars().any(char::is_control))
}

/// `None` when execution `nonce` holds nothing; a v1 hold's identity is its file content.
pub fn read(nonce: &str) -> Result<Option<Held>, String> {
    let text = match std::fs::read(hold_path(nonce)?) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("herdr turn: input hold unreadable: {error}")),
    };
    match serde_json::from_slice::<HoldRecord>(&text) {
        Ok(record) if record.schema == 2 && record.execution_nonce == nonce => {
            Ok(Some(Held::V2(Box::new(record))))
        }
        Ok(_) => Err("herdr turn: input hold names another execution".into()),
        Err(_) if text.first() == Some(&b'{') => Err("herdr turn: input hold malformed".into()),
        Err(_) => Ok(Some(Held::V1 {
            hold_id: sha256_hex(&[nonce.as_bytes(), b"\0", &text].concat()),
        })),
    }
}

/// Temp file, sync, rename, directory sync: a crash leaves the old or the new record whole.
pub fn write(record: &HoldRecord) -> Result<PathBuf, String> {
    use std::io::Write;
    let path = hold_path(&record.execution_nonce)?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let temp = dir.join(format!(".{}.tmp", record.execution_nonce));
    let body = serde_json::to_vec(record).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::File::create(&temp))
        .and_then(|mut file| {
            file.write_all(&body)?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&temp, &path))
        .and_then(|()| crate::domain::file_sync::fsync_parent_dir(&path))
        .map_err(|error| format!("herdr turn: input hold not recorded: {error}"))?;
    Ok(path)
}

/// The final send outcome and its proof, written once over `in_flight`; `None` when the
/// composer is as before the run (submitted, or never written) and the hold ends.
pub(crate) fn outcome(run: &InputRun) -> Option<(SendOutcome, UnsentProof)> {
    let proven = |outcome| Some((outcome, UnsentProof::SendOutcome));
    match run {
        InputRun::Applied | InputRun::Refused(_) | InputRun::Cancelled { confirmed: 0 } => None,
        InputRun::Indeterminate {
            confirmed: 1,
            cause: StopCause::Refused(_),
        } => proven(SendOutcome::EnterRefused),
        InputRun::Cancelled { confirmed: 1 } => proven(SendOutcome::CancelledBeforeEnter),
        InputRun::Indeterminate { confirmed: 0, .. } => proven(SendOutcome::PasteSendFailed),
        _ => Some((SendOutcome::EnterSendFailed, UnsentProof::None)),
    }
}

/// Retries only the directory sync of an unlinked hold; unlinking again would report Released.
pub(crate) fn ensure_absent_durable(nonce: &str) -> Result<(), String> {
    let path = hold_path(nonce)?;
    if path.exists() {
        return Err("herdr turn: input hold came back".into());
    }
    crate::domain::file_sync::fsync_parent_dir(&path).map_err(|error| error.to_string())
}

/// The status fields of execution `nonce`'s v2 hold; no path or input text. A v1 hold has none.
pub fn summary(nonce: &str) -> Option<serde_json::Value> {
    let Ok(Some(Held::V2(record))) = read(nonce) else {
        return None;
    };
    Some(serde_json::json!({
        "send_outcome": record.send_outcome, "unsent_proof": record.unsent_proof,
        "cleanup_phase": record.cleanup_phase, "unclear_passes": record.unclear_passes,
        "notice_intent_at": record.notice_intent_at,
        "notice_enqueued_at": record.notice_enqueued_at,
    }))
}
