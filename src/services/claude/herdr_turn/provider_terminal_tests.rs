use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

const ASSISTANT: &str =
    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"partial"}]}}"#;
const INTERRUPT: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#;
const RESULT: &str = r#"{"type":"result","subtype":"success","result":"answer"}"#;
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
) -> Vec<StreamMessage> {
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
    reader.join().unwrap().unwrap();
    rx.try_iter().collect()
}

fn terminals(frames: &[StreamMessage]) -> Vec<String> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            StreamMessage::Done { result, .. } => Some(format!("done:{result}")),
            StreamMessage::ClaudeTuiTerminalDone { kind, .. } => Some(format!("native:{kind:?}")),
            StreamMessage::Error { message, .. } => Some(format!("error:{message}")),
            _ => None,
        })
        .collect()
}

/// A Herdr Claude read ends only on its turn-end record: the interrupt as a typed abort at its end,
/// a result or turn_duration as a plain Done; a dead pane without one sends no Done or error.
#[test]
fn a_herdr_claude_turn_ends_only_on_its_own_transcript_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let token = herdr_token();
    let path = dir.path().join("interrupted.jsonl");
    let next = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"next turn"}]}}"#;
    let frames = read(
        &path,
        &[ASSISTANT, INTERRUPT, next],
        &token,
        Duration::from_secs(5),
    );
    assert_eq!(terminals(&frames), ["native:Aborted"]);
    let Some(StreamMessage::ClaudeTuiTerminalDone {
        complete_record_end,
        actor,
        turn_nonce,
        ..
    }) = frames.last()
    else {
        panic!("the abort ends the read: {frames:?}");
    };
    assert_eq!(
        *complete_record_end,
        (ASSISTANT.len() + INTERRUPT.len() + 2) as u64
    );
    assert!(Arc::ptr_eq(&actor.upgrade().unwrap(), &token));
    assert_eq!(Some(turn_nonce.as_str()), token.turn_nonce());

    let path = dir.path().join("result.jsonl");
    let frames = read(&path, &[ASSISTANT, RESULT], &token, Duration::from_secs(5));
    assert_eq!(terminals(&frames), ["done:answer"]);
    let path = dir.path().join("duration.jsonl");
    let frames = read(
        &path,
        &[ASSISTANT, TURN_END],
        &token,
        Duration::from_secs(5),
    );
    assert_eq!(terminals(&frames), ["done:"]);

    let path = dir.path().join("running.jsonl");
    let frames = read(&path, &[ASSISTANT], &token, Duration::from_millis(300));
    assert!(
        terminals(&frames).is_empty(),
        "a dead pane is no terminal: {frames:?}"
    );
}
