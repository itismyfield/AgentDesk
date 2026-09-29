use std::fs;
use std::path::PathBuf;
use std::sync::MutexGuard;

use axum::Router;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::claude_tui::hook_registry::{self, RegistryKey};
use crate::services::claude_tui::hook_server::adoption_retry::{
    deferred_adoption_count, reset_deferred_adoptions_for_tests,
};
use crate::services::claude_tui::hook_server::relay_receipts::{
    RELAY_DEADLINE_HEADER, RELAY_PUBLISHED_AT_HEADER, RELAY_REQUEST_ID_HEADER,
};
use crate::services::claude_tui::hook_server::{
    HookEvent, HookServerState, hook_receiver_router_with_state, retry_deferred_claude_adoptions,
};
use crate::services::tui_prompt_dedupe::binding_events::{
    APPEND_FAULT, BindingEvent, BindingTarget, binding_events_since, set_test_root,
};
use crate::services::tui_prompt_dedupe::{
    TEST_LOCK, TuiRuntimeBinding, clear_claude_session_rotation,
    lock_claude_session_rotations_for_tests, register_provider_session,
    register_rehydrated_tmux_runtime_binding, register_tmux_channel, register_tmux_runtime_binding,
    reset_state_for_tests, runtime_binding_for_tmux_session,
};

/// One receiver over a scratch binding log, with the dedupe state held for the test.
struct Ingress {
    _root: tempfile::TempDir,
    dir: tempfile::TempDir,
    state: HookServerState,
    app: Router,
    runtime: tokio::runtime::Runtime,
    _rotations: MutexGuard<'static, ()>,
    _state: MutexGuard<'static, ()>,
}

impl Ingress {
    fn new() -> Self {
        let state_lock = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let rotations = lock_claude_session_rotations_for_tests();
        reset_state_for_tests();
        reset_deferred_adoptions_for_tests();
        set_discovery_pending_for_tests(false);
        let root = tempfile::tempdir().unwrap();
        set_test_root(Some(root.path()));
        let state = HookServerState::new();
        Self {
            _root: root,
            dir: tempfile::tempdir().unwrap(),
            app: hook_receiver_router_with_state(state.clone()),
            state,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
            _rotations: rotations,
            _state: state_lock,
        }
    }

    fn path(&self, session: &str) -> PathBuf {
        self.dir.path().join(format!("{session}.jsonl"))
    }

    fn transcript(&self, session: &str) -> PathBuf {
        let path = self.path(session);
        fs::write(&path, b"{}\n").unwrap();
        path
    }

    /// A managed Claude pane bound to `a`, logging to `channel`.
    fn pane(&self, tmux: &str, channel: u64, a: &str) -> PathBuf {
        let a_path = self.transcript(a);
        register_provider_session("claude", a, tmux);
        register_tmux_channel(tmux, channel);
        register_tmux_runtime_binding(tmux, claude(&a_path, a));
        a_path
    }

    fn payload(&self, session: &str, source: Option<&str>) -> Value {
        json!({ "session_id": session, "source": source, "transcript_path": self.path(session) })
    }

    fn send(&self, uri: &str, payload: &Value, request_id: Option<&str>) -> (u16, Value) {
        let now = chrono::Utc::now();
        let mut request = axum::http::Request::post(uri).header("content-type", "application/json");
        if let Some(request_id) = request_id {
            request = request
                .header(RELAY_REQUEST_ID_HEADER, request_id)
                .header(RELAY_PUBLISHED_AT_HEADER, now.to_rfc3339())
                .header(
                    RELAY_DEADLINE_HEADER,
                    (now + chrono::Duration::minutes(5)).to_rfc3339(),
                );
        }
        let request = request
            .body(axum::body::Body::from(payload.to_string()))
            .unwrap();
        self.runtime.block_on(async {
            let response = self.app.clone().oneshot(request).await.unwrap();
            let status = response.status().as_u16();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, serde_json::from_slice(&body).unwrap())
        })
    }

    fn claude_hook(&self, event: &str, command: &str, payload: &Value, id: Option<&str>) -> u16 {
        let uri = format!("/hooks/claude/{event}?session_id={command}");
        self.send(&uri, payload, id).0
    }

    /// Seeds one memento recall whose feedback the next Stop of `session` must flush.
    fn seed_feedback(&self, session: &str) {
        let recall = json!({
            "tool_name": "mcp__memento__recall",
            "tool_response": {"_meta": {"searchEventId": "4308"}}
        });
        let uri = format!("/hooks/claude/PostToolUse?session_id={session}");
        assert_eq!(self.send(&uri, &recall, None).0, 202);
        assert_eq!(self.state.memento_feedback.pending_count(session), 1);
    }
}

impl Drop for Ingress {
    fn drop(&mut self) {
        set_test_root(None);
        APPEND_FAULT.with(|fault| fault.set(None));
        set_discovery_pending_for_tests(false);
        reset_deferred_adoptions_for_tests();
        reset_state_for_tests();
    }
}

fn claude(path: &std::path::Path, session: &str) -> TuiRuntimeBinding {
    TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(session.to_owned()),
        last_offset: 0,
        relay_last_offset: None,
    }
}

fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn events(channel: u64) -> Vec<BindingEvent> {
    binding_events_since(channel, 0).unwrap()
}

fn pending_lines(channel: u64, session: &str) -> usize {
    let is_pending = |e: &BindingEvent| matches!(&e.new, BindingTarget::Pending { payload_session_id, .. } if payload_session_id == session);
    events(channel).iter().filter(|e| is_pending(e)).count()
}

fn session_lines(channel: u64, session: &str) -> Vec<u64> {
    let names = |e: &BindingEvent| match &e.new {
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => {
            source.session_id == session
        }
        _ => false,
    };
    events(channel)
        .into_iter()
        .filter(|e| names(e))
        .map(|e| e.seq)
        .collect()
}

fn buffered(session: &str) -> usize {
    let key = RegistryKey::new("claude", Some(session), None).unwrap();
    hook_registry::global().buffered_len(&key)
}

fn drain(rx: &mut tokio::sync::broadcast::Receiver<HookEvent>) -> usize {
    std::iter::from_fn(|| rx.try_recv().ok()).count()
}

fn check_refused_until_durable(fault: &'static str) {
    let ingress = Ingress::new();
    let (channel, tmux) = (7_400, "ingress-fault");
    let (a, b, request_id) = (uuid(), uuid(), uuid());
    ingress.pane(tmux, channel, &a);
    let fork = ingress.payload(&b, Some("fork"));
    let before = events(channel).len();

    APPEND_FAULT.with(|slot| slot.set(Some(fault)));
    let (status, body) = ingress.send(
        &format!("/hooks/claude/SessionStart?session_id={a}"),
        &fork,
        Some(&request_id),
    );
    assert_eq!(status, 425, "first send status == 425 ({fault}): {body}");
    assert_eq!(
        events(channel).len(),
        before,
        "no line survives a failed {fault}"
    );

    APPEND_FAULT.with(|slot| slot.set(None));
    let resend = ingress.claude_hook("SessionStart", &a, &fork, Some(&request_id));
    assert_eq!(resend, 202, "resend.status == 202");
    assert_eq!(pending_lines(channel, &b), 1, "Pending line count == 1");
}

#[test]
fn a_pending_write_failure_is_refused_and_the_same_request_is_acknowledged_once_logged() {
    check_refused_until_durable("write");
}

#[test]
fn a_pending_fsync_failure_is_refused_and_leaves_no_line() {
    check_refused_until_durable("sync");
}

#[test]
fn a_refused_hook_has_no_effect_until_its_resend_is_acknowledged() {
    let ingress = Ingress::new();
    let (channel, tmux) = (7_410, "ingress-effects");
    let (a, b, request_id) = (uuid(), uuid(), uuid());
    ingress.pane(tmux, channel, &a);
    ingress.seed_feedback(&a);
    let base = buffered(&a);
    let mut rx = ingress.state.subscribe();
    let clear = ingress.payload(&b, Some("clear"));

    APPEND_FAULT.with(|slot| slot.set(Some("write")));
    assert_eq!(
        ingress.claude_hook("SessionStart", &a, &clear, Some(&request_id)),
        425
    );
    let refused = (buffered(&a) - base, buffered(&b), drain(&mut rx));
    assert_eq!(
        refused,
        (0, 0, 0),
        "refused hook reached registry or broadcast"
    );
    let pending = ingress.state.memento_feedback.pending_count(&a);
    assert_eq!(pending, 1, "refused hook changed memento state");

    APPEND_FAULT.with(|slot| slot.set(None));
    assert_eq!(
        ingress.claude_hook("SessionStart", &a, &clear, Some(&request_id)),
        202
    );
    let accepted = (buffered(&a) - base, buffered(&b), drain(&mut rx));
    assert_eq!(accepted, (1, 0, 1), "accepted hook delivered exactly once");
    assert_eq!(ingress.state.memento_feedback.pending_count(&a), 0);
}

#[test]
fn a_codex_session_switch_is_not_adopted_as_claude() {
    let ingress = Ingress::new();
    let (channel, tmux) = (7_420, "ingress-codex");
    let (a, b) = (uuid(), uuid());
    ingress.pane(tmux, channel, &a);
    ingress.transcript(&b);
    let before = (
        events(channel).len(),
        runtime_binding_for_tmux_session(tmux),
    );
    let uri = format!("/hooks/codex/SessionStart?session_id={a}");
    let (status, _) = ingress.send(&uri, &ingress.payload(&b, Some("clear")), Some(&uuid()));
    assert_eq!(status, 202);
    let after = (
        events(channel).len(),
        runtime_binding_for_tmux_session(tmux),
    );
    assert_eq!(
        after, before,
        "codex hook changed the Claude log or binding"
    );
}

#[test]
fn a_repeat_hook_of_a_recorded_source_waiting_for_its_rotation_is_acknowledged() {
    let ingress = Ingress::new();
    let (channel, tmux) = (7_430, "ingress-repeat");
    let (a, b) = (uuid(), uuid());
    ingress.pane(tmux, channel, &a);
    ingress.transcript(&b);
    APPEND_FAULT.with(|slot| slot.set(Some("write")));
    let clear = ingress.payload(&b, Some("clear"));
    assert_eq!(
        ingress.claude_hook("SessionStart", &a, &clear, Some(&uuid())),
        425
    );
    APPEND_FAULT.with(|slot| slot.set(None));
    retry_deferred_claude_adoptions();
    assert_eq!(deferred_adoption_count(), 1, "B held until A→B settles");
    let logged = events(channel).len();

    let stop = ingress.payload(&b, None);
    assert_eq!(
        ingress.claude_hook("Stop", &a, &stop, Some(&uuid())),
        202,
        "status == 202"
    );
    assert_eq!(events(channel).len(), logged, "repeat adds no record");
}

#[test]
fn a_hook_behind_an_unlogged_front_is_refused_until_its_own_source_is_logged() {
    let ingress = Ingress::new();
    let (channel, tmux) = (7_440, "ingress-behind");
    let (a, b, c, r_id) = (uuid(), uuid(), uuid(), uuid());
    ingress.pane(tmux, channel, &a);
    let (b_path, c_path) = (ingress.transcript(&b), ingress.transcript(&c));
    filetime::set_file_mtime(&b_path, filetime::FileTime::from_unix_time(20, 0)).unwrap();
    filetime::set_file_mtime(&c_path, filetime::FileTime::from_unix_time(30, 0)).unwrap();
    let (front, r) = (
        ingress.payload(&b, Some("clear")),
        ingress.payload(&c, Some("clear")),
    );

    APPEND_FAULT.with(|slot| slot.set(Some("write")));
    assert_eq!(
        ingress.claude_hook("SessionStart", &a, &front, Some(&uuid())),
        425
    );
    let uri = format!("/hooks/claude/SessionStart?session_id={a}");
    let (status, body) = ingress.send(&uri, &r, Some(&r_id));
    assert_eq!(
        (status, &body["reason"]),
        (425, &json!("NotDurable(QueuedBehind)"))
    );
    assert!(session_lines(channel, &c).is_empty());

    APPEND_FAULT.with(|slot| slot.set(None));
    retry_deferred_claude_adoptions();
    assert_eq!(
        session_lines(channel, &b).len(),
        1,
        "front logged, rotation pending"
    );
    let resend = ingress.claude_hook("SessionStart", &a, &r, Some(&r_id));
    assert!(
        resend == 425 && session_lines(channel, &c).is_empty(),
        "status == 425 && no R record while the front's rotation is pending (got {resend})"
    );

    assert!(clear_claude_session_rotation(tmux));
    retry_deferred_claude_adoptions();
    assert_eq!(
        ingress.claude_hook("SessionStart", &a, &r, Some(&r_id)),
        202
    );
    let (b_seq, c_seq) = (session_lines(channel, &b)[0], session_lines(channel, &c)[0]);
    assert!(b_seq < c_seq, "front record precedes R record");
}

#[test]
fn an_unmapped_hook_is_refused_until_the_first_discovery_pass_finishes() {
    let ingress = Ingress::new();
    let (x, y, request_id) = (uuid(), uuid(), uuid());
    let payload = ingress.payload(&y, Some("clear"));
    let before = ingress_counters_for_tests().0;
    set_discovery_pending_for_tests(true);
    let (status, body) = ingress.send(
        &format!("/hooks/claude/SessionStart?session_id={x}"),
        &payload,
        Some(&request_id),
    );
    assert_eq!(status, 425, "first status == 425: {body}");
    mark_boot_discovery_complete();
    let resend = ingress.claude_hook("SessionStart", &x, &payload, Some(&request_id));
    assert_eq!(resend, 202);
    assert_eq!(
        ingress_counters_for_tests().0,
        before + 1,
        "UnmappedCommandSession counted"
    );
}

#[test]
fn a_legacy_hook_that_cannot_be_logged_is_refused_and_counted() {
    let ingress = Ingress::new();
    let (channel, tmux) = (7_450, "ingress-legacy");
    let (a, b) = (uuid(), uuid());
    ingress.pane(tmux, channel, &a);
    let before = ingress_counters_for_tests().1;
    APPEND_FAULT.with(|slot| slot.set(Some("write")));
    let status = ingress.claude_hook("SessionStart", &a, &ingress.payload(&b, Some("fork")), None);
    assert_eq!(status, 425, "legacy status == 425");
    assert_eq!(
        ingress_counters_for_tests().1,
        before + 1,
        "legacy_not_durable == 1"
    );
}

#[test]
fn a_pane_whose_boot_registration_failed_stays_refused_after_discovery() {
    let ingress = Ingress::new();
    let (channel, tmux) = (7_460, "ingress-boot-failure");
    let (a, b, request_id) = (uuid(), uuid(), uuid());
    let a_path = ingress.transcript(&a);
    let mut rx = ingress.state.subscribe();

    // The pass: a live pane with a channel and a launch transcript whose first append fails.
    APPEND_FAULT.with(|slot| slot.set(Some("write")));
    let registered =
        register_rehydrated_tmux_runtime_binding("claude", tmux, channel, claude(&a_path, &a));
    assert!(!registered);
    note_claude_pane_registration(tmux, Some(&a), registered);
    mark_boot_discovery_complete();
    APPEND_FAULT.with(|slot| slot.set(None));

    let clear = ingress.payload(&b, Some("clear"));
    let uri = format!("/hooks/claude/SessionStart?session_id={a}");
    let (status, body) = ingress.send(&uri, &clear, Some(&request_id));
    assert_eq!(status, 425, "failed pane status == 425: {body}");
    assert_eq!((buffered(&a), buffered(&b), drain(&mut rx)), (0, 0, 0));
    assert!(events(channel).is_empty());

    let registered =
        register_rehydrated_tmux_runtime_binding("claude", tmux, channel, claude(&a_path, &a));
    assert!(registered);
    note_claude_pane_registration(tmux, Some(&a), registered);
    let resend = ingress.claude_hook("SessionStart", &a, &clear, Some(&request_id));
    assert_eq!(
        resend, 202,
        "the abandoned receipt lets the same id through"
    );
    assert_eq!(pending_lines(channel, &b), 1);
}

#[test]
fn a_poll_over_a_front_that_cannot_be_logged_returns() {
    let ingress = Ingress::new();
    let (channel, tmux) = (7_470, "ingress-poll-returns");
    let (a, b) = (uuid(), uuid());
    ingress.pane(tmux, channel, &a);
    ingress.transcript(&b);
    APPEND_FAULT.with(|slot| slot.set(Some("write")));
    let clear = ingress.payload(&b, Some("clear"));
    assert_eq!(
        ingress.claude_hook("SessionStart", &a, &clear, Some(&uuid())),
        425
    );
    assert_eq!(deferred_adoption_count(), 1);

    // The poll runs where the log still fails; a Hold must end it instead of settling again.
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let root = ingress._root.path().to_path_buf();
    std::thread::spawn(move || {
        set_test_root(Some(&root));
        APPEND_FAULT.with(|slot| slot.set(Some("write")));
        retry_deferred_claude_adoptions();
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("poll returned");
    assert_eq!(
        deferred_adoption_count(),
        1,
        "the unlogged front stays queued"
    );
}
