use std::path::{Path, PathBuf};
use std::sync::Mutex;

struct Hook {
    path: PathBuf,
    run: Box<dyn FnOnce() + Send>,
}

static HOOKS: Mutex<Vec<Hook>> = Mutex::new(Vec::new());

pub(crate) fn on_poll_once(path: &Path, hook: Box<dyn FnOnce() + Send>) {
    HOOKS.lock().unwrap_or_else(|e| e.into_inner()).push(Hook {
        path: path.to_owned(),
        run: hook,
    });
}

pub(crate) fn poll(path: &Path) {
    let hook = {
        let mut hooks = HOOKS.lock().unwrap_or_else(|e| e.into_inner());
        hooks
            .iter()
            .position(|hook| hook.path == path)
            .map(|at| hooks.remove(at))
    };
    if let Some(hook) = hook {
        (hook.run)();
    }
}

fn token(
    path: &Path,
    at: Option<chrono::DateTime<chrono::Utc>>,
) -> std::sync::Arc<super::CancelToken> {
    use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
    use crate::services::provider::cancel_token_claude_interrupt::HerdrTurnStart;
    let token = std::sync::Arc::new(super::CancelToken::new());
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: "hash".into(),
        channel_id: "1".into(),
        logical_key: "AgentDesk-claude-start-offset".into(),
        owner_node: "node".into(),
        runtime_root: path.parent().unwrap().display().to_string(),
    };
    let state = token.prepare_herdr_interrupt(super::super::ProviderKind::Claude, &owner);
    assert!(state.record_turn_start(HerdrTurnStart::at_end_of("nonce", path, at)));
    token
}

fn stamp() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-10-21T10:16:44.925000001Z")
        .unwrap()
        .with_timezone(&chrono::Utc)
}

#[test]
fn timestamp_eligibility_is_inclusive_and_rejects_metadata_or_earlier_stamps() {
    let at = stamp();
    for (line, expected) in [
        (r#"{"timestamp":"2026-10-21T10:16:44.925000001Z"}"#, true),
        (r#"{"timestamp":"2026-10-21T10:16:44.925Z"}"#, false),
        (r#"{"timestamp":"2026-10-21T10:16:44.925000002Z"}"#, true),
        (r#"{"timestamp":"2026-10-21T10:16:44.926Z"}"#, true),
        (r#"{"type":"file-history-snapshot"}"#, false),
        (r#"{"timestamp":"invalid"}"#, false),
        (r#"{"timestamp":"2026-10-21T10:16:44.925Z""#, false),
    ] {
        assert_eq!(super::eligible(line, &at), expected, "{line}");
    }
}

fn latch(path: &Path) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (observed_tx, observed_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    on_poll_once(
        path,
        Box::new(move || {
            observed_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }),
    );
    (observed_rx, release_tx)
}

fn observed(rx: std::sync::mpsc::Receiver<()>) {
    rx.recv_timeout(std::time::Duration::from_secs(5))
        .expect("the reader observed no eligible complete record");
}

#[test]
fn a_missing_file_and_metadata_or_whole_json_without_newline_commit_no_start() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cold.jsonl");
    let token = token(&path, Some(stamp()));
    let (missing, release_missing) = latch(&path);
    let reader = {
        let path = path.display().to_string();
        std::thread::spawn(move || super::wait(&path, 0, &token, &|_| None))
    };
    observed(missing);
    let before = (stamp() - chrono::Duration::nanoseconds(1)).to_rfc3339();
    let previous_prompt = serde_json::json!({"type": "user", "timestamp": before,
        "message": {"role": "user", "content": "previous"}});
    let previous_terminal = serde_json::json!({"type": "system", "subtype": "stop_hook_summary",
        "timestamp": before});
    let metadata = format!(
        "{previous_prompt}\r\n{previous_terminal}\n \r\n{{\"type\":\"file-history-snapshot\"}}\r\nnot-json\n\n"
    );
    let (metadata_seen, release_metadata) = latch(&path);
    std::fs::write(&path, &metadata).unwrap();
    release_missing.send(()).unwrap();
    observed(metadata_seen);
    assert!(
        !reader.is_finished(),
        "metadata does not commit fallback zero"
    );

    let prompt = serde_json::json!({"type": "user", "timestamp": stamp().to_rfc3339(),
        "message": {"role": "user", "content": "question"}})
    .to_string();
    let (unterminated_seen, release_unterminated) = latch(&path);
    let mut writer = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writer.write_all(prompt.as_bytes()).unwrap();
    writer.flush().unwrap();
    release_metadata.send(()).unwrap();
    observed(unterminated_seen);
    assert!(
        !reader.is_finished(),
        "a whole JSON tail still lacks its newline"
    );
    writer.write_all(b"\r\n").unwrap();
    writer.flush().unwrap();
    release_unterminated.send(()).unwrap();
    let start = reader.join().unwrap().unwrap().unwrap();
    assert_eq!(start, metadata.len() as u64);

    let assistant = serde_json::json!({"type": "assistant", "timestamp": stamp().to_rfc3339(),
        "message": {"content": [{"type": "text", "text": "answer"}]}});
    let terminal = serde_json::json!({"type": "system", "subtype": "stop_hook_summary",
        "timestamp": stamp().to_rfc3339()});
    writeln!(writer, "{assistant}\n{terminal}").unwrap();
    writer.flush().unwrap();
    let mut file = std::fs::File::open(&path).unwrap();
    let len = file.metadata().unwrap().len();
    assert_eq!(
        super::super::replay_herdr_turn(&mut file, stamp(), len),
        Some((start, super::super::NativeTerminalKind::Completed, len)),
        "replay and live selection use the same raw timestamp boundary"
    );
}

#[test]
fn an_ended_or_cancelled_wait_returns_no_start_and_unstamped_fixtures_keep_their_offset() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("metadata.jsonl");
    std::fs::write(&path, "{\"type\":\"metadata\"}\n").unwrap();
    let token = token(&path, Some(stamp()));
    assert_eq!(
        super::wait(&path.display().to_string(), 17, &token, &|at| {
            assert_eq!(at.0, std::fs::metadata(&path).unwrap().len());
            Some(String::new())
        })
        .unwrap(),
        None
    );
    token
        .cancelled
        .store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(
        super::wait(&path.display().to_string(), 17, &token, &|_| None).unwrap(),
        None
    );
    let legacy = self::token(&path, None);
    assert_eq!(
        super::wait(&path.display().to_string(), 17, &legacy, &|_| None).unwrap(),
        Some(17)
    );

    let rotating = dir.path().join("rotating.jsonl");
    let replacement = dir.path().join("replacement.jsonl");
    std::fs::write(&rotating, "{\"type\":\"metadata\"}\n").unwrap();
    std::fs::write(&replacement, "{\"type\":\"replacement\"}\n").unwrap();
    let actor = self::token(&rotating, Some(stamp()));
    let target = rotating.clone();
    on_poll_once(
        &rotating,
        Box::new(move || std::fs::rename(replacement, target).unwrap()),
    );
    let error = super::wait(&rotating.display().to_string(), 0, &actor, &|_| None).unwrap_err();
    assert!(
        error.contains("rotated"),
        "a replacement keeps no start: {error}"
    );
}
