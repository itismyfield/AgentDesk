use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_input::human_frame::history;
use crate::services::tui_prompt_dedupe::{self, TuiRuntimeBinding};
use serde_json::json;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, Ordering};

const SID: &str = "busy-pin-session";
const ATTEMPT: &str = "busy-attempt-A";

struct Fixture {
    path: std::path::PathBuf,
    session: String,
    frame: String,
    source: serde_json::Value,
    token: Arc<CancelToken>,
    row: InflightTurnState,
    submitted: Arc<AtomicBool>,
    _dir: tempfile::TempDir,
    _root: crate::config::TestRuntimeRootGuard,
}

impl Fixture {
    fn new(channel: u64, registered_attempt: &str) -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("busy-native.jsonl");
        std::fs::write(&path, "").unwrap();
        let session = format!("AgentDesk-claude-busy-pin-{channel}");
        let marker = crate::services::tmux_common::session_temp_path(&session, "spawn_nonce");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, "busy-pin-incarnation").unwrap();
        Self::bind(&session, &path, SID);
        let frame = crate::services::claude_tui::busy_inject::frame(
            "discord",
            "7",
            "ann",
            ATTEMPT,
            "same body",
        );
        history::register(
            ("claude", &session, registered_attempt, "7"),
            &path,
            0,
            &frame,
        )
        .unwrap();
        let metadata = path.metadata().unwrap();
        let source = json!({"kind":"existing","session_id":SID,"transcript":path,
            "file":[metadata.dev(),metadata.ino()],"offset":0});
        let token = Arc::new(CancelToken::new());
        token.bind_claude_tmux_session(&session);
        let mut row = InflightTurnState::new(
            ProviderKind::Claude,
            channel,
            None,
            7,
            channel + 1,
            channel + 2,
            "same body".into(),
            Some(SID.into()),
            Some(session.clone()),
            Some(path.display().to_string()),
            None,
            0,
        );
        row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        row.turn_nonce = token.turn_nonce().map(str::to_owned);
        save_inflight_state_create_new(&row).unwrap();
        bind_native_stop_pin(&row, &token);
        Self {
            path,
            session,
            frame,
            source,
            token,
            row,
            submitted: Arc::new(AtomicBool::new(false)),
            _dir: dir,
            _root: root,
        }
    }

    fn bind(session: &str, path: &std::path::Path, sid: &str) {
        tui_prompt_dedupe::register_tmux_runtime_binding(
            session,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(sid.into()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
    }

    fn proof(&self) -> BusyNativeInput {
        BusyNativeInput::capture(
            &self.session,
            ATTEMPT,
            &self.frame,
            &self.path,
            &self.source,
            self.submitted.clone(),
            self.token.clone(),
        )
        .unwrap()
    }

    fn append(&self, record: serde_json::Value) -> u64 {
        let offset = self.path.metadata().unwrap().len();
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&self.path)
                .unwrap(),
            "{record}"
        )
        .unwrap();
        offset
    }

    fn user(&self, uuid: &str) -> u64 {
        self.append(json!({"type":"user","uuid":uuid,"sessionId":SID,
            "message":{"role":"user","content":self.frame}}))
    }

    fn stored(&self) -> InflightTurnState {
        load_inflight_state_read_only(&ProviderKind::Claude, self.row.channel_id).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        tui_prompt_dedupe::clear_tmux_runtime_binding(&self.session);
    }
}

#[test]
fn busy_native_input_pins_its_own_user_after_enqueue_without_replay() {
    let f = Fixture::new(780_710_101, ATTEMPT);
    let proof = f.proof();
    f.submitted.store(true, Ordering::Release);
    let enqueue = f.append(json!({"type":"queue-operation","operation":"enqueue",
        "uuid":"queue-A","sessionId":SID,"content":f.frame}));
    proof.adopt(enqueue);
    assert!(
        f.stored().native_stop_user_pin.is_none(),
        "enqueue is not native user proof"
    );
    assert!(f.token.native_stop.receipt_sink().observed().is_none());
    let offset = f.user("native-A");
    proof.adopt(offset);
    let row = f.stored();
    let input = &row.native_stop_user_pin.as_ref().unwrap().input;
    assert_eq!(input.uuid, "native-A");
    assert_eq!(input.range.0, offset);
    assert_eq!(
        input.source_file,
        (
            f.path.metadata().unwrap().dev(),
            f.path.metadata().unwrap().ino()
        )
    );
    assert!(historical_input_verified(&row));
    let bytes = std::fs::read(inflight_state_path(
        &inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        f.row.channel_id,
    ))
    .unwrap();
    proof.adopt(offset);
    assert_eq!(
        std::fs::read(inflight_state_path(
            &inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            f.row.channel_id
        ))
        .unwrap(),
        bytes
    );
}

#[test]
fn busy_native_input_requires_successfully_settled_write() {
    let f = Fixture::new(780_710_102, ATTEMPT);
    let proof = f.proof();
    let offset = f.user("native-A");
    proof.adopt(offset);
    assert!(f.stored().native_stop_user_pin.is_none());
    assert!(
        f.token.native_stop.receipt_sink().observed().is_none(),
        "uncertain or failed writes cannot adopt"
    );
}

#[test]
fn busy_native_input_rejects_another_attempt_with_identical_frame() {
    let f = Fixture::new(780_710_103, "another-attempt");
    let proof = f.proof();
    f.submitted.store(true, Ordering::Release);
    let offset = f.user("native-other");
    assert!(
        matches!(history::row_owner("claude", &f.session, std::slice::from_ref(&f.frame), "native-other"), history::RowOwner::User(nonce) if nonce == "another-attempt")
    );
    proof.adopt(offset);
    assert!(f.stored().native_stop_user_pin.is_none());
    assert!(f.token.native_stop.receipt_sink().observed().is_none());
}

#[test]
fn busy_native_input_rejects_source_and_session_changes() {
    for (index, case) in [
        "row_sid",
        "binding_sid",
        "inode",
        "incarnation",
        "offset",
        "source_path",
    ]
    .into_iter()
    .enumerate()
    {
        let f = Fixture::new(780_710_110 + index as u64, ATTEMPT);
        let mut proof = f.proof();
        f.submitted.store(true, Ordering::Release);
        let offset = match case {
            "row_sid" => f.append(
                json!({"type":"user","uuid":"native-A","sessionId":"another-session",
                "message":{"role":"user","content":f.frame}}),
            ),
            _ => f.user("native-A"),
        };
        match case {
            "binding_sid" => Fixture::bind(&f.session, &f.path, "another-session"),
            "inode" => {
                let next = f._dir.path().join("replacement.jsonl");
                std::fs::copy(&f.path, &next).unwrap();
                std::fs::rename(next, &f.path).unwrap();
            }
            "incarnation" => std::fs::write(
                crate::services::tmux_common::session_temp_path(&f.session, "spawn_nonce"),
                "next-incarnation",
            )
            .unwrap(),
            "offset" => proof.source_offset = offset + 1,
            "source_path" => proof.transcript = f._dir.path().join("other-source.jsonl"),
            _ => {}
        }
        proof.adopt(offset);
        assert!(f.stored().native_stop_user_pin.is_none(), "{case}");
        assert!(
            f.token.native_stop.receipt_sink().observed().is_none(),
            "{case}"
        );
    }
}

#[test]
fn busy_native_input_requires_a_complete_native_user_row_at_the_exact_offset() {
    for (index, case) in [
        "queue",
        "command",
        "meta",
        "sidechain",
        "summary",
        "torn",
        "middle",
    ]
    .into_iter()
    .enumerate()
    {
        let f = Fixture::new(780_710_120 + index as u64, ATTEMPT);
        let proof = f.proof();
        f.submitted.store(true, Ordering::Release);
        let mut user = json!({"type":"user","uuid":"native-A","sessionId":SID,
            "message":{"role":"user","content":f.frame}});
        match case {
            "queue" => {
                user = json!({"type":"queue-operation","operation":"enqueue","uuid":"native-A","sessionId":SID,"content":f.frame})
            }
            "command" => {
                user = json!({"type":"attachment","uuid":"native-A","sessionId":SID,"attachment":{"type":"queued_command","commandMode":"prompt","prompt":f.frame}})
            }
            "meta" => user["isMeta"] = true.into(),
            "sidechain" => user["isSidechain"] = true.into(),
            "summary" => user["isCompactSummary"] = true.into(),
            _ => {}
        }
        let mut offset = if case == "torn" {
            std::fs::write(&f.path, user.to_string()).unwrap();
            0
        } else {
            f.append(user)
        };
        if case == "middle" {
            offset += 1;
        }
        proof.adopt(offset);
        assert!(f.stored().native_stop_user_pin.is_none(), "{case}");
        assert!(
            f.token.native_stop.receipt_sink().observed().is_none(),
            "{case}"
        );
    }
}

#[test]
fn busy_native_input_never_changes_a_successor_birth_episode() {
    let f = Fixture::new(780_710_130, ATTEMPT);
    let proof = f.proof();
    f.submitted.store(true, Ordering::Release);
    let offset = f.user("native-A");
    let mut successor = f.row.clone();
    successor.user_msg_id += 100;
    successor.turn_nonce = Some("successor-B".into());
    save_inflight_state(&successor).unwrap();
    let path = inflight_state_path(
        &inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        successor.channel_id,
    );
    let before = std::fs::read(&path).unwrap();
    proof.adopt(offset);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(f.stored().native_stop_user_pin.is_none());
}

#[test]
fn busy_native_input_keeps_the_current_native_turn_row_and_token_untouched() {
    let f = Fixture::new(780_710_131, ATTEMPT);
    let proof = f.proof();
    f.submitted.store(true, Ordering::Release);
    let offset = f.user("native-A");
    let native = Arc::new(CancelToken::new());
    let mut n = f.row.clone();
    n.user_msg_id += 200;
    n.user_text = "native N".into();
    n.turn_nonce = native.turn_nonce().map(str::to_owned);
    save_inflight_state(&n).unwrap();
    bind_native_stop_pin(&n, &native);
    let path = inflight_state_path(
        &inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        n.channel_id,
    );
    let before = std::fs::read(&path).unwrap();
    proof.adopt(offset);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(!native.cancelled.load(Ordering::Acquire));
    assert!(native.native_stop.receipt_sink().observed().is_none());
}
