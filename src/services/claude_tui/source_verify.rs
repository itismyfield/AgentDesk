//! Decides whether a Claude hook names a transcript the pane may bind to.
//! Payload path, session id, opened-file identity and first record decide; mtime never does.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::services::claude_tui::hook_server::HookEventKind;
use crate::services::cluster::stream_relay::SourceFileIdentity;
use crate::services::tui_prompt_dedupe::binding_events::{HookSignal, SourceId};

/// Longest first line read to learn whose transcript a file is.
const FIRST_RECORD_LIMIT: u64 = 1 << 20;

/// Parent-session facts of one Claude hook payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClaudeHookSource {
    pub event: String,
    pub session_id: String,
    pub transcript_path: PathBuf,
    /// SessionStart `source`: startup, resume, clear, compact or fork.
    pub start_source: Option<String>,
}

impl ClaudeHookSource {
    /// Reads parent fields only, so a subagent's `agent_transcript_path` never names the pane source.
    #[cfg(test)]
    pub(crate) fn from_payload(payload: &Value) -> Option<Self> {
        let field = |key: &str| payload.get(key).and_then(Value::as_str);
        Some(Self {
            event: field("hook_event_name")?.to_string(),
            session_id: field("session_id")?.to_string(),
            transcript_path: PathBuf::from(field("transcript_path")?),
            start_source: field("source").map(str::to_string),
        })
    }

    /// `transcript_path` is the payload's path, already spelled under the pane's projects root.
    pub(crate) fn from_signal(
        session_id: &str,
        hook: &HookSignal,
        transcript_path: PathBuf,
    ) -> Self {
        Self {
            event: hook.event.clone(),
            session_id: session_id.to_owned(),
            transcript_path,
            start_source: hook.source.clone(),
        }
    }

    fn is_explicit_resume(&self) -> bool {
        HookEventKind::from_path(&self.event) == HookEventKind::SessionStart
            && self.start_source.as_deref() == Some("resume")
    }
}

/// A transcript the pane has bound. In a history the last entry is the current source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClaudeSource {
    pub session_id: String,
    pub path: PathBuf,
    /// None until a hook verified the file; the launch binding exists before it.
    pub file: Option<SourceFileIdentity>,
}

impl ClaudeSource {
    /// The log identity of a verified source; `None` without a Unix file identity.
    pub(crate) fn source_id(&self) -> Option<SourceId> {
        let (dev, ino) = match self.file? {
            #[cfg(unix)]
            SourceFileIdentity::Unix { dev, ino } => (dev, ino),
            SourceFileIdentity::Unavailable => return None,
        };
        Some(SourceId {
            session_id: self.session_id.clone(),
            path: self.path.clone(),
            dev,
            ino,
        })
    }
}

/// First line of an opened transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FirstRecord {
    /// Empty file, or its first line is still being written.
    NotWritten,
    Session(String),
    /// A complete first line that names no session.
    Unnamed,
}

/// What opening the payload's `transcript_path` found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OpenedTranscript {
    Missing,
    Opened {
        file: SourceFileIdentity,
        first: FirstRecord,
    },
}

/// Opens `path` once and reads its (dev, ino) and first record from that same descriptor.
pub(crate) fn observe_transcript(path: &Path) -> io::Result<OpenedTranscript> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(OpenedTranscript::Missing);
        }
        Err(error) => return Err(error),
    };
    let identity = SourceFileIdentity::from_open_file(&file);
    let mut line = Vec::new();
    BufReader::new(file.take(FIRST_RECORD_LIMIT)).read_until(b'\n', &mut line)?;
    let first = if line.last() != Some(&b'\n') {
        FirstRecord::NotWritten
    } else {
        serde_json::from_slice::<Value>(&line)
            .ok()
            .and_then(|record| record.get("sessionId")?.as_str().map(str::to_string))
            .map_or(FirstRecord::Unnamed, FirstRecord::Session)
    };
    Ok(OpenedTranscript::Opened {
        file: identity,
        first,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SourceVerdict {
    /// The hook is about the bound source; same path, (dev, ino) and first record.
    Current,
    /// The bound session's file is now verified; record this identity for it.
    Confirm(ClaudeSource),
    /// A newly verified source takes over from the bound one.
    Rotate(ClaudeSource),
    /// Path and id agree, but the file or its first record does not exist yet.
    Pending,
    /// A resume back to a session the pane left; arrival order cannot tell it from a late hook.
    PendingConflict,
    Rejected(SourceRejection),
    /// The bound source's file was replaced or removed; stop instead of rebinding.
    Anomaly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceRejection {
    InvalidSessionId,
    /// The path is not `<projects root>/<project>/<session_id>.jsonl`.
    NotTopLevelTranscript,
    /// The file's first record names another session, or none.
    FirstRecordMismatch,
    IdentityUnavailable,
    /// The session is one the pane already left.
    Regression,
}

/// Judges one hook against the pane's source history, oldest first and current last.
pub(crate) fn verify_claude_source(
    hook: &ClaudeHookSource,
    projects_root: &Path,
    opened: &OpenedTranscript,
    history: &[ClaudeSource],
) -> SourceVerdict {
    use SourceVerdict::{Anomaly, Confirm, Current, Pending, PendingConflict, Rejected, Rotate};
    if let Some(rejection) = precheck(hook, projects_root) {
        return Rejected(rejection);
    }
    let named = |source: &&ClaudeSource| source.session_id == hook.session_id;
    let current = history.last().filter(named);
    let left_before = history.iter().rev().skip(1).any(|source| named(&source));
    if current.is_none() && left_before {
        return if hook.is_explicit_resume() {
            PendingConflict
        } else {
            Rejected(SourceRejection::Regression)
        };
    }
    let pinned = current.and_then(|source| Some((source, source.file?)));
    let (file, first) = match opened {
        OpenedTranscript::Missing if pinned.is_some() => return Anomaly,
        OpenedTranscript::Missing => return Pending,
        OpenedTranscript::Opened { file, first } => (*file, first),
    };
    if let Some((bound, bound_file)) = pinned {
        // A verified file rewritten or truncated in place keeps its inode, so its first record is rechecked.
        let own_first = matches!(first, FirstRecord::Session(id) if *id == hook.session_id);
        let same = bound.path == hook.transcript_path && bound_file == file && own_first;
        return if same { Current } else { Anomaly };
    }
    match first {
        FirstRecord::NotWritten => return Pending,
        FirstRecord::Session(id) if *id == hook.session_id => {}
        FirstRecord::Session(_) | FirstRecord::Unnamed => {
            return Rejected(SourceRejection::FirstRecordMismatch);
        }
    }
    if matches!(file, SourceFileIdentity::Unavailable) {
        return Rejected(SourceRejection::IdentityUnavailable);
    }
    let source = ClaudeSource {
        session_id: hook.session_id.clone(),
        path: hook.transcript_path.clone(),
        file: Some(file),
    };
    if current.is_some() {
        Confirm(source)
    } else {
        Rotate(source)
    }
}

/// The checks that need no file: a valid session id and a top-level transcript path.
pub(crate) fn precheck(hook: &ClaudeHookSource, projects_root: &Path) -> Option<SourceRejection> {
    if uuid::Uuid::parse_str(&hook.session_id).is_err() {
        return Some(SourceRejection::InvalidSessionId);
    }
    if !is_top_level_transcript(projects_root, &hook.transcript_path, &hook.session_id) {
        return Some(SourceRejection::NotTopLevelTranscript);
    }
    None
}

/// Spells `payload` under `root` when only the spelling differs (a symlinked or `/private` root);
/// any other path is returned as is and fails the top-level check.
pub(crate) fn normalize_payload_path(root: &Path, payload: &Path) -> PathBuf {
    if payload.starts_with(root) {
        return payload.to_path_buf();
    }
    let parts = payload.parent().and_then(|project| {
        let same_root =
            std::fs::canonicalize(project.parent()?).ok()? == std::fs::canonicalize(root).ok()?;
        same_root.then(|| (project.file_name(), payload.file_name()))
    });
    match parts {
        Some((Some(project), Some(file))) => root.join(project).join(file),
        _ => payload.to_path_buf(),
    }
}

pub(crate) fn is_top_level_transcript(root: &Path, path: &Path, session_id: &str) -> bool {
    let file_name = format!("{session_id}.jsonl");
    let Ok(rest) = path.strip_prefix(root) else {
        return false;
    };
    let parts: Vec<Component<'_>> = rest.components().collect();
    path.is_absolute()
        && matches!(parts.as_slice(), [Component::Normal(_), Component::Normal(name)]
            if *name == OsStr::new(&file_name))
}

/// Byte offset where a fork's own rows start, and how many leading rows it inherited.
#[allow(dead_code)] // Dormant until the fork boundary path calls it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ForkBoundary {
    pub offset: u64,
    pub inherited_rows: usize,
}

/// Why a fork boundary cannot be named; the caller stops that source instead of guessing.
#[allow(dead_code)] // Dormant until the fork boundary path calls it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ForkBoundaryError {
    ParentUuidsUnknown,
    UnreadableRow,
    NoInheritedRows,
    MixedRows,
    EofBeforeNewRow,
}

/// Finds the byte after the leading rows whose `uuid` the parent already had.
/// Rows are matched by native uuid only; bodies are never compared.
#[allow(dead_code)] // Dormant until the fork boundary path calls it.
pub(crate) fn fork_start_boundary(
    parent_uuids: &HashSet<String>,
    fork_head: &[u8],
) -> Result<ForkBoundary, ForkBoundaryError> {
    if parent_uuids.is_empty() {
        return Err(ForkBoundaryError::ParentUuidsUnknown);
    }
    let (mut read, mut offset, mut inherited, mut seen_new) = (0u64, 0u64, 0usize, false);
    for line in fork_head.split_inclusive(|byte| *byte == b'\n') {
        if line.last() != Some(&b'\n') {
            break;
        }
        read += line.len() as u64;
        let record: Value =
            serde_json::from_slice(line).map_err(|_| ForkBoundaryError::UnreadableRow)?;
        let Some(uuid) = record.get("uuid").and_then(Value::as_str) else {
            continue;
        };
        match (parent_uuids.contains(uuid), seen_new) {
            (true, true) => return Err(ForkBoundaryError::MixedRows),
            (true, false) => (inherited, offset) = (inherited + 1, read),
            (false, _) if inherited == 0 => return Err(ForkBoundaryError::NoInheritedRows),
            (false, _) => seen_new = true,
        }
    }
    if !seen_new {
        return Err(ForkBoundaryError::EofBeforeNewRow);
    }
    Ok(ForkBoundary {
        offset,
        inherited_rows: inherited,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::{Duration, SystemTime};

    use serde_json::json;

    use super::*;

    const A: &str = "0a000000-0000-4000-8000-00000000000a";
    const B: &str = "0b000000-0000-4000-8000-00000000000b";
    const C: &str = "0c000000-0000-4000-8000-00000000000c";
    const D: &str = "0d000000-0000-4000-8000-00000000000d";

    fn transcript(root: &Path, session: &str) -> PathBuf {
        root.join("-work-tree").join(format!("{session}.jsonl"))
    }

    fn first_row(session: &str) -> String {
        format!("{}\n", json!({"type": "mode", "sessionId": session}))
    }

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn hook(event: &str, session: &str, path: &Path, source: Option<&str>) -> ClaudeHookSource {
        let payload = json!({"hook_event_name": event, "session_id": session,
            "transcript_path": path, "source": source});
        ClaudeHookSource::from_payload(&payload).unwrap()
    }

    /// Opens the payload's file and applies the verdict to the history as the binding would.
    fn feed(
        root: &Path,
        history: &mut Vec<ClaudeSource>,
        hook: &ClaudeHookSource,
    ) -> SourceVerdict {
        let opened = observe_transcript(&hook.transcript_path).unwrap();
        let verdict = verify_claude_source(hook, root, &opened, history);
        match &verdict {
            SourceVerdict::Confirm(source) => *history.last_mut().unwrap() = source.clone(),
            SourceVerdict::Rotate(source) => history.push(source.clone()),
            _ => {}
        }
        verdict
    }

    fn kind(verdict: &SourceVerdict) -> &'static str {
        match verdict {
            SourceVerdict::Current => "current",
            SourceVerdict::Confirm(_) => "confirm",
            SourceVerdict::Rotate(_) => "rotate",
            SourceVerdict::Pending => "pending",
            SourceVerdict::PendingConflict => "conflict",
            SourceVerdict::Rejected(_) => "rejected",
            SourceVerdict::Anomaly => "anomaly",
        }
    }

    fn bound_chain(root: &Path, sessions: &[&str]) -> Vec<ClaudeSource> {
        let mut history = Vec::new();
        for session in sessions {
            let path = transcript(root, session);
            write(&path, &first_row(session));
            let verdict = feed(
                root,
                &mut history,
                &hook("UserPromptSubmit", session, &path, None),
            );
            assert!(matches!(verdict, SourceVerdict::Rotate(_)), "{verdict:?}");
        }
        history
    }

    #[test]
    fn captured_hooks_bind_only_once_payload_and_opened_file_agree() {
        let fixture: Value = serde_json::from_str(
            &std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/hook_payload/claude-2.1.283.json"),
            )
            .unwrap(),
        )
        .unwrap();
        // The first complete line of each captured run's own transcript, as the CLI wrote it.
        let shapes = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/hook_payload/claude-2.1.283.transcript-first-records.jsonl");
        let first_records: std::collections::HashMap<String, String> =
            std::fs::read_to_string(shapes)
                .unwrap()
                .lines()
                .map(|line| {
                    let record = serde_json::from_str::<Value>(line).unwrap()["record"].clone();
                    let session = record["sessionId"].as_str().unwrap().to_owned();
                    (session, format!("{record}\n"))
                })
                .collect();
        let mut replayed = std::collections::HashSet::new();
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("projects");
        // Captured paths are <claude home>/projects/<project>/<file>; replay them under a temp home.
        let rebase = |captured: &Value| {
            let path = Path::new(captured.as_str().unwrap());
            home.path()
                .join(path.strip_prefix(path.ancestors().nth(3).unwrap()).unwrap())
        };
        let tui = "tui_startup_clear_compact_exit";
        let expected = [
            ("print_startup", "pending pending confirm current"),
            ("print_resume", "confirm current current current"),
            (
                tui,
                "pending pending confirm current pending rotate current current current current current current",
            ),
            ("print_fork", "pending pending rotate current"),
        ];
        for (run, want) in expected {
            let events = fixture["runs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["name"] == run);
            let events = events.unwrap()["events"].as_array().unwrap();
            let launch = events[0]["command_session_id"].as_str().unwrap();
            let launch_path = rebase(&events[0]["payload"]["transcript_path"]);
            let mut history = vec![ClaudeSource {
                session_id: launch.to_string(),
                path: launch_path.with_file_name(format!("{launch}.jsonl")),
                file: None,
            }];
            let mut got = Vec::new();
            let mut hooks = Vec::new();
            for event in events {
                let mut payload = event["payload"].clone();
                let path = rebase(&payload["transcript_path"]);
                let session = payload["session_id"].as_str().unwrap().to_string();
                if event["transcript_exists_at_hook"] == true && !path.exists() {
                    // Sessions whose transcript was not captured, like the TUI's /clear target, get a
                    // synthetic first line.
                    let captured = first_records.get(&session).inspect(|_| {
                        replayed.insert(session.clone());
                    });
                    write(&path, captured.map_or(&first_row(&session), |line| line));
                }
                payload["transcript_path"] = json!(path);
                let parsed = ClaudeHookSource::from_payload(&payload).unwrap();
                got.push(kind(&feed(&root, &mut history, &parsed)));
                hooks.push((event, parsed));
            }
            assert_eq!(got.join(" "), want, "{run}");
            if run != tui {
                continue;
            }
            // The pre-clear SessionEnd delivered late does not take the pane back.
            let (_, late) = hooks
                .iter()
                .find(|(event, _)| event["payload"]["reason"] == "clear")
                .unwrap();
            assert_eq!(
                feed(&root, &mut history, late),
                SourceVerdict::Rejected(SourceRejection::Regression)
            );
            // A subagent file carries the parent session id but never stands in for the parent.
            let (stop, parsed) = hooks
                .iter()
                .find(|(event, _)| event["event"] == "SubagentStop")
                .unwrap();
            let child = rebase(&stop["payload"]["agent_transcript_path"]);
            write(&child, &first_row(&parsed.session_id));
            let mut as_parent = parsed.clone();
            as_parent.transcript_path = child;
            assert_eq!(
                feed(&root, &mut history, &as_parent),
                SourceVerdict::Rejected(SourceRejection::NotTopLevelTranscript)
            );
            assert_eq!(history.last().unwrap().path, parsed.transcript_path);
        }
        assert_eq!(
            replayed.len(),
            first_records.len(),
            "every captured first line replayed"
        );
    }

    #[test]
    fn a_late_hook_from_a_left_session_never_rebinds_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut history = bound_chain(root, &[A, B, C]);
        let rejected = SourceVerdict::Rejected(SourceRejection::Regression);
        assert_eq!(
            feed(
                root,
                &mut history,
                &hook("Stop", B, &transcript(root, B), None)
            ),
            rejected
        );
        let end = hook("SessionEnd", A, &transcript(root, A), None);
        assert_eq!(feed(root, &mut history, &end), rejected);
        // An explicit resume back to B looks the same as a late B start, so it waits for more evidence.
        let resume = hook("SessionStart", B, &transcript(root, B), Some("resume"));
        assert_eq!(
            feed(root, &mut history, &resume),
            SourceVerdict::PendingConflict
        );
        assert_eq!(history.len(), 3);
        let stop = hook("Stop", C, &transcript(root, C), None);
        assert_eq!(feed(root, &mut history, &stop), SourceVerdict::Current);
    }

    #[test]
    fn snake_case_session_start_is_an_explicit_resume() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let history = bound_chain(root, &[A, B, C]);
        // The receiver hands hooks over under their snake_case event name.
        let signal = HookSignal::from_payload("session_start", &json!({"source": "resume"}));
        let resume = ClaudeHookSource::from_signal(B, &signal, transcript(root, B));
        let opened = observe_transcript(&resume.transcript_path).unwrap();
        assert_eq!(
            verify_claude_source(&resume, root, &opened, &history),
            SourceVerdict::PendingConflict
        );
    }

    #[test]
    fn a_newer_mtime_alone_never_binds_a_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut history = bound_chain(root, &[B, C]);
        let touch = |path: &Path| {
            let later = SystemTime::now() + Duration::from_secs(3600);
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(later)
                .unwrap();
        };
        touch(&transcript(root, B));
        let late = hook("Stop", B, &transcript(root, B), None);
        assert_eq!(
            feed(root, &mut history, &late),
            SourceVerdict::Rejected(SourceRejection::Regression)
        );
        // A fresh, newer file under the payload's name whose first record is another session's.
        let foreign = transcript(root, D);
        write(&foreign, &first_row(A));
        touch(&foreign);
        assert_eq!(
            feed(root, &mut history, &hook("Stop", D, &foreign, None)),
            SourceVerdict::Rejected(SourceRejection::FirstRecordMismatch)
        );
        // A newer file renamed over the bound path is a different file, not the bound source.
        let bound = transcript(root, C);
        let replacement = bound.with_extension("tmp");
        write(&replacement, &first_row(C));
        touch(&replacement);
        std::fs::rename(&replacement, &bound).unwrap();
        let stop = hook("Stop", C, &bound, None);
        assert_eq!(feed(root, &mut history, &stop), SourceVerdict::Anomaly);
        assert_eq!(
            history
                .iter()
                .map(|source| source.session_id.as_str())
                .collect::<Vec<_>>(),
            [B, C]
        );
    }

    #[test]
    fn a_bound_file_rewritten_in_place_is_an_anomaly() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut history = bound_chain(root, &[C]);
        let bound = transcript(root, C);
        let stop = hook("Stop", C, &bound, None);
        let pinned = history[0].file;
        // Another session's record, then an empty file, written over the same inode.
        for (body, first) in [
            (first_row(A), FirstRecord::Session(A.to_string())),
            (String::new(), FirstRecord::NotWritten),
        ] {
            std::fs::write(&bound, &body).unwrap();
            let opened = observe_transcript(&bound).unwrap();
            assert_eq!(
                opened,
                OpenedTranscript::Opened {
                    file: pinned.unwrap(),
                    first
                }
            );
            assert_eq!(feed(root, &mut history, &stop), SourceVerdict::Anomaly);
        }
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn a_session_start_before_its_transcript_is_written_waits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut history = bound_chain(root, &[A]);
        let path = transcript(root, B);
        let start = hook("SessionStart", B, &path, Some("clear"));
        assert_eq!(feed(root, &mut history, &start), SourceVerdict::Pending);
        // Claude has created the file but not finished its first record.
        let row = first_row(B);
        write(&path, row.trim_end());
        let submit = hook("UserPromptSubmit", B, &path, None);
        assert_eq!(feed(root, &mut history, &submit), SourceVerdict::Pending);
        write(&path, &row);
        assert!(
            matches!(feed(root, &mut history, &submit), SourceVerdict::Rotate(source) if source.session_id == B)
        );
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn fork_boundary_follows_inherited_row_uuids_not_bodies() {
        let parent_rows: Vec<Value> = (0..8)
            .map(|index| {
                json!({"type": "assistant", "uuid": format!("inherited-{index}"), "sessionId": A,
                    "message": {"id": "msg_parent", "content": format!("block {index}")}, "apiBlockIndex": index})
            })
            .collect();
        let parent: HashSet<String> = parent_rows
            .iter()
            .map(|row| row["uuid"].as_str().unwrap().to_string())
            .collect();
        let line = |row: &Value, uuid: Option<&str>| {
            let mut row = row.clone();
            row["sessionId"] = json!(B);
            if let Some(uuid) = uuid {
                row["uuid"] = json!(uuid);
            }
            format!("{row}\n")
        };
        let mut head = line(&json!({"type": "mode"}), None);
        for row in &parent_rows {
            head += &line(row, None);
        }
        // The new row repeats the last inherited body under a fresh uuid; only the uuid decides.
        let fresh = line(&parent_rows[7], Some("new-0"));
        let fork = format!(
            "{head}{fresh}{}",
            line(&json!({"type": "last-prompt"}), None)
        );
        let boundary = ForkBoundary {
            offset: head.len() as u64,
            inherited_rows: 8,
        };
        assert_eq!(fork_start_boundary(&parent, fork.as_bytes()), Ok(boundary));

        let fail = |bytes: &str| fork_start_boundary(&parent, bytes.as_bytes()).unwrap_err();
        assert_eq!(
            fork_start_boundary(&HashSet::new(), fork.as_bytes()),
            Err(ForkBoundaryError::ParentUuidsUnknown)
        );
        assert_eq!(fail(&head), ForkBoundaryError::EofBeforeNewRow);
        assert_eq!(
            fail(&format!("{head}{}", fresh.trim_end())),
            ForkBoundaryError::EofBeforeNewRow
        );
        assert_eq!(
            fail(&format!("{fork}{}", line(&parent_rows[0], None))),
            ForkBoundaryError::MixedRows
        );
        assert_eq!(
            fail(&format!("{fresh}{head}")),
            ForkBoundaryError::NoInheritedRows
        );
    }
}
