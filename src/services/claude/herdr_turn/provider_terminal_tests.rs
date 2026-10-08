use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

const ASSISTANT: &str =
    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"partial"}]}}"#;
const INTERRUPT: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#;
const RESULT: &str = r#"{"type":"result","subtype":"success","result":"answer"}"#;
const PROMPT: &str = r#"{"type":"user","message":{"role":"user","content":"question"}}"#;
const TURN_END: &str = r#"{"type":"system","subtype":"turn_duration","durationMs":10}"#;

fn herdr_token() -> Arc<CancelToken> {
    let token = Arc::new(CancelToken::new());
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: "hash".into(),
        channel_id: "1".into(),
        logical_key: "AgentDesk-claude-herdr-terminal".into(),
        owner_node: "node".into(),
        runtime_root: "/tmp".into(),
    };
    token.prepare_herdr_interrupt(ProviderKind::Claude, &owner);
    token
}

/// Reads `lines` on a reader thread; the pane is reported dead once `alive_for` passes.
fn read(
    path: &Path,
    lines: &[&str],
    token: &Arc<CancelToken>,
    alive_for: Duration,
) -> (Vec<StreamMessage>, Option<ClaudeTurnTerminal>) {
    let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(path, body).unwrap();
    let alive = Arc::new(AtomicBool::new(true));
    let (tx, rx) = mpsc::channel();
    let reader = {
        let (path, token, alive) = (path.display().to_string(), token.clone(), alive.clone());
        std::thread::spawn(move || {
            let alive = move || alive.load(Ordering::SeqCst);
            read_to_provider_terminal(&path, 0, &tx, &token, alive)
        })
    };
    let deadline = Instant::now() + alive_for;
    while !reader.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    alive.store(false, Ordering::SeqCst);
    let terminal = reader.join().unwrap().unwrap();
    (rx.try_iter().collect(), terminal)
}

fn record_end(lines: &[&str]) -> u64 {
    lines.iter().map(|line| line.len() as u64 + 1).sum()
}

/// A Herdr Claude read returns its own turn's interrupt as an abort and its result or turn_duration
/// as a completion, offsets never past it; an earlier turn's tail or a dead pane ends nothing.
#[test]
fn a_herdr_claude_turn_ends_only_on_its_own_transcript_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let token = herdr_token();
    let path = dir.path().join("interrupted.jsonl");
    let next = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"next turn"}]}}"#;
    let turn = [PROMPT, ASSISTANT, INTERRUPT];
    let lines = [&turn[..], &[next]].concat();
    let (frames, terminal) = read(&path, &lines, &token, Duration::from_secs(5));
    let terminal = terminal.expect("the interrupt ends the turn");
    assert_eq!(terminal.kind, NativeTerminalKind::Aborted);
    assert_eq!(terminal.end, record_end(&turn));
    let meta = std::fs::metadata(&path).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(terminal.file, (meta.dev(), meta.ino()));
    for frame in &frames {
        match frame {
            StreamMessage::OutputOffset { offset } => assert!(*offset <= terminal.end, "{frame:?}"),
            StreamMessage::Done { .. }
            | StreamMessage::ClaudeTuiTerminalDone { .. }
            | StreamMessage::Error { .. } => panic!("the read sends no terminal: {frame:?}"),
            _ => {}
        }
    }

    for (name, last, result) in [("result", RESULT, "answer"), ("duration", TURN_END, "")] {
        let path = dir.path().join(format!("{name}.jsonl"));
        let terminal = read(
            &path,
            &[PROMPT, ASSISTANT, last],
            &token,
            Duration::from_secs(5),
        )
        .1;
        let terminal = terminal.expect("a completion ends the turn");
        assert_eq!(terminal.kind, NativeTerminalKind::Completed, "{name}");
        assert_eq!(terminal.result, result, "{name}");
    }

    let path = dir.path().join("previous.jsonl");
    let previous = [ASSISTANT, INTERRUPT, RESULT, TURN_END, PROMPT, ASSISTANT];
    let (_, terminal) = read(&path, &previous, &token, Duration::from_millis(400));
    assert!(terminal.is_none(), "an earlier turn's tail ends nothing");
    let path = dir.path().join("running.jsonl");
    let (frames, terminal) = read(
        &path,
        &[PROMPT, ASSISTANT],
        &token,
        Duration::from_millis(300),
    );
    assert!(terminal.is_none(), "a dead pane is no terminal: {frames:?}");
}
