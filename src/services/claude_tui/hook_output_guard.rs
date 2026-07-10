//! Bounded Claude Stop/SubagentStop transcript inspection.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::services::provider::ProviderKind;
use crate::services::provider_output_guard::{ProviderOutputVerdict, inspect_provider_output};

pub(crate) const CLAUDE_HOOK_BLOCK_REASON: &str =
    "내부 제어 데이터가 섞인 응답입니다. 원문을 출력하지 말고 안전한 최종 답변을 다시 작성하세요.";
const MAX_TRANSCRIPT_TAIL_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HookOutputGuardError {
    MissingProjectsRoot,
    MissingTranscriptPath,
    InvalidProjectsRoot,
    InvalidTranscriptPath,
    OutsideProjectsRoot,
    NotRegularFile,
    ReadFailed,
    OversizedTailRecord,
    InvalidUtf8,
    MalformedJsonl,
    MissingAssistantText,
}

impl HookOutputGuardError {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::MissingProjectsRoot => "missing_projects_root",
            Self::MissingTranscriptPath => "missing_transcript_path",
            Self::InvalidProjectsRoot => "invalid_projects_root",
            Self::InvalidTranscriptPath => "invalid_transcript_path",
            Self::OutsideProjectsRoot => "outside_projects_root",
            Self::NotRegularFile => "not_regular_file",
            Self::ReadFailed => "read_failed",
            Self::OversizedTailRecord => "oversized_tail_record",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::MalformedJsonl => "malformed_jsonl",
            Self::MissingAssistantText => "missing_assistant_text",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HookOutputInspection {
    pub(crate) verdict: ProviderOutputVerdict,
    pub(crate) byte_len: usize,
    pub(crate) char_len: usize,
}

pub(crate) fn configured_claude_projects_root() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
        .map(|root| root.join("projects"))
}

pub(crate) fn inspect_claude_hook_output(
    payload: &Value,
    projects_root: Option<&Path>,
) -> Result<HookOutputInspection, HookOutputGuardError> {
    let projects_root = projects_root.ok_or(HookOutputGuardError::MissingProjectsRoot)?;
    let transcript_path = payload
        .get("transcript_path")
        .or_else(|| payload.get("transcriptPath"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(HookOutputGuardError::MissingTranscriptPath)?;
    inspect_claude_transcript(Path::new(transcript_path), projects_root)
}

pub(crate) fn inspect_claude_transcript(
    transcript_path: &Path,
    projects_root: &Path,
) -> Result<HookOutputInspection, HookOutputGuardError> {
    let canonical_root = projects_root
        .canonicalize()
        .map_err(|_| HookOutputGuardError::InvalidProjectsRoot)?;
    let canonical_transcript = transcript_path
        .canonicalize()
        .map_err(|_| HookOutputGuardError::InvalidTranscriptPath)?;
    if !canonical_transcript.starts_with(&canonical_root) {
        return Err(HookOutputGuardError::OutsideProjectsRoot);
    }

    let mut file =
        File::open(&canonical_transcript).map_err(|_| HookOutputGuardError::ReadFailed)?;
    let metadata = file
        .metadata()
        .map_err(|_| HookOutputGuardError::ReadFailed)?;
    if !metadata.file_type().is_file() {
        return Err(HookOutputGuardError::NotRegularFile);
    }
    let start = metadata
        .len()
        .saturating_sub(MAX_TRANSCRIPT_TAIL_BYTES as u64);
    file.seek(SeekFrom::Start(start))
        .map_err(|_| HookOutputGuardError::ReadFailed)?;
    let mut bytes = Vec::with_capacity(MAX_TRANSCRIPT_TAIL_BYTES);
    file.take(MAX_TRANSCRIPT_TAIL_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| HookOutputGuardError::ReadFailed)?;
    if bytes.len() > MAX_TRANSCRIPT_TAIL_BYTES {
        return Err(HookOutputGuardError::OversizedTailRecord);
    }
    if start > 0 {
        let first_newline = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or(HookOutputGuardError::OversizedTailRecord)?;
        bytes.drain(..=first_newline);
    }
    let tail = std::str::from_utf8(&bytes).map_err(|_| HookOutputGuardError::InvalidUtf8)?;
    let assistant_text = latest_assistant_text(tail)?;
    Ok(HookOutputInspection {
        verdict: inspect_provider_output(&ProviderKind::Claude, &assistant_text),
        byte_len: assistant_text.len(),
        char_len: assistant_text.chars().count(),
    })
}

fn latest_assistant_text(tail: &str) -> Result<String, HookOutputGuardError> {
    let mut latest = None;
    for line in tail.lines().filter(|line| !line.trim().is_empty()) {
        let record: Value =
            serde_json::from_str(line).map_err(|_| HookOutputGuardError::MalformedJsonl)?;
        if record.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(message) = record.get("message") else {
            continue;
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        let text = blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            latest = Some(text);
        }
    }
    latest.ok_or(HookOutputGuardError::MissingAssistantText)
}
