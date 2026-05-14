use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use crate::services::agent_protocol::StreamMessage;
use crate::services::session_backend::{StreamLineState, process_stream_line};

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
}
