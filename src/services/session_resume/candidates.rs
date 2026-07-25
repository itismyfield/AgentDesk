use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

const TITLE_TAIL_READ_LIMIT: u64 = 64 * 1024;
const MAX_INVENTORY_CANDIDATES: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ResumeCandidate {
    pub(crate) session_id: String,
    pub(crate) cwd: String,
    pub(crate) modified_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
}

pub(crate) fn discover_candidates(
    current_cwd: Option<&str>,
    current_session_id: Option<&str>,
    live_bound: &HashSet<String>,
    claude_home: Option<&Path>,
) -> Vec<ResumeCandidate> {
    let Some(current_cwd) = current_cwd else {
        return Vec::new();
    };
    let current_path = Path::new(current_cwd);
    let Some(parent) = current_path.parent() else {
        return Vec::new();
    };
    let Some(lineage) = current_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(worktree_lineage_stem)
    else {
        return Vec::new();
    };

    let mut worktrees = vec![current_path.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || path == current_path {
                continue;
            }
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .map(worktree_lineage_stem)
                == Some(lineage)
            {
                worktrees.push(path);
            }
        }
    }

    let empty_exclude = HashSet::new();
    let mut transcripts = Vec::new();
    for worktree in worktrees {
        for transcript in
            crate::services::claude_tui::transcript_tail::claude_transcripts_for_cwd_since(
                &worktree,
                UNIX_EPOCH,
                claude_home,
                &empty_exclude,
            )
        {
            let Some(session_id) = transcript
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if Some(session_id.as_str()) == current_session_id || live_bound.contains(&session_id) {
                continue;
            }
            let Ok(modified) = std::fs::metadata(&transcript).and_then(|meta| meta.modified())
            else {
                continue;
            };
            transcripts.push((modified, worktree.clone(), transcript, session_id));
        }
    }

    transcripts.sort_by(|left, right| right.0.cmp(&left.0));
    let mut seen_session_ids = HashSet::new();
    transcripts.retain(|(_, _, _, session_id)| seen_session_ids.insert(session_id.clone()));
    transcripts.truncate(MAX_INVENTORY_CANDIDATES);
    transcripts
        .into_iter()
        .map(
            |(modified, worktree, transcript, session_id)| ResumeCandidate {
                session_id,
                cwd: worktree.to_string_lossy().to_string(),
                modified_at_ms: unix_millis(modified),
                title: read_ai_title_bounded(&transcript, TITLE_TAIL_READ_LIMIT)
                    .ok()
                    .flatten(),
            },
        )
        .collect()
}

pub(crate) fn marker_session_id(value: &str) -> Option<&str> {
    let session_id = value.trim().strip_prefix("pick:")?;
    uuid::Uuid::parse_str(session_id).ok()?;
    Some(session_id)
}

fn unix_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn worktree_lineage_stem(dir_name: &str) -> &str {
    let bytes = dir_name.as_bytes();
    if bytes.len() > 16 {
        let tail = &dir_name[dir_name.len() - 16..];
        let tb = tail.as_bytes();
        if tb[0] == b'-'
            && tb[9] == b'-'
            && tb[1..9].iter().all(u8::is_ascii_digit)
            && tb[10..16].iter().all(u8::is_ascii_digit)
        {
            return &dir_name[..dir_name.len() - 16];
        }
    }
    dir_name
}

fn read_ai_title_bounded(path: &Path, limit: u64) -> std::io::Result<Option<String>> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(limit);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity((len - start).min(limit) as usize);
    file.take(limit).read_to_end(&mut bytes)?;

    if start > 0 {
        if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            bytes.drain(..=newline);
        } else {
            return Ok(None);
        }
    }

    for line in bytes.split(|byte| *byte == b'\n').rev() {
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) != Some("ai-title") {
            continue;
        }
        let title = value
            .get("aiTitle")
            .or_else(|| value.get("title"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_string);
        if title.is_some() {
            return Ok(title);
        }
    }
    Ok(None)
}
