//! Identity of the native user turn an Escape targets.

use std::io::{BufRead, Seek};
use std::path::Path;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ClaudeStopTurnIdentity {
    output_path: String,
    file_identity: (u64, u64),
    user_line_start: u64,
    user_line_end: u64,
    user_line_hash: u64,
    user_entry_id: Option<String>,
}

impl ClaudeStopTurnIdentity {
    pub(super) fn capture(output_path: &str) -> Option<Self> {
        claude_user_turn_identity(Path::new(output_path), None)
    }

    pub(super) fn matches_start(&self, output_path: &str, offset: u64) -> bool {
        std::fs::canonicalize(output_path)
            .ok()
            .zip(std::fs::canonicalize(&self.output_path).ok())
            .is_some_and(|(row, binding)| row == binding)
            && claude_user_turn_identity(Path::new(&self.output_path), Some(offset)).as_ref()
                == Some(self)
    }

    pub(super) fn still_current(&self) -> bool {
        claude_user_turn_identity(Path::new(&self.output_path), None).as_ref() == Some(self)
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

fn claude_user_turn_identity(
    path: &Path,
    first_from: Option<u64>,
) -> Option<ClaudeStopTurnIdentity> {
    use std::hash::{Hash, Hasher};

    const MAX_TAIL_BYTES: u64 = 256 * 1024;

    let mut file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let len = metadata.len();
    let file_identity = transcript_file_identity(&metadata);
    let start = first_from.unwrap_or_else(|| len.saturating_sub(MAX_TAIL_BYTES));
    file.seek(std::io::SeekFrom::Start(start)).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    let mut offset = start;
    if start > 0 && first_from.is_none() {
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
        });
        if first_from.is_some() {
            return latest;
        }
    }
}
