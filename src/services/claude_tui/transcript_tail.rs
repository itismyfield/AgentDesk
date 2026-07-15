use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use crate::services::agent_protocol::StreamMessage;
use crate::services::session_backend::{StreamLineState, process_stream_line};
use chrono::{DateTime, Utc};

// #3034: test-only replay surface — `replay_transcript_file` and its outcome
// type are exercised by the transcript-parser regression suite but not wired to
// a production caller (replay now happens via the streaming session backend).
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptReplayOutcome {
    pub bytes_read: u64,
    pub lines_read: usize,
}

pub fn claude_transcript_path(
    cwd: &Path,
    session_id: &str,
    claude_home: Option<&Path>,
) -> Result<PathBuf, String> {
    if uuid::Uuid::parse_str(session_id).is_err() {
        return Err("invalid Claude session_id UUID".to_string());
    }
    let candidates = claude_transcript_path_candidates(cwd, session_id, claude_home)?;
    Ok(candidates
        .iter()
        .find(|path| path.exists())
        .or_else(|| {
            candidates
                .iter()
                .find(|path| path.parent().is_some_and(Path::exists))
        })
        .cloned()
        .unwrap_or_else(|| candidates[0].clone()))
}

pub fn claude_transcript_path_candidates(
    cwd: &Path,
    session_id: &str,
    claude_home: Option<&Path>,
) -> Result<Vec<PathBuf>, String> {
    if uuid::Uuid::parse_str(session_id).is_err() {
        return Err("invalid Claude session_id UUID".to_string());
    }
    let filename = format!("{session_id}.jsonl");
    Ok(claude_project_dir_candidates_for_cwd(cwd, claude_home)?
        .into_iter()
        .map(|project_dir| project_dir.join(&filename))
        .collect())
}

// #3034: test-only single-candidate helper; production resolves transcripts via
// `claude_project_dir_candidates_for_cwd` (the multi-candidate form).
#[allow(dead_code)]
pub fn claude_project_dir_for_cwd(
    cwd: &Path,
    claude_home: Option<&Path>,
) -> Result<PathBuf, String> {
    Ok(claude_project_dir_candidates_for_cwd(cwd, claude_home)?
        .into_iter()
        .next()
        .expect("candidate list is never empty"))
}

pub fn claude_project_dir_candidates_for_cwd(
    cwd: &Path,
    claude_home: Option<&Path>,
) -> Result<Vec<PathBuf>, String> {
    let home = claude_home
        .map(Path::to_path_buf)
        .or_else(default_claude_home)
        .ok_or_else(|| "Claude home directory is unavailable".to_string())?
        .join("projects");
    let mut path_candidates = Vec::new();
    if let Ok(canonical) = std::fs::canonicalize(cwd) {
        path_candidates.push(canonical);
    }
    if !path_candidates.iter().any(|path| path == cwd) {
        path_candidates.push(cwd.to_path_buf());
    }

    let mut project_dirs = Vec::new();
    for path in path_candidates {
        let project_dir = home.join(encode_project_path(&path));
        if !project_dirs.contains(&project_dir) {
            project_dirs.push(project_dir);
        }
    }
    Ok(project_dirs)
}

/// #2843: find the newest top-level Claude transcript (`<uuid>.jsonl`) under the
/// Claude project directory for `cwd` that was modified at/after
/// `modified_since`. The direct-TUI idle relay uses this to converge on the
/// SAME transcript a Discord-originated turn writes, even when the stored
/// runtime binding points at a stale/older transcript path (e.g. after a
/// redeploy rotated the Claude session_id, or when the binding never learned
/// the active transcript). Mirrors the codex-side `latest_rollout_for_cwd_since`.
///
/// `modified_since` discriminates against transcripts that predate this tmux
/// session (pass the session's launch-script mtime): a transcript older than the
/// session launch belongs to a prior session and must not be adopted. Pass
/// `UNIX_EPOCH` to disable the filter. NOTE: this does not fully disambiguate
/// two *concurrently active* Claude TUI sessions sharing one cwd — that needs a
/// per-session transcript identity tracked at handoff, which the binding does
/// not yet carry (session_id is registered as None by the Discord turn).
/// Returns `None` when no project directory or no qualifying transcript exists.
pub fn latest_claude_transcript_for_cwd(
    cwd: &Path,
    modified_since: std::time::SystemTime,
    claude_home: Option<&Path>,
    exclude: &std::collections::HashSet<PathBuf>,
) -> Option<PathBuf> {
    claude_transcripts_for_cwd_since(cwd, modified_since, claude_home, exclude)
        .into_iter()
        .next()
}

/// #3212 (codex P1): like [`latest_claude_transcript_for_cwd`] but returns ALL
/// qualifying `<uuid>.jsonl` transcripts under the cwd's Claude project dir(s),
/// newest mtime first, instead of only the single newest. The follow-up
/// readiness resolver needs the full candidate set so it can apply an
/// *ambiguity guard*: when more than one same-cwd transcript qualifies (two
/// concurrent same-cwd sessions) and there is no stronger per-session identity,
/// picking the newest mtime is exactly the false-ready / false-busy bug — the
/// caller refuses to guess. A single candidate is unambiguous and safe to adopt.
///
/// `modified_since` floors the result to transcripts at/after this session's
/// launch (pass `UNIX_EPOCH` to disable); `exclude` drops transcripts already
/// claimed by OTHER live sessions' bindings.
pub fn claude_transcripts_for_cwd_since(
    cwd: &Path,
    modified_since: std::time::SystemTime,
    claude_home: Option<&Path>,
    exclude: &std::collections::HashSet<PathBuf>,
) -> Vec<PathBuf> {
    let Ok(project_dirs) = claude_project_dir_candidates_for_cwd(cwd, claude_home) else {
        return Vec::new();
    };
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for project_dir in project_dirs {
        let Ok(entries) = std::fs::read_dir(&project_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // #2843 multi-session fix: skip transcripts already claimed by a
            // *different* live session's binding. Without this, concurrent
            // Claude TUI sessions sharing one cwd all collapse onto the single
            // project-newest transcript, thrashing bindings and losing relay
            // output. Caller passes the other sessions' bound paths.
            if exclude.contains(&path) {
                continue;
            }
            // Top-level `<uuid>.jsonl` only — Claude writes one transcript per
            // session id at the project-dir root.
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let is_uuid_stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| uuid::Uuid::parse_str(stem).is_ok());
            if !is_uuid_stem {
                continue;
            }
            let Some(modified) = entry.metadata().and_then(|meta| meta.modified()).ok() else {
                continue;
            };
            if modified < modified_since {
                continue;
            }
            // Canonical project-dir candidates can overlap (canonicalized vs
            // raw cwd encode to the same dir); de-dup identical paths so a
            // single physical transcript never inflates the candidate count and
            // trips the ambiguity guard.
            if !seen.insert(path.clone()) {
                continue;
            }
            found.push((modified, path));
        }
    }
    found.sort_by(|(a, _), (b, _)| b.cmp(a));
    found.into_iter().map(|(_, path)| path).collect()
}

// #3034: test-only — see `TranscriptReplayOutcome` note. Pins the JSONL replay
// parser contract; no production caller (streaming backend owns live replay).
#[allow(dead_code)]
pub fn replay_transcript_file(
    transcript_path: &Path,
    sender: &Sender<StreamMessage>,
) -> Result<TranscriptReplayOutcome, String> {
    let file = std::fs::File::open(transcript_path).map_err(|error| {
        format!(
            "read transcript {}: {error}",
            transcript_path.to_string_lossy()
        )
    })?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    let mut bytes_read = 0u64;
    let mut state = StreamLineState::new();
    let mut lines_read = 0usize;
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| format!("read transcript line: {error}"))?;
        if read == 0 {
            break;
        }
        bytes_read += read as u64;
        lines_read += 1;
        if !process_stream_line(&line, sender, &mut state) {
            break;
        }
    }
    Ok(TranscriptReplayOutcome {
        bytes_read,
        lines_read,
    })
}

pub(crate) fn claude_transcript_timestamp_at_or_after(
    transcript_path: &Path,
    turn_started_at: DateTime<Utc>,
) -> Result<Option<u64>, String> {
    let file = std::fs::File::open(transcript_path).map_err(|error| {
        format!(
            "read transcript {}: {error}",
            transcript_path.to_string_lossy()
        )
    })?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    let mut offset = 0u64;
    loop {
        line.clear();
        let line_start_offset = offset;
        let read = reader
            .read_line(&mut line)
            .map_err(|error| format!("read transcript line: {error}"))?;
        if read == 0 {
            return Ok(None);
        }
        offset = offset.saturating_add(read as u64);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            if !line.ends_with('\n') {
                return Ok(None);
            }
            continue;
        };
        if let Some(timestamp) = claude_transcript_line_timestamp(&json)
            && timestamp >= turn_started_at
        {
            return Ok(Some(line_start_offset));
        }
    }
}

fn claude_transcript_line_timestamp(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    let raw = value.get("timestamp")?.as_str()?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

/// Return the first and last parseable Claude JSONL timestamps. Continuation
/// discovery uses these semantic activity bounds instead of filesystem mtime
/// alone so a newer same-cwd transcript cannot steal another session's relay.
#[allow(clippy::type_complexity)]
pub(crate) fn claude_transcript_timestamp_bounds(
    transcript_path: &Path,
) -> Result<Option<(DateTime<Utc>, DateTime<Utc>)>, String> {
    let file = std::fs::File::open(transcript_path).map_err(|error| {
        format!(
            "read transcript {}: {error}",
            transcript_path.to_string_lossy()
        )
    })?;
    let reader = std::io::BufReader::new(file);
    let mut first = None;
    let mut last = None;
    for line in reader.lines() {
        let line = line.map_err(|error| format!("read transcript line: {error}"))?;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let Some(timestamp) = claude_transcript_line_timestamp(&value) else {
            continue;
        };
        first.get_or_insert(timestamp);
        last = Some(timestamp);
    }
    Ok(first.zip(last))
}

pub(crate) fn observe_transcript_turn_state(
    transcript_path: &Path,
) -> crate::services::tui_turn_state::TuiTurnState {
    crate::services::tui_turn_state::observe_claude_jsonl_turn_state(transcript_path)
}

pub(crate) fn encode_project_path(path: &Path) -> String {
    // Matches Claude Code's project-dir bucket shape observed under
    // ~/.claude/projects: every non-ASCII-alphanumeric path byte surface is
    // collapsed to '-'.
    path.to_string_lossy()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn default_claude_home() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::mpsc;

    #[test]
    fn encode_project_path_matches_claude_project_directory_shape() {
        let encoded = encode_project_path(Path::new(
            "/Users/itismyfield/.adk/release/workspaces/agentdesk",
        ));

        assert_eq!(
            encoded,
            "-Users-itismyfield--adk-release-workspaces-agentdesk"
        );
    }

    #[test]
    fn transcript_path_uses_canonical_cwd_and_uuid_filename() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let session_id = "01234567-89ab-cdef-0123-456789abcdef";

        let path = claude_transcript_path(dir.path(), session_id, Some(home.path())).unwrap();

        assert!(path.starts_with(home.path().join("projects")));
        assert_eq!(
            path.file_name().unwrap(),
            "01234567-89ab-cdef-0123-456789abcdef.jsonl"
        );
    }

    #[test]
    fn transcript_path_rejects_non_uuid_session_id() {
        let dir = tempfile::tempdir().unwrap();

        let error = claude_transcript_path(dir.path(), "not-a-uuid", None).unwrap_err();

        assert_eq!(error, "invalid Claude session_id UUID");
    }

    #[test]
    fn latest_claude_transcript_for_cwd_picks_newest_uuid_jsonl() {
        // #2843: idle relay must converge on the freshest transcript under the
        // project dir, ignoring non-UUID / non-jsonl siblings.
        let cwd = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let project_dir = claude_project_dir_for_cwd(cwd.path(), Some(home.path())).unwrap();
        std::fs::create_dir_all(&project_dir).unwrap();

        let older = project_dir.join("01234567-89ab-cdef-0123-456789abcdef.jsonl");
        let newer = project_dir.join("fedcba98-7654-3210-fedc-ba9876543210.jsonl");
        std::fs::write(&older, b"old").unwrap();
        std::fs::write(&newer, b"new").unwrap();
        // Must be ignored: non-UUID stem and non-jsonl extension.
        std::fs::write(project_dir.join("not-a-uuid.jsonl"), b"x").unwrap();
        std::fs::write(
            project_dir.join("01234567-89ab-cdef-0123-456789abcdef.txt"),
            b"x",
        )
        .unwrap();

        // Pin explicit mtimes so "newest" is deterministic.
        let base =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        std::fs::File::options()
            .write(true)
            .open(&older)
            .unwrap()
            .set_modified(base)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&newer)
            .unwrap()
            .set_modified(base + std::time::Duration::from_secs(60))
            .unwrap();

        let no_exclude = std::collections::HashSet::new();
        let latest = latest_claude_transcript_for_cwd(
            cwd.path(),
            std::time::SystemTime::UNIX_EPOCH,
            Some(home.path()),
            &no_exclude,
        )
        .unwrap();
        assert_eq!(latest, newer);

        // #2843: modified_since excludes transcripts older than the session
        // launch. With the cutoff between `older` and `newer`, only `newer`
        // qualifies; past both, nothing qualifies.
        let between = base + std::time::Duration::from_secs(30);
        assert_eq!(
            latest_claude_transcript_for_cwd(cwd.path(), between, Some(home.path()), &no_exclude),
            Some(newer.clone())
        );
        let after_all = base + std::time::Duration::from_secs(120);
        assert!(
            latest_claude_transcript_for_cwd(cwd.path(), after_all, Some(home.path()), &no_exclude)
                .is_none()
        );

        // #2843 multi-session fix: excluding the project-newest transcript
        // (claimed by another live session) must fall back to the next-newest
        // qualifying transcript instead of returning the claimed one.
        let exclude_newer: std::collections::HashSet<PathBuf> =
            [newer.clone()].into_iter().collect();
        assert_eq!(
            latest_claude_transcript_for_cwd(
                cwd.path(),
                std::time::SystemTime::UNIX_EPOCH,
                Some(home.path()),
                &exclude_newer,
            ),
            Some(older.clone())
        );
    }

    #[test]
    fn latest_claude_transcript_for_cwd_none_when_absent() {
        let cwd = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        // No project directory created at all.
        assert!(
            latest_claude_transcript_for_cwd(
                cwd.path(),
                std::time::SystemTime::UNIX_EPOCH,
                Some(home.path()),
                &std::collections::HashSet::new()
            )
            .is_none()
        );
    }

    #[test]
    fn replay_transcript_file_reuses_shared_stream_parser() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            concat!(
                r#"{"type":"system","subtype":"init","session_id":"sess-1"}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}"#,
                "\n",
                r#"{"type":"result","result":"done","session_id":"sess-1"}"#,
                "\n"
            ),
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();

        let outcome = replay_transcript_file(file.path(), &tx).unwrap();
        drop(tx);
        let messages: Vec<_> = rx.iter().collect();

        assert_eq!(outcome.lines_read, 3);
        assert!(matches!(messages[0], StreamMessage::Init { .. }));
        assert!(matches!(&messages[1], StreamMessage::Text { content } if content == "hello"));
        assert!(matches!(messages[2], StreamMessage::Done { .. }));
    }

    #[test]
    fn claude_transcript_timestamp_at_or_after_returns_first_matching_line_offset() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let first = r#"{"timestamp":"2026-05-28T00:00:00Z","type":"assistant","message":{"content":[{"type":"text","text":"first"}]}}"#;
        let second = r#"{"timestamp":"2026-05-28T00:00:01Z","type":"user","message":{"content":[{"type":"text","text":"second"}]}}"#;
        std::fs::write(file.path(), format!("{first}\n{second}\n")).unwrap();

        let offset = claude_transcript_timestamp_at_or_after(
            file.path(),
            Utc.with_ymd_and_hms(2026, 5, 28, 0, 0, 1).unwrap(),
        )
        .unwrap();

        assert_eq!(offset, Some(first.len() as u64 + 1));
    }

    #[test]
    fn claude_transcript_timestamp_at_or_after_is_inclusive() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let line = r#"{"timestamp":"2026-05-28T00:00:00.123Z","type":"assistant"}"#;
        std::fs::write(file.path(), format!("{line}\n")).unwrap();
        let turn_started_at = DateTime::parse_from_rfc3339("2026-05-28T00:00:00.123Z")
            .unwrap()
            .with_timezone(&Utc);

        let offset = claude_transcript_timestamp_at_or_after(file.path(), turn_started_at).unwrap();

        assert_eq!(offset, Some(0));
    }

    #[test]
    fn claude_transcript_timestamp_at_or_after_skips_unusable_lines() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let matching = r#"{"timestamp":"2026-05-28T00:00:05Z","type":"assistant"}"#;
        let transcript = format!(
            "{}\n{}\n{}\n",
            r#"{"type":"assistant"}"#, "not-json", matching
        );
        std::fs::write(file.path(), &transcript).unwrap();

        let offset = claude_transcript_timestamp_at_or_after(
            file.path(),
            Utc.with_ymd_and_hms(2026, 5, 28, 0, 0, 1).unwrap(),
        )
        .unwrap();

        let expected = r#"{"type":"assistant"}"#.len() + 1 + "not-json".len() + 1;
        assert_eq!(offset, Some(expected as u64));
    }

    #[test]
    fn claude_transcript_timestamp_bounds_ignore_unusable_lines() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            concat!(
                "not-json\n",
                "{\"type\":\"system\"}\n",
                "{\"timestamp\":\"2026-07-10T00:00:01Z\",\"type\":\"user\"}\n",
                "{\"timestamp\":\"bad\",\"type\":\"assistant\"}\n",
                "{\"timestamp\":\"2026-07-10T00:00:09Z\",\"type\":\"assistant\"}\n",
            ),
        )
        .unwrap();

        let (first, last) = claude_transcript_timestamp_bounds(file.path())
            .unwrap()
            .expect("timestamp bounds");
        assert_eq!(first.to_rfc3339(), "2026-07-10T00:00:01+00:00");
        assert_eq!(last.to_rfc3339(), "2026-07-10T00:00:09+00:00");
    }

    #[test]
    fn replay_transcript_file_tolerates_current_claude_tui_history_envelopes() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            concat!(
                r#"{"type":"last-prompt","sessionId":"sess-tui"}"#,
                "\n",
                r#"{"type":"attachment","attachment":{"type":"hook_success"},"sessionId":"sess-tui"}"#,
                "\n",
                r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Respond briefly."}]},"sessionId":"sess-tui"}"#,
                "\n",
                r#"{"type":"assistant","message":{"model":"claude-opus-4-7","content":[{"type":"text","text":"ADK_TUI_SMOKE_OK"}],"usage":{"input_tokens":6,"cache_creation_input_tokens":21795,"cache_read_input_tokens":17347,"output_tokens":20}},"sessionId":"sess-tui"}"#,
                "\n",
                r#"{"type":"system","subtype":"stop_hook_summary","sessionId":"sess-tui","hookCount":1,"hasOutput":true}"#,
                "\n",
                r#"{"type":"system","subtype":"turn_duration","durationMs":3606,"messageCount":8,"sessionId":"sess-tui"}"#,
                "\n",
            ),
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();

        let outcome = replay_transcript_file(file.path(), &tx).unwrap();
        drop(tx);
        let messages: Vec<_> = rx.iter().collect();

        assert_eq!(outcome.lines_read, 6);
        assert_eq!(messages.len(), 3);
        assert!(
            matches!(&messages[0], StreamMessage::Text { content } if content == "ADK_TUI_SMOKE_OK")
        );
        assert!(matches!(
            &messages[1],
            StreamMessage::Done { session_id, .. }
                if session_id.as_deref() == Some("sess-tui")
        ));
        assert!(matches!(
            &messages[2],
            StreamMessage::StatusUpdate {
                model,
                duration_ms: Some(3606),
                num_turns: Some(8),
                input_tokens: Some(6),
                cache_create_tokens: Some(21795),
                cache_read_tokens: Some(17347),
                output_tokens: Some(20),
                ..
            } if model.as_deref() == Some("claude-opus-4-7")
        ));
    }

    #[test]
    fn replay_transcript_file_ignores_unknown_envelopes_until_result() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            concat!(
                r#"{"type":"future-metadata","sessionId":"sess-tui","payload":{"field":1}}"#,
                "\n",
                r#"{"type":"system","subtype":"future_subtype","sessionId":"sess-tui"}"#,
                "\n",
                r#"{"type":"attachment","attachment":{"type":"future_preview"},"sessionId":"sess-tui"}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"still-running"}]},"sessionId":"sess-tui"}"#,
                "\n",
                r#"{"type":"result","result":"done","session_id":"sess-tui"}"#,
                "\n",
            ),
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();

        let outcome = replay_transcript_file(file.path(), &tx).unwrap();
        drop(tx);
        let messages: Vec<_> = rx.iter().collect();

        assert_eq!(outcome.lines_read, 5);
        assert_eq!(messages.len(), 2);
        assert!(
            matches!(&messages[0], StreamMessage::Text { content } if content == "still-running")
        );
        assert!(matches!(
            &messages[1],
            StreamMessage::Done { result, session_id }
                if result == "done" && session_id.as_deref() == Some("sess-tui")
        ));
    }
}
