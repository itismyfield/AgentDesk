//! Rollout discovery, session metadata matching, and path identity.

use serde_json::Value;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::rollout_files_under;

/// The Codex home this host reads rollouts under, honouring the `CODEX_HOME`
/// override. Split out of `default_codex_sessions_dir` for #5452 PR-A so
/// `tmux_common::classify_watcher_jsonl_owner` recognises a provider-owned file
/// anywhere under the home rather than only under `sessions/` — Codex keeps
/// other state there too, and none of it is AgentDesk's to rewrite.
pub(crate) fn default_codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
}

pub fn default_codex_sessions_dir() -> Option<PathBuf> {
    default_codex_home().map(|home| home.join("sessions"))
}

pub(crate) fn observe_rollout_turn_state(
    rollout_path: &Path,
) -> crate::services::tui_turn_state::TuiTurnState {
    crate::services::tui_turn_state::observe_codex_jsonl_turn_state(rollout_path)
}

pub(crate) fn rollout_file_matches_cwd(rollout_path: &Path, cwd: &Path) -> bool {
    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    rollout_session_cwd_matches(rollout_path, &canonical_cwd)
}

/// Find a codex rollout transcript by its session UUID alone, scanning the
/// configured codex sessions directory. Used by recovery / restore paths after
/// a dcserver restart: the inflight row carries `session_id` but its stored
/// `output_path` may point at the AgentDesk-side relay JSONL that does not
/// exist for codex_tui (codex writes directly to `~/.codex/sessions/...`).
/// Returns the first matching `rollout-*-<session_id>.jsonl` whose filename
/// ends with the session UUID — codex assigns one rollout per session id.
pub fn find_rollout_by_session_id(session_id: &str) -> Option<PathBuf> {
    let sessions_dir = default_codex_sessions_dir()?;
    find_rollout_by_session_id_under(&sessions_dir, session_id)
}

pub(crate) fn find_rollout_by_session_id_under(
    sessions_dir: &Path,
    session_id: &str,
) -> Option<PathBuf> {
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        return None;
    }
    let suffix = format!("-{trimmed}.jsonl");
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for path in rollout_files_under(sessions_dir) {
        let matches = path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.ends_with(&suffix));
        if !matches {
            continue;
        }
        let modified = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if best
            .as_ref()
            .is_none_or(|(best_modified, _)| modified >= *best_modified)
        {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}

pub(super) fn latest_rollout_for_cwd_and_session_since(
    cwd: &Path,
    session_id: &str,
    modified_since: SystemTime,
    sessions_dir: &Path,
) -> Option<PathBuf> {
    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for path in rollout_files_under(sessions_dir) {
        let Some(modified) = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok()
        else {
            continue;
        };
        if modified < modified_since {
            continue;
        }
        if !rollout_session_meta_matches(&path, &canonical_cwd, Some(session_id)) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(best_modified, _)| modified > *best_modified)
        {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}

fn rollout_session_cwd_matches(path: &Path, cwd: &Path) -> bool {
    rollout_session_meta_matches(path, cwd, None)
}

fn rollout_session_meta_matches(path: &Path, cwd: &Path, session_id: Option<&str>) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().map_while(Result::ok).take(20) {
        let Ok(json) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if json.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let Some(payload) = json.get("payload") else {
            continue;
        };
        if let Some(expected_session_id) = session_id {
            let Some(actual_session_id) = payload.get("id").and_then(Value::as_str) else {
                continue;
            };
            if actual_session_id != expected_session_id {
                continue;
            }
        }
        let Some(raw_cwd) = payload.get("cwd").and_then(Value::as_str) else {
            continue;
        };
        let session_cwd = std::fs::canonicalize(raw_cwd).unwrap_or_else(|_| PathBuf::from(raw_cwd));
        return session_cwd == cwd;
    }
    false
}

pub(super) fn rollout_file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|metadata| metadata.len())
}

pub(super) fn same_path(left: &Path, right: &Path) -> bool {
    let canonical_left = std::fs::canonicalize(left).ok();
    let canonical_right = std::fs::canonicalize(right).ok();
    match (canonical_left, canonical_right) {
        (Some(left), Some(right)) => left == right,
        _ => left == right,
    }
}
