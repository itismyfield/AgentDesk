use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::claude_tui::hook_server;
use crate::services::tui_prompt_dedupe::{self, TuiRuntimeBinding};
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::time::Duration;

struct Rig {
    _dir: tempfile::TempDir,
    session: String,
    path: PathBuf,
    root: PathBuf,
    ctx: BindingContext,
}
impl Rig {
    fn new() -> Self {
        initialize();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        let native = uuid::Uuid::new_v4().to_string();
        let session = format!("readiness-{}", uuid::Uuid::new_v4().simple());
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let path = project.join(format!("{native}.jsonl"));
        let ctx: BindingContext = serde_json::from_value(json!({
            "schema":1,"provider":"claude","created_at":Utc::now(),"execution_nonce":nonce,
            "tmux_session":session,"channel_id":null,"owner_runtime_root":"fixture",
            "host":"readiness-test","expected_native_session_id":native,"launch_mode":"fresh",
            "provider_root":root
        }))
        .unwrap();
        crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(
            ctx.clone(),
        )
        .unwrap();
        let marker = crate::services::tmux_common::session_temp_path(&session, "spawn_nonce");
        fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
        fs::write(marker, &ctx.execution_nonce).unwrap();
        tui_prompt_dedupe::register_tmux_runtime_binding(
            &session,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(native),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        let rig = Self {
            _dir: dir,
            session,
            path,
            root,
            ctx,
        };
        rig.hook("startup");
        rig
    }
    fn append(&self, mut value: Value) {
        if value["sessionId"].is_null() {
            value["sessionId"] = json!(self.ctx.expected_native_session_id);
        }
        writeln!(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .unwrap(),
            "{value}"
        )
        .unwrap();
    }
    fn assistant(&self, text: &str) {
        self.append(json!({"type":"assistant","timestamp":Utc::now(),
            "message":{"role":"assistant","content":[{"type":"text","text":text}]}}));
    }
    fn terminal(&self) {
        self.append(json!({"type":"system","subtype":"turn_duration"}));
    }
    fn hook(&self, source: &str) {
        note_hook(
            "claude",
            &HookEventKind::SessionStart,
            &self.session,
            &json!({"session_id":self.ctx.expected_native_session_id,"transcript_path":self.path,"source":source}),
            Some(&self.ctx),
            Some(Utc::now()),
            false,
        );
    }
    fn ready(&self) -> bool {
        observe(&self.session, Some(&self.path)).is_some()
    }
    fn send(&self) -> Result<(), super::super::guarded_outcome::SubmissionError> {
        super::super::input::send_warm_followup_owned(&self.session, "one original", None)
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        tui_prompt_dedupe::clear_tmux_runtime_binding(&self.session);
    }
}

#[test]
fn strict_terminal_rejects_running_soft_partial_unknown_and_moved_source() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    r.assistant("still streaming");
    assert!(!r.ready());
    r.append(json!({"type":"system","subtype":"stop_hook_summary"}));
    assert!(!r.ready());
    r.terminal();
    assert!(r.ready());
    r.append(json!({"type":"future-terminal"}));
    assert!(!r.ready());
    r.terminal();
    let proof = observe(&r.session, None).unwrap();
    r.assistant("late assistant");
    assert!(reserve(proof).is_none());
    r.terminal();
    write!(
        OpenOptions::new().append(true).open(&r.path).unwrap(),
        "{{\"type\":"
    )
    .unwrap();
    assert!(!r.ready());
    fs::write(
        crate::services::tmux_common::session_temp_path(&r.session, "spawn_nonce"),
        "new-execution",
    )
    .unwrap();
    assert!(!r.ready());
}

#[test]
fn startup_and_clear_need_current_session_start_and_reserve_only_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for source in ["startup", "clear", "resume"] {
        let r = Rig::new();
        ledger()
            .entries
            .retain(|source, _| source.session != r.session);
        assert!(!r.ready());
        r.hook("compact");
        assert!(!r.ready());
        let mut old = r.ctx.clone();
        old.execution_nonce = "0".repeat(32);
        note_hook(
            "claude",
            &HookEventKind::SessionStart,
            &r.session,
            &json!({"session_id":r.ctx.expected_native_session_id,"transcript_path":r.path,"source":source}),
            Some(&old),
            Some(Utc::now()),
            false,
        );
        assert!(!r.ready());
        r.hook(source);
        assert!(r.ready());
        let a = observe(&r.session, None).unwrap();
        let stale = observe(&r.session, None).unwrap();
        reserve(a).unwrap().sent();
        assert!(reserve(stale).is_none());
        assert!(!r.ready());
        r.assistant("done");
        r.terminal();
        assert!(r.ready());
        reserve(observe(&r.session, None).unwrap())
            .unwrap()
            .not_sent();
        assert!(r.ready());
    }
}

#[cfg(unix)]
#[test]
fn narrow_and_resized_composers_send_without_a_screen_ready_marker() {
    use super::super::input::submission::tests::ScriptedTmux;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    r.assistant("finished");
    r.terminal();
    let fixture = |width| {
        format!(
            "⚠ 1 MCP server(s) need authentication\n run /mcp\n\n{}\n❯\u{a0}\n{}\n  ⏵⏵ bypass permissions on\n  (shift+tab to cycle)\n",
            "─".repeat(width),
            "─".repeat(width)
        )
    };
    let narrow = fixture(46);
    assert!(narrow.lines().all(|line| line.chars().count() <= 46));
    assert!(!crate::services::claude_tui::input::final_prompt_ready(
        &crate::services::claude_tui::input::prompt_readiness_snapshot_from_capture(
            Some(&narrow),
            true
        )
    ));
    let tmux = ScriptedTmux::new(&narrow);
    tmux.set_dead(false);
    assert_eq!(
        crate::services::claude_tui::input::send_followup_prompt(&r.session, "one original", None),
        Ok(())
    );
    assert_eq!(tmux.sends(), ["load", "paste", "Enter"]);
    for width in [120, 46, 80] {
        r.assistant("next turn");
        r.terminal();
        tmux.show(&fixture(width));
        assert_eq!(
            crate::services::claude_tui::input::send_followup_prompt(
                &r.session,
                "one original",
                None
            ),
            Ok(())
        );
    }
    assert_eq!(
        tmux.sends(),
        [
            "load", "paste", "Enter", "load", "paste", "Enter", "load", "paste", "Enter", "load",
            "paste", "Enter"
        ]
    );
}

#[cfg(unix)]
#[test]
fn person_draft_and_unread_capture_keep_keys_then_recover_without_a_switch() {
    use super::super::input::submission::tests::{ScriptedTmux, view};
    let _root = crate::config::TestRuntimeRootGuard::new();
    super::super::input::FOLLOWUP_TIMEOUT_FOR_TESTS.with(|budget| budget.set(Some(Duration::ZERO)));
    let r = Rig::new();
    r.assistant("finished");
    r.terminal();
    let tmux = ScriptedTmux::new(&view("a person's draft", false));
    tmux.set_dead(false);
    assert!(
        crate::services::claude_tui::input::send_followup_prompt(&r.session, "one original", None)
            .is_err()
    );
    assert!(tmux.sends().is_empty());
    tmux.show("transient redraw");
    assert!(
        crate::services::claude_tui::input::send_followup_prompt(&r.session, "one original", None)
            .is_err()
    );
    assert!(tmux.sends().is_empty());
    tmux.show(&format!(
        "Allow this tool?\nAllow  Deny\nEnter to confirm\n{}",
        view("", false)
    ));
    assert!(
        crate::services::claude_tui::input::send_followup_prompt(&r.session, "one original", None)
            .is_err()
    );
    assert!(tmux.sends().is_empty());
    tmux.show(&view("", false));
    assert_eq!(
        crate::services::claude_tui::input::send_followup_prompt(&r.session, "one original", None),
        Ok(())
    );
    assert_eq!(tmux.sends(), ["load", "paste", "Enter"]);
    super::super::input::FOLLOWUP_TIMEOUT_FOR_TESTS.with(|budget| budget.set(None));
}

#[cfg(unix)]
#[test]
fn output_guard_hold_and_blocked_fence_timer_and_late_assistant_before_final() {
    use super::super::input::submission::tests::{ScriptedTmux, view};
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    super::super::input::FOLLOWUP_TIMEOUT_FOR_TESTS.with(|budget| budget.set(Some(Duration::ZERO)));
    for text in [
        "answer [SYSTEM NOTIF",
        "done [SYSTEM NOTIFICATION - NOT USER INPUT] <task-id>private</task-id>",
    ] {
        let r = Rig::new();
        r.assistant(text);
        let tmux = ScriptedTmux::new(&view("", false));
        tmux.set_dead(false);
        let state = hook_server::HookServerState::new_with_claude_projects_root(r.root.clone());
        let response = runtime
            .block_on(
                hook_server::hook_receiver_router_with_state(state).oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/hooks/claude/Stop?session_id={}", r.session))
                        .header("content-type", "application/json")
                        .body(Body::from(json!({"transcript_path":r.path}).to_string()))
                        .unwrap(),
                ),
            )
            .unwrap();
        let bytes = runtime
            .block_on(to_bytes(response.into_body(), usize::MAX))
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap()["decision"],
            "block"
        );
        r.append(json!({"type":"system","subtype":"stop_hook_summary"}));
        r.terminal();
        assert!(
            prepare(
                &r.session,
                None,
                None,
                Duration::ZERO,
                super::super::input::PromptReadinessKind::ProvenWarmFollowup
            )
            .is_err()
        );
        assert!(!r.ready());
        assert!(r.send().is_err());
        assert!(tmux.sends().is_empty());
        r.assistant("late assistant");
        assert!(!r.ready());
        assert!(r.send().is_err());
        assert!(tmux.sends().is_empty());
        r.terminal();
        assert!(r.ready());
        assert_eq!(r.send(), Ok(()));
        assert_eq!(tmux.sends(), ["load", "paste", "Enter"]);
    }
    super::super::input::FOLLOWUP_TIMEOUT_FOR_TESTS.with(|budget| budget.set(None));
}

#[test]
fn subagent_stop_and_reentrant_stop_cannot_clear_root_continuation() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    r.assistant("needs continuation");
    let payload = json!({"transcript_path":r.path});
    note_hook(
        "claude",
        &HookEventKind::Stop,
        &r.session,
        &payload,
        None,
        None,
        true,
    );
    r.terminal();
    assert!(!r.ready());
    note_hook(
        "claude",
        &HookEventKind::SubagentStop,
        &r.session,
        &payload,
        None,
        None,
        false,
    );
    note_hook(
        "claude",
        &HookEventKind::Stop,
        &r.session,
        &json!({"transcript_path":r.path,"stop_hook_active":true}),
        Some(&r.ctx),
        Some(Utc::now()),
        false,
    );
    assert!(!r.ready());
    r.assistant("actual continuation");
    r.terminal();
    assert!(r.ready());
}

thread_local! {
    static SOURCES: std::cell::RefCell<Vec<tempfile::TempDir>> = const { std::cell::RefCell::new(Vec::new()) };
}
/// Existing submission fixtures exercise keys and custody over a completed source.
pub(crate) fn idle_fixture(session: &str) {
    if Source::current(session, None).is_some_and(|source| source.path.exists()) {
        return;
    }
    ledger()
        .entries
        .retain(|source, _| source.session != session);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fixture.jsonl");
    fs::write(
        &path,
        "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
    )
    .unwrap();
    ledger().entries.insert(
        Source {
            session: session.into(),
            nonce: source::nonce(session).flatten(),
            native: "fixture".into(),
            path,
        },
        Entry::default(),
    );
    SOURCES.with(|sources| sources.borrow_mut().push(dir));
}

#[test]
fn restored_history_needs_new_activity_and_summary_needs_a_matching_stop() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    r.assistant("old answer");
    r.terminal();
    ledger()
        .entries
        .retain(|source, _| source.session != r.session);
    assert!(
        !r.ready(),
        "restoring mappings cannot restore an old Idle proof"
    );
    r.assistant("new answer");
    r.terminal();
    assert!(r.ready());
    let q = Rig::new();
    q.assistant("final answer");
    q.append(json!({"type":"system","subtype":"stop_hook_summary"}));
    assert!(!q.ready());
    note_hook(
        "claude",
        &HookEventKind::Stop,
        &q.session,
        &json!({"transcript_path":q.path,"session_id":q.ctx.expected_native_session_id,"last_assistant_message":"final answer"}),
        Some(&q.ctx),
        Some(Utc::now()),
        false,
    );
    assert!(q.ready());
    reserve(observe(&q.session, None).unwrap()).unwrap().sent();
    q.append(json!({"type":"system","subtype":"stop_hook_summary"}));
    assert!(
        !q.ready(),
        "a duplicate old terminal cannot complete a newly reserved input"
    );
}

#[cfg(unix)]
#[test]
fn fresh_payload_waits_for_session_start_and_running_idle_path_sends_no_key() {
    use super::super::input::submission::tests::{ScriptedTmux, view};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    ledger()
        .entries
        .retain(|source, _| source.session != r.session);
    let tmux = ScriptedTmux::new(&view("", false));
    tmux.set_dead(false);
    super::super::input::FOLLOWUP_TIMEOUT_FOR_TESTS.with(|budget| budget.set(Some(Duration::ZERO)));
    assert!(r.send().is_err());
    assert!(tmux.sends().is_empty());
    r.hook("startup");
    assert_eq!(r.send(), Ok(()));
    assert_eq!(tmux.sends(), ["load", "paste", "Enter"]);
    r.assistant("still Running");
    assert!(r.send().is_err());
    assert_eq!(tmux.sends(), ["load", "paste", "Enter"]);
    r.terminal();
    assert_eq!(r.send(), Ok(()));
    assert_eq!(
        tmux.sends(),
        ["load", "paste", "Enter", "load", "paste", "Enter"]
    );
    super::super::input::FOLLOWUP_TIMEOUT_FOR_TESTS.with(|budget| budget.set(None));
}

pub(crate) fn pre_source(session: &str, path: &Path) {
    let source = Source::current(session, Some(path)).unwrap_or_else(|| Source {
        session: session.into(),
        nonce: source::nonce(session).flatten(),
        native: path.file_stem().unwrap().to_string_lossy().into_owned(),
        path: path.into(),
    });
    let mut state = ledger();
    state
        .entries
        .retain(|old, _| old.session != session || old == &source);
    state.entries.entry(source).or_insert_with(|| Entry {
        start: true,
        ..Entry::default()
    });
}

#[test]
fn duplicate_stop_invalidates_proof_without_stranding_an_unsent_reservation() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    r.assistant("done");
    r.terminal();
    let reservation = reserve(observe(&r.session, None).unwrap()).unwrap();
    note_hook(
        "claude",
        &HookEventKind::Stop,
        &r.session,
        &json!({"session_id":r.ctx.expected_native_session_id,"transcript_path":r.path,"last_assistant_message":"done"}),
        Some(&r.ctx),
        Some(Utc::now()),
        false,
    );
    assert!(!reservation.current());
    reservation.not_sent();
    assert!(r.ready());
}

#[test]
fn large_assistant_row_does_not_hide_the_reserved_turn_activity() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    r.assistant("first");
    r.terminal();
    reserve(observe(&r.session, None).unwrap()).unwrap().sent();
    r.append(json!({"type":"user","message":{"content":"one original"}}));
    r.assistant(&"x".repeat(512 * 1024));
    r.terminal();
    assert!(
        r.ready(),
        "a completed turn stays live when its assistant row exceeds the tail window"
    );
}

#[test]
fn clear_signal_fences_old_binding_and_old_start_until_new_native_binding_arrives() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    r.assistant("old completion");
    r.terminal();
    assert!(r.ready());
    let old = Utc::now();
    let native = uuid::Uuid::new_v4().to_string();
    let path = r.path.with_file_name(format!("{native}.jsonl"));
    let clear = json!({"source":"clear","session_id":native,"transcript_path":path});
    note_hook(
        "claude",
        &HookEventKind::SessionStart,
        &r.session,
        &clear,
        Some(&r.ctx),
        Some(Utc::now()),
        false,
    );
    assert!(!r.ready());
    assert!(observe(&r.session, Some(&path)).is_none());
    note_hook(
        "claude",
        &HookEventKind::SessionStart,
        &r.session,
        &json!({"source":"startup","session_id":r.ctx.expected_native_session_id,"transcript_path":r.path}),
        Some(&r.ctx),
        Some(old),
        false,
    );
    assert!(!r.ready(), "an older Start must not restore the old source");
    tui_prompt_dedupe::register_tmux_runtime_binding(
        &r.session,
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(native),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    assert!(observe(&r.session, Some(&path)).is_some());
}

#[test]
fn early_session_start_is_buffered_without_authorizing_the_unadopted_incarnation() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    {
        let mut state = ledger();
        state
            .entries
            .retain(|source, _| source.session != r.session);
        state.starts.retain(|(session, _), _| session != &r.session);
    }
    let marker = crate::services::tmux_common::session_temp_path(&r.session, "spawn_nonce");
    fs::remove_file(&marker).unwrap();
    r.hook("startup");
    assert!(!r.ready(), "a buffered Start is not adoption");
    fs::write(marker, &r.ctx.execution_nonce).unwrap();
    assert!(
        r.ready(),
        "adoption must consume the early Start without another hook"
    );
}

#[cfg(unix)]
#[test]
fn idle_mcp_chrome_is_not_focus_but_busy_and_real_dialog_keep_their_veto() {
    use super::super::input::submission::tests::view;
    let chrome = format!("⚠ 1 MCP server(s) need authentication\n{}", view("", false));
    assert!(!idle::focus_modal(&chrome));
    assert!(super::super::busy_inject::modal(&chrome));
    for dialog in [
        "Allow this tool?\nAllow  Deny\nEnter to confirm",
        "Choose session\n❯ old session\nEsc to cancel",
    ] {
        let capture = format!("{dialog}\n{}", view("", false));
        assert!(idle::focus_modal(&capture));
        assert!(super::super::busy_inject::modal(&capture));
    }
}

#[test]
fn early_continuation_is_fenced_after_adoption_and_old_epoch_cannot_consume_start() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    let mut old = r.ctx.clone();
    old.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
    crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(old.clone())
        .unwrap();
    let payload = json!({"session_id":r.ctx.expected_native_session_id,"transcript_path":r.path});
    note_hook(
        "claude",
        &HookEventKind::Stop,
        &r.session,
        &payload,
        Some(&old),
        Some(Utc::now()),
        true,
    );
    assert!(
        r.ready(),
        "an old epoch's deny-only fence cannot consume the current Start"
    );
    r.assistant("rewrite needed");
    let marker = crate::services::tmux_common::session_temp_path(&r.session, "spawn_nonce");
    fs::remove_file(&marker).unwrap();
    note_hook(
        "claude",
        &HookEventKind::Stop,
        &r.session,
        &payload,
        Some(&r.ctx),
        Some(Utc::now()),
        true,
    );
    r.terminal();
    fs::write(marker, &r.ctx.execution_nonce).unwrap();
    assert!(
        !r.ready(),
        "adoption must not lose an early root continuation"
    );
    r.assistant("continued answer");
    r.terminal();
    assert!(r.ready());
}

#[test]
fn completing_a_pre_stop_partial_record_is_not_new_continuation_activity() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    let record = format!(
        "{}\n",
        json!({"type":"assistant","sessionId":r.ctx.expected_native_session_id,
        "timestamp":Utc::now(),"message":{"content":"old partial response"}})
    );
    let cut = record.len() - 4;
    fs::write(&r.path, &record[..cut]).unwrap();
    note_hook(
        "claude",
        &HookEventKind::Stop,
        &r.session,
        &json!({"session_id":r.ctx.expected_native_session_id,"transcript_path":r.path}),
        Some(&r.ctx),
        Some(Utc::now()),
        true,
    );
    OpenOptions::new()
        .append(true)
        .open(&r.path)
        .unwrap()
        .write_all(&record.as_bytes()[cut..])
        .unwrap();
    r.terminal();
    assert!(
        !r.ready(),
        "completing the old record cannot discharge AwaitResume"
    );
    r.assistant("actual resumed activity");
    r.terminal();
    assert!(r.ready());
}

#[cfg(unix)]
#[test]
fn permission_mode_is_a_barrier_until_later_completion_not_a_permanent_veto() {
    use super::super::input::submission::tests::{ScriptedTmux, view};
    let _root = crate::config::TestRuntimeRootGuard::new();
    super::super::input::FOLLOWUP_TIMEOUT_FOR_TESTS.with(|b| b.set(Some(Duration::ZERO)));
    let r = Rig::new();
    let tmux = ScriptedTmux::new(&view("", false));
    tmux.set_dead(false);
    r.append(json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool-1","name":"Read","input":{}}]}}));
    r.append(json!({"type":"permission-mode","mode":"bypassPermissions"}));
    r.append(json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","content":"done"}]}}));
    r.assistant("actual completion");
    r.terminal();
    assert_eq!(
        r.send(),
        Ok(()),
        "a past permission marker must not hide a later terminal"
    );
    assert_eq!(tmux.sends(), ["load", "paste", "Enter"]);
    r.append(json!({"type":"permission-mode","mode":"default"}));
    assert!(r.send().is_err());
    r.assistant("still Running after marker");
    assert!(r.send().is_err());
    assert_eq!(
        tmux.sends(),
        ["load", "paste", "Enter"],
        "no later input while marker-last or Running"
    );
    super::super::input::FOLLOWUP_TIMEOUT_FOR_TESTS.with(|b| b.set(None));
}

#[cfg(unix)]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[cfg(unix)]
#[test]
fn late_startup_picker_enters_once_before_start_and_then_submits_one_original() {
    use super::super::input::{
        self, PromptReadinessKind,
        submission::tests::{ScriptedTmux, view},
    };
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    {
        let mut state = ledger();
        state
            .entries
            .retain(|source, _| source.session != r.session);
        state.starts.retain(|(session, _), _| session != &r.session);
    }
    let tmux = ScriptedTmux::new("Claude is booting…");
    tmux.set_dead(false);
    fs::write(
        tmux.directory().join("next_screen"),
        "Resume from summary\n❯ 1. Resume from summary\nEnter to confirm",
    )
    .unwrap();
    let socket = tmux.directory().join("dialog.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let script = format!(
        "if [ \"$1\" = Enter ] && [ ! -f {done} ]; then\n touch {done}\n python3 -c \"import socket; s=socket.socket(socket.AF_UNIX); s.settimeout(5); s.connect({socket}); s.sendall(b'D'); assert s.recv(1)==b'1'\"\nfi\n",
        done = shell_quote(&tmux.directory().join("dialog_done")),
        socket = shell_quote(&socket)
    );
    fs::write(tmux.directory().join("on_key"), script).unwrap();
    let screen = tmux.directory().join("screen");
    let sends = tmux.directory().join("sends");
    let session = r.session.clone();
    let path = r.path.clone();
    let ctx = r.ctx.clone();
    let controller = std::thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut tag = [0];
        peer.read_exact(&mut tag).unwrap();
        if tag == [b'C'] {
            return false;
        }
        assert_eq!(
            fs::read_to_string(&sends).unwrap(),
            "Enter\n",
            "dialog key precedes every payload"
        );
        assert!(
            observe(&session, Some(&path)).is_none(),
            "no Start evidence yet"
        );
        fs::write(screen, view("", false)).unwrap();
        peer.write_all(b"1").unwrap();
        std::thread::sleep(Duration::from_millis(800));
        assert_eq!(
            fs::read_to_string(&sends).unwrap(),
            "Enter\n",
            "empty screen alone must not admit payload before Start"
        );
        note_hook(
            "claude",
            &HookEventKind::SessionStart,
            &session,
            &json!({"session_id":ctx.expected_native_session_id,"transcript_path":path,"source":"startup"}),
            Some(&ctx),
            Some(Utc::now()),
            false,
        );
        true
    });
    let token = std::sync::Arc::new(crate::services::provider::CancelToken::new());
    let (done, wait) = std::sync::mpsc::channel();
    let timeout = token.clone();
    let timer = std::thread::spawn(move || {
        if wait.recv_timeout(Duration::from_secs(5)).is_err() {
            timeout
                .cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    });
    let result = input::send_prompt_with_readiness(
        &r.session,
        "one original",
        PromptReadinessKind::FreshTurn,
        Some(&token),
    );
    let _ = done.send(());
    if result.is_err() {
        if let Ok(mut peer) = UnixStream::connect(&socket) {
            let _ = peer.write_all(b"C");
        }
    }
    crate::test_support::wait::join_within(timer, Duration::from_secs(6));
    let handled = crate::test_support::wait::join_within(controller, Duration::from_secs(6));
    assert_eq!(result, Ok(()));
    assert!(handled);
    assert_eq!(tmux.sends(), ["Enter", "load", "paste", "Enter"]);
}

#[cfg(unix)]
#[test]
fn startup_workspace_caller_uses_home_only_plan_before_every_dialog_key() {
    use super::super::input::{
        self, PromptReadinessKind,
        submission::tests::{ScriptedTmux, view},
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let home = dirs::home_dir().unwrap().join("startup-fixture");
    for (workspace, accepted) in [
        ("/".to_owned(), false),
        ("/outside-home".to_owned(), false),
        (String::new(), false),
        (home.display().to_string(), true),
    ] {
        let r = Rig::new();
        let dialog = if workspace.is_empty() {
            "Quick safety check\nEnter to confirm".into()
        } else {
            format!("Accessing workspace:\n{workspace}\nQuick safety check\nEnter to confirm")
        };
        let tmux = ScriptedTmux::new(&dialog);
        tmux.set_dead(false);
        let empty = tmux.directory().join("empty");
        fs::write(&empty, view("", false)).unwrap();
        fs::write(
            tmux.directory().join("on_key"),
            format!(
                "if [ \"$1\" = Enter ]; then cp {} {}; fi\n",
                shell_quote(&empty),
                shell_quote(&tmux.directory().join("screen"))
            ),
        )
        .unwrap();
        let result = input::send_prompt_with_readiness(
            &r.session,
            "one original",
            PromptReadinessKind::FreshTurn,
            None,
        );
        if accepted {
            assert_eq!(result, Ok(()));
            assert_eq!(tmux.sends(), ["Enter", "load", "paste", "Enter"]);
        } else {
            assert!(
                result.is_err(),
                "unsafe workspace {workspace:?} must be refused"
            );
            assert!(
                tmux.sends().is_empty(),
                "unsafe workspace must get zero keys"
            );
        }
    }
}

#[cfg(unix)]
#[path = "compact_tests.rs"]
mod compact_tests;
