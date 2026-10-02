use std::collections::HashMap;
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
    RELAY_RESPOND_BY_HEADER,
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
pub(crate) struct Ingress {
    _root: tempfile::TempDir,
    dir: tempfile::TempDir,
    pub(crate) state: HookServerState,
    app: Router,
    pins: std::cell::RefCell<HashMap<String, (String, Value, HeaderMap)>>,
    runtime: tokio::runtime::Runtime,
    _rotations: MutexGuard<'static, ()>,
    _state: MutexGuard<'static, ()>,
}

impl Ingress {
    pub(crate) fn new() -> Self {
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
            pins: Default::default(),
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
            _rotations: rotations,
            _state: state_lock,
        }
    }

    pub(crate) fn pending_feedback(&self, session: &str) -> usize {
        self.state.memento_feedback.pending_count(session)
    }

    pub(crate) fn path(&self, session: &str) -> PathBuf {
        self.dir.path().join(format!("{session}.jsonl"))
    }

    pub(crate) fn transcript(&self, session: &str) -> PathBuf {
        let path = self.path(session);
        let row = json!({"type": "mode", "sessionId": session});
        fs::write(&path, format!("{row}\n")).unwrap();
        path
    }

    /// A managed Claude pane bound to `a`, logging to `channel`.
    pub(crate) fn pane(&self, tmux: &str, channel: u64, a: &str) -> PathBuf {
        let a_path = self.transcript(a);
        register_provider_session("claude", a, tmux);
        register_tmux_channel(tmux, channel);
        register_tmux_runtime_binding(tmux, claude(&a_path, a));
        a_path
    }

    pub(crate) fn payload(&self, session: &str, source: Option<&str>) -> Value {
        json!({ "session_id": session, "source": source, "transcript_path": self.path(session) })
    }

    pub(crate) fn send(
        &self,
        uri: &str,
        payload: &Value,
        request_id: Option<&str>,
    ) -> (u16, Value) {
        self.send_envelope(uri, payload, request_id, None)
    }

    pub(crate) fn send_envelope(
        &self,
        uri: &str,
        payload: &Value,
        request_id: Option<&str>,
        envelope: Option<&str>,
    ) -> (u16, Value) {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        if let Some(envelope) = envelope {
            headers.insert(
                crate::services::tui_prompt_dedupe::binding_context::BINDING_HEADER,
                envelope.parse().unwrap(),
            );
        }
        let mut frozen_payload = payload.clone();
        let mut frozen_uri = uri.to_owned();
        if let Some(request_id) = request_id {
            let mut pins = self.pins.borrow_mut();
            let pin = pins.entry(request_id.to_owned()).or_insert_with(|| {
                let now = chrono::Utc::now();
                headers.insert(RELAY_REQUEST_ID_HEADER, request_id.parse().unwrap());
                headers.insert(RELAY_PUBLISHED_AT_HEADER, now.to_rfc3339().parse().unwrap());
                headers.insert(
                    RELAY_DEADLINE_HEADER,
                    (now + chrono::Duration::minutes(5))
                        .to_rfc3339()
                        .parse()
                        .unwrap(),
                );
                headers.insert(
                    RELAY_RESPOND_BY_HEADER,
                    (now + chrono::Duration::minutes(4))
                        .to_rfc3339()
                        .parse()
                        .unwrap(),
                );
                (uri.to_owned(), payload.clone(), headers.clone())
            });
            assert_eq!(
                (&pin.0, &pin.1),
                (&uri.to_owned(), payload),
                "retry must keep URI and payload"
            );
            assert_eq!(
                pin.2
                    .get(crate::services::tui_prompt_dedupe::binding_context::BINDING_HEADER),
                headers.get(crate::services::tui_prompt_dedupe::binding_context::BINDING_HEADER),
                "retry must keep binding envelope"
            );
            (frozen_uri, frozen_payload, headers) = pin.clone();
        }
        let mut request = axum::http::Request::post(frozen_uri);
        *request.headers_mut().unwrap() = headers;
        let request = request
            .body(axum::body::Body::from(frozen_payload.to_string()))
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

    pub(crate) fn claude_hook(
        &self,
        event: &str,
        command: &str,
        payload: &Value,
        id: Option<&str>,
    ) -> u16 {
        let uri = format!("/hooks/claude/{event}?session_id={command}");
        self.send(&uri, payload, id).0
    }

    /// Seeds one memento recall whose feedback the next Stop of `session` must flush.
    pub(crate) fn seed_feedback(&self, session: &str) {
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

pub(crate) fn claude(path: &std::path::Path, session: &str) -> TuiRuntimeBinding {
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

pub(crate) fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub(crate) fn events(channel: u64) -> Vec<BindingEvent> {
    binding_events_since(channel, 0).unwrap()
}

pub(crate) fn pending_lines(channel: u64, session: &str) -> usize {
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

pub(crate) fn buffered(session: &str) -> usize {
    let key = RegistryKey::new("claude", Some(session), None).unwrap();
    hook_registry::global().buffered_len(&key)
}

pub(crate) fn drain(rx: &mut tokio::sync::broadcast::Receiver<HookEvent>) -> usize {
    std::iter::from_fn(|| rx.try_recv().ok()).count()
}

fn check_refused_until_durable(fault: &'static str) {
    let ingress = Ingress::new();
    let (channel, tmux) = (7_400, "ingress-fault");
    let (a, b, request_id) = (uuid(), uuid(), uuid());
    ingress.pane(tmux, channel, &a);
    let fork = ingress.payload(&b, Some("fork"));
    let before = events(channel).len();
    let base = buffered(&a);
    let mut rx = ingress.state.subscribe();

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

    assert_eq!(
        (buffered(&a) - base, buffered(&b), drain(&mut rx)),
        (0, 0, 0)
    );
    APPEND_FAULT.with(|slot| slot.set(None));
    let resend = ingress.claude_hook("SessionStart", &a, &fork, Some(&request_id));
    assert_eq!(resend, 202, "resend.status == 202");
    assert_eq!(pending_lines(channel, &b), 1, "Pending line count == 1");
    assert_eq!(
        events(channel).len(),
        before + 1,
        "accepted adds exactly one log line"
    );
    assert_eq!(
        (buffered(&a) - base, buffered(&b), drain(&mut rx)),
        (1, 0, 1)
    );
    let logged = events(channel).len();
    let cached = ingress.claude_hook("SessionStart", &a, &fork, Some(&request_id));
    assert_ne!(cached, 409, "same pin never conflicts (409)");
    assert_eq!(cached, 202, "cached status == 202");
    assert_eq!(events(channel).len(), logged);
    assert_eq!(
        (buffered(&a) - base, buffered(&b), drain(&mut rx)),
        (1, 0, 0)
    );
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
        // A start of C could not show it is newer than B's hooked move; C's prompt can.
        ingress.payload(&c, None),
    );

    APPEND_FAULT.with(|slot| slot.set(Some("write")));
    assert_eq!(
        ingress.claude_hook("SessionStart", &a, &front, Some(&uuid())),
        425
    );
    let uri = format!("/hooks/claude/UserPromptSubmit?session_id={a}");
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
    let resend = ingress.claude_hook("UserPromptSubmit", &a, &r, Some(&r_id));
    assert!(
        resend == 425 && session_lines(channel, &c).is_empty(),
        "status == 425 && no R record while the front's rotation is pending (got {resend})"
    );

    assert!(clear_claude_session_rotation(tmux));
    retry_deferred_claude_adoptions();
    assert_eq!(
        ingress.claude_hook("UserPromptSubmit", &a, &r, Some(&r_id)),
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

#[test]
fn discovery_grace_uses_the_production_sixty_second_boundary() {
    let ingress = Ingress::new();
    let (a, b) = (uuid(), uuid());
    let payload = ingress.payload(&b, Some("clear"));
    TEST_DISCOVERY_CLOCK.set((false, Duration::from_secs(60) - Duration::from_nanos(1)));
    assert_eq!(
        ingress.claude_hook("SessionStart", &a, &payload, Some(&uuid())),
        425
    );
    TEST_DISCOVERY_CLOCK.set((false, Duration::from_secs(60)));
    assert_eq!(
        ingress.claude_hook("SessionStart", &a, &payload, Some(&uuid())),
        202
    );
    note_claude_pane_registration("ingress-grace-failure", Some(&a), false);
    assert_eq!(
        ingress.claude_hook("SessionStart", &a, &payload, Some(&uuid())),
        425,
        "grace never clears pane failure"
    );
}

#[test]
fn a_legacy_command_alias_is_kept_until_its_mapping_is_ready() {
    let ingress = Ingress::new();
    let (a, h, b, id) = (uuid(), uuid(), uuid(), uuid());
    let (tmux, channel) = ("ingress-old-alias", 7491);
    let path = ingress.pane(tmux, channel, &a);
    register_provider_session("claude", &uuid(), tmux);
    register_provider_session("claude", &h, tmux);
    note_claude_pane_registration(tmux, Some(&a), false);
    reset_state_for_tests();
    let payload = ingress.payload(&b, Some("clear"));
    assert_eq!(
        ingress.claude_hook("SessionStart", &h, &payload, Some(&id)),
        425,
        "old alias status == 425"
    );
    assert!(register_rehydrated_tmux_runtime_binding(
        "claude",
        tmux,
        channel,
        claude(&path, &a)
    ));
    note_claude_pane_registration(tmux, Some(&a), true);
    assert_eq!(
        crate::services::tui_prompt_dedupe::provider_session_for_tmux("claude", tmux).as_deref(),
        Some(h.as_str()),
        "newest legacy command remains the wait key among multiple aliases"
    );
    assert_eq!(
        ingress.claude_hook("SessionStart", &h, &payload, Some(&id)),
        202
    );
    assert_eq!(pending_lines(channel, &b), 1);
}

// With no Herdr pane listed a tmux pane's switch never looks the hold up, so the hook path takes
// no lock it did not take before Herdr existed.
#[test]
fn a_tmux_pane_switch_never_looks_up_the_herdr_hold() {
    use crate::services::tui_prompt_dedupe::HERDR_HOLD_LOOKUPS;
    let ingress = Ingress::new();
    let (channel, tmux) = (7_430, "ingress-herdr-off");
    let (a, b) = (uuid(), uuid());
    ingress.pane(tmux, channel, &a);
    ingress.transcript(&b);
    let switch = ingress.payload(&b, None);
    let status = ingress.claude_hook("UserPromptSubmit", &a, &switch, Some(&uuid()));
    assert_eq!(status, 202);
    let bound = runtime_binding_for_tmux_session(tmux).unwrap().session_id;
    assert_eq!(bound.as_deref(), Some(b.as_str()));
    assert_eq!(HERDR_HOLD_LOOKUPS.with(std::cell::Cell::get), 0);
}

// A Herdr hold follows the execution it names: an unnamed refusal holds no unlisted pane, an older
// execution's refusal leaves a newer admission open, and the listed one's holds until admitted.
#[test]
fn a_herdr_hold_follows_only_the_execution_it_names() {
    use crate::services::tui_prompt_dedupe::{admit_herdr_execution, withhold_herdr_execution};
    let ingress = Ingress::new();
    let (channel, tmux) = (7_431, "ingress-herdr-hold");
    let (a, b, c, d) = (uuid(), uuid(), uuid(), uuid());
    ingress.pane(tmux, channel, &a);
    let bound = || runtime_binding_for_tmux_session(tmux).unwrap().session_id;
    let switch = |next: &str, id: &str| {
        ingress.transcript(next);
        let payload = ingress.payload(next, None);
        ingress.claude_hook("UserPromptSubmit", &a, &payload, Some(id))
    };
    withhold_herdr_execution(tmux, None);
    assert_eq!(switch(&b, &uuid()), 202);
    assert_eq!(bound().as_deref(), Some(b.as_str()));
    clear_claude_session_rotation(tmux);

    admit_herdr_execution(tmux, "n2");
    withhold_herdr_execution(tmux, Some("n1"));
    assert_eq!(switch(&c, &uuid()), 202);
    assert_eq!(bound().as_deref(), Some(c.as_str()));
    clear_claude_session_rotation(tmux);

    withhold_herdr_execution(tmux, Some("n2"));
    let (id, lines) = (uuid(), events(channel).len());
    assert_eq!(switch(&d, &id), 425);
    withhold_herdr_execution(tmux, None);
    assert_eq!(switch(&d, &id), 425);
    assert_eq!((bound(), events(channel).len()), (Some(c.clone()), lines));
    admit_herdr_execution(tmux, "n2");
    assert_eq!(switch(&d, &id), 202);
    assert_eq!(bound().as_deref(), Some(d.as_str()));
}
