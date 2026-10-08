use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use crate::services::agent_protocol::StreamMessage;
use crate::services::codex_tui::rollout_tail::tail_rollout_file_from_offset;
use crate::services::provider::{CancelToken, ProviderKind, ReadOutputResult};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

const LOGICAL: &str = "AgentDesk-codex-herdr-terminal";

fn record(value: serde_json::Value) -> String {
    format!("{value}\n")
}

fn event(kind: &str, turn_id: Option<&str>) -> String {
    record(json!({"type": "event_msg", "payload": {"type": kind, "turn_id": turn_id}}))
}

fn reply(text: &str) -> String {
    record(json!({
        "type": "response_item",
        "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]},
    }))
}

/// A rollout whose turn `t1` has streamed text and no terminal record yet.
fn running_turn() -> String {
    let meta = record(json!({"type": "session_meta", "payload": {"id": "herdr-session"}}));
    meta + &event("task_started", Some("t1")) + &reply("partial")
}

fn herdr_token(user_stop: bool) -> Arc<CancelToken> {
    let token = Arc::new(CancelToken::new());
    let owner = HostedOwner {
        provider: "codex".into(),
        discord_token_hash: "hash".into(),
        channel_id: "1".into(),
        logical_key: LOGICAL.into(),
        owner_node: "node".into(),
        runtime_root: "/tmp".into(),
    };
    let state = token.prepare_herdr_interrupt(ProviderKind::Codex, &owner);
    state.user_stop.store(user_stop, Ordering::SeqCst);
    token
}

fn append(path: &Path, bytes: &str) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes.as_bytes()).unwrap();
}

/// The production Herdr reader on its own thread, where the settlement override is set as given.
fn spawn_tail(
    path: PathBuf,
    token: Arc<CancelToken>,
    settlement: bool,
    alive: Arc<AtomicBool>,
    sender: mpsc::Sender<StreamMessage>,
) -> JoinHandle<Result<ReadOutputResult, String>> {
    std::thread::spawn(move || {
        use crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE;
        HERDR_SETTLEMENT_OVERRIDE.set(settlement);
        let alive = move || alive.load(Ordering::SeqCst);
        tail_rollout_file_from_offset(&path, 0, Some("herdr-session"), sender, Some(token), alive)
    })
}

struct Tail {
    path: PathBuf,
    alive: Arc<AtomicBool>,
    frames: Receiver<StreamMessage>,
    reader: JoinHandle<Result<ReadOutputResult, String>>,
}

impl Tail {
    fn start(dir: &Path, name: &str, token: &Arc<CancelToken>, settlement: bool) -> Self {
        let path = dir.join(name);
        std::fs::write(&path, running_turn()).unwrap();
        let alive = Arc::new(AtomicBool::new(true));
        let (tx, frames) = mpsc::channel();
        let reader = spawn_tail(path.clone(), token.clone(), settlement, alive.clone(), tx);
        Self {
            path,
            alive,
            frames,
            reader,
        }
    }

    /// Waits up to five seconds for the read to end, then reports the pane dead and collects it.
    fn finish(self) -> (ReadOutputResult, Vec<StreamMessage>) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !self.reader.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.alive.store(false, Ordering::SeqCst);
        let result = self.reader.join().unwrap().unwrap();
        (result, self.frames.try_iter().collect())
    }
}

fn terminal_frames(frames: &[StreamMessage]) -> Vec<String> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            StreamMessage::Done { result, .. } => Some(format!("done:{result}")),
            StreamMessage::CodexTuiTerminalDone { kind, result, .. } => {
                Some(format!("native:{kind:?}:{result}"))
            }
            _ => None,
        })
        .collect()
}

/// A Herdr Codex read ends only on its own turn's record: never at the drain, an abort typed at its
/// end, a completion after a stop plain; unnamed aborts and dead panes end nothing. Off: drain.
#[test]
fn a_herdr_codex_turn_ends_only_on_its_own_rollout_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let token = herdr_token(false);

    let aborted = Tail::start(dir.path(), "aborted.jsonl", &token, true);
    std::thread::sleep(Duration::from_millis(1_600));
    assert!(
        terminal_frames(&aborted.frames.try_iter().collect::<Vec<_>>()).is_empty(),
        "past the drain a Herdr turn without its terminal is not done"
    );
    assert!(!aborted.reader.is_finished(), "the reader keeps reading");
    let abort_end = running_turn().len() + event("turn_aborted", Some("t1")).len();
    let next_turn = event("task_started", Some("t2")) + &reply("next turn");
    append(
        &aborted.path,
        &(event("turn_aborted", Some("t1")) + &next_turn),
    );
    let (result, frames) = aborted.finish();
    let offset = abort_end as u64;
    assert_eq!(result, ReadOutputResult::Completed { offset });
    assert_eq!(terminal_frames(&frames), ["native:Aborted:partial"]);
    let Some(StreamMessage::CodexTuiTerminalDone {
        complete_record_end,
        tmux_session_name,
        turn_nonce,
        ..
    }) = frames.last()
    else {
        panic!("the abort is the last frame: {frames:?}");
    };
    assert_eq!(*complete_record_end, offset);
    assert_eq!(tmux_session_name, LOGICAL);
    assert_eq!(Some(turn_nonce.as_str()), token.turn_nonce());

    let stopped = herdr_token(true);
    let completed = Tail::start(dir.path(), "completed.jsonl", &stopped, true);
    append(&completed.path, &event("task_complete", Some("t1")));
    let (result, frames) = completed.finish();
    assert!(matches!(result, ReadOutputResult::Completed { .. }));
    assert_eq!(terminal_frames(&frames), ["done:partial"]);

    let unnamed = Tail::start(dir.path(), "unnamed.jsonl", &token, true);
    append(&unnamed.path, &event("turn_aborted", None));
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !unnamed.reader.is_finished(),
        "an unnamed abort ends nothing"
    );
    unnamed.alive.store(false, Ordering::SeqCst);
    let (result, frames) = unnamed.finish();
    assert!(matches!(result, ReadOutputResult::SessionDied { .. }));
    assert!(
        terminal_frames(&frames).is_empty(),
        "a dead pane is no terminal: {frames:?}"
    );

    let legacy = Tail::start(dir.path(), "legacy.jsonl", &token, false);
    let (result, frames) = legacy.finish();
    assert!(matches!(result, ReadOutputResult::Completed { .. }));
    assert_eq!(
        terminal_frames(&frames),
        ["done:partial"],
        "without settlement the drain still completes the turn"
    );
}

fn tool(kind: &str) -> String {
    record(
        json!({"type": "response_item", "payload": {"type": kind, "name": "exec", "call_id": "c1"}}),
    )
}

/// A Herdr completion waits for its open tool, and the next turn's records that one read already
/// pulled in stay unaccepted: the result offset and `final_offset` both end at the tool output.
#[test]
fn a_herdr_turn_accepts_nothing_past_its_terminal_record() {
    use crate::services::codex_tui::rollout_tail::{
        RolloutTailOptions, tail_rollout_file_until_assistant_response_with_pane_busy_probe as tail,
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("accepted.jsonl");
    let open_tool = tool("function_call") + &event("task_complete", Some("t1"));
    let accepted = running_turn() + &open_tool + &tool("function_call_output");
    let next_turn = event("task_started", Some("t2")) + &reply("next turn");
    let torn = &reply("next tail")[..24];
    std::fs::write(&path, accepted.clone() + &next_turn + torn).unwrap();
    let (tx, rx) = mpsc::channel();
    let token = Some(herdr_token(false));
    let options = RolloutTailOptions::default();
    let session = Some("herdr-session".to_owned());
    let (result, outcome) = tail(&path, 0, session, &tx, token, || true, options).unwrap();

    let end = accepted.len() as u64;
    assert_eq!(result, ReadOutputResult::Completed { offset: end });
    let counted = (outcome.final_offset, outcome.bytes_read, outcome.lines_read);
    assert_eq!(counted, (end, end, 6));
    let frames: Vec<_> = rx.try_iter().collect();
    assert_eq!(terminal_frames(&frames), ["done:partial"]);
    assert!(!format!("{frames:?}").contains("next t"), "{frames:?}");
}
