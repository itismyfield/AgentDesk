use super::*;
use crate::config::TestEnvVarGuard as Guard;
use crate::services::agent_protocol::{RuntimeHandoff, RuntimeHandoffKind, StreamMessage};
use crate::services::codex_tui::{rollout_index, session};
use crate::services::tui_prompt_dedupe as dedupe;
use dedupe::binding_context::{
    BINDING_HEADER, BindingContext, CapturedContext, HookBindingEnvelope, ObservedHookProcess,
    PreparedIncarnation,
};
use dedupe::binding_events::{self, APPEND_FAULT, BindingEvent, BindingTarget};
use std::fs;
use std::path::{Path, PathBuf};
use tower::ServiceExt;

struct Harness {
    _env: Vec<Guard>,
    _lock: std::sync::MutexGuard<'static, ()>,
    root: tempfile::TempDir,
    requests:
        std::cell::RefCell<std::collections::HashMap<String, (axum::http::HeaderMap, String)>>,
    context: BindingContext,
    app: Router,
    state: HookServerState,
    rt: tokio::runtime::Runtime,
    command: String,
    payload: Value,
    header: Value,
    path: PathBuf,
    // Last field: the env guards above restore while this lock is still held.
    _env_lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

impl Harness {
    fn new() -> Self {
        let env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (root, env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
        let home = root.path().join("launch-home");
        let home_env = Guard::set_path_after_shared_test_env_lock("CODEX_HOME", &home);
        dedupe::reset_state_for_tests();
        binding_events::set_test_root(Some(root.path()));
        let data: Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/hook_payload/codex-0.157.1.json"
        ))
        .unwrap();
        let run = &data["runs"][1];
        let start = &run["events"][0];
        let clear = &run["events"][3];
        let command = clear["command_session_id"].as_str().unwrap().to_owned();
        let prepared = PreparedIncarnation::prepare_at(
            "codex",
            "codex-ingress-test",
            Some(8745),
            start["payload"]["session_id"].as_str(),
            false,
            Some(home.join("sessions")),
        )
        .unwrap();
        let context = prepared.context;
        let marker =
            crate::services::tmux_common::session_temp_path(&context.tmux_session, "spawn_nonce");
        fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
        fs::write(marker, &context.execution_nonce).unwrap();
        let local = |payload: &Value| {
            home.join("sessions").join(
                payload["transcript_path"]
                    .as_str()
                    .unwrap()
                    .split_once("/.codex/sessions/")
                    .unwrap()
                    .1,
            )
        };
        let old = local(&start["payload"]);
        write(&old, &run["rollout_session_meta"][0]);
        let path = local(&clear["payload"]);
        let mut payload = clear["payload"].clone();
        payload["transcript_path"] = json!(path);
        dedupe::register_provider_session("codex", &command, &context.tmux_session);
        dedupe::register_tmux_channel(&context.tmux_session, 8745);
        session::install_codex_tui_runtime_binding(
            &context.tmux_session,
            Some(19),
            dedupe::TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: old.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: start["payload"]["session_id"].as_str().map(str::to_owned),
                last_offset: 19,
                relay_last_offset: Some(19),
            },
        );
        let state = HookServerState::new();
        Self {
            _env: std::iter::once(home_env).chain(env).collect(),
            _lock: lock,
            root,
            context,
            requests: Default::default(),
            app: hook_receiver_router_with_state(state.clone()),
            state,
            rt: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
            command,
            payload,
            header: run["rollout_session_meta"][1].clone(),
            path,
            _env_lock: env_lock,
        }
    }

    fn send(&self, payload: &Value, context: Option<BindingContext>, id: &str) -> (u16, Value) {
        let (headers, body) = self
            .requests
            .borrow_mut()
            .entry(id.to_owned())
            .or_insert_with(|| {
                let envelope = HookBindingEnvelope {
                    context: context.map_or_else(
                        || {
                            CapturedContext::Absent(
                                dedupe::binding_context::AbsentReason::LegacyRequest,
                            )
                        },
                        CapturedContext::Captured,
                    ),
                    observed: ObservedHookProcess::default(),
                };
                let now = Utc::now();
                let mut headers = axum::http::HeaderMap::new();
                for (name, value) in [
                    ("content-type", "application/json".to_owned()),
                    (BINDING_HEADER, envelope.encode().unwrap()),
                    (
                        relay_receipts::RELAY_REQUEST_ID_HEADER,
                        uuid::Uuid::new_v4().to_string(),
                    ),
                    (relay_receipts::RELAY_PUBLISHED_AT_HEADER, now.to_rfc3339()),
                    (
                        relay_receipts::RELAY_DEADLINE_HEADER,
                        (now + chrono::Duration::minutes(5)).to_rfc3339(),
                    ),
                ] {
                    headers.insert(name, value.parse().unwrap());
                }
                (headers, payload.to_string())
            })
            .clone();
        let mut req = axum::http::Request::post(format!(
            "/hooks/codex/SessionStart?session_id={}",
            self.command
        ))
        .body(axum::body::Body::from(body))
        .unwrap();
        *req.headers_mut() = headers;
        self.rt.block_on(async {
            let response = self.app.clone().oneshot(req).await.unwrap();
            let status = response.status().as_u16();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, serde_json::from_slice(&bytes).unwrap())
        })
    }
    fn hook(&self) -> (u16, Value) {
        self.send(
            &self.payload,
            Some(self.context.clone()),
            &uuid::Uuid::new_v4().to_string(),
        )
    }
    fn binding(&self) -> dedupe::TuiRuntimeBinding {
        dedupe::runtime_binding_for_tmux_session(&self.context.tmux_session).unwrap()
    }
    fn events(&self) -> Vec<BindingEvent> {
        binding_events::binding_events_since(8745, 0).unwrap()
    }
    fn snapshot(
        &self,
    ) -> (
        dedupe::TuiRuntimeBinding,
        Vec<BindingEvent>,
        session::CodexTuiRolloutMarker,
    ) {
        (
            self.binding(),
            self.events(),
            session::read_codex_tui_rollout_marker(&self.context.tmux_session).unwrap(),
        )
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        APPEND_FAULT.with(|s| s.set(None));
        binding_events::set_test_root(None);
        dedupe::reset_state_for_tests();
    }
}
fn write(path: &Path, header: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("{header}\n")).unwrap();
}
fn source(event: &BindingEvent) -> &binding_events::SourceId {
    match &event.new {
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => source,
        other => panic!("expected verified source, got {other:?}"),
    }
}

#[test]
fn codex_clear_fixture_binds_the_verified_rollout_and_preserves_repeat_cursor() {
    let h = Harness::new();
    write(&h.path, &h.header);
    let before = h.events().len();
    assert_eq!(h.hook().0, 202);
    assert_eq!(
        h.binding().output_path,
        h.path.canonicalize().unwrap().display().to_string(),
        "clear must bind the verified new rollout"
    );
    let events = h.events();
    assert_eq!(events.len(), before + 1);
    let event = events.last().unwrap();
    assert_eq!(source(event).path, h.path.canonicalize().unwrap());
    assert_eq!(
        source(event).session_id,
        h.payload["session_id"].as_str().unwrap()
    );
    assert_eq!(
        event.execution_nonce.as_deref(),
        Some(h.context.execution_nonce.as_str())
    );
    assert_eq!(event.cause, binding_events::BindingCause::Clear);
    assert_eq!(event.evidence.hook_event.as_deref(), Some("session_start"));
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(&h.path).unwrap();
    assert_eq!(
        (source(event).dev, source(event).ino),
        (meta.dev(), meta.ino())
    );
    let marker = session::read_codex_tui_rollout_marker(&h.context.tmux_session).unwrap();
    assert_eq!(marker.rollout_path, source(event).path);
    assert_eq!(marker.session_id, h.binding().session_id);
    assert_eq!(h.binding().last_offset, 0);
    session::advance_codex_tui_runtime_binding_and_marker_offset(
        &h.context.tmux_session,
        &h.path,
        47,
    );
    let stable = h.snapshot();
    assert_eq!(h.hook().0, 202);
    assert_eq!(
        h.snapshot(),
        stable,
        "repeat hook must preserve the source cursor"
    );
}

#[test]
fn codex_retryable_claims_are_durable_pending_even_when_a_file_exists() {
    let h = Harness::new();
    let original = h.binding();
    for phase in ["absent", "unfinished", "complete"] {
        if phase == "unfinished" {
            fs::write(&h.path, "{\"type\":").unwrap();
        }
        if phase == "complete" {
            write(&h.path, &h.header);
        }
        assert_eq!(h.hook().0, 202);
        let events = h.events();
        if phase != "complete" {
            assert!(
                matches!(events.last().unwrap().new, BindingTarget::Pending { .. }),
                "retryable source must be durable Pending before ACK: {phase}"
            );
            assert_eq!(h.binding(), original);
        } else {
            assert!(matches!(
                events.last().unwrap().new,
                BindingTarget::Resolved { .. }
            ));
            assert_ne!(h.binding(), original);
        }
    }
}

#[test]
fn codex_incomplete_index_is_pending_and_never_a_verified_source() {
    let h = Harness::new();
    let _index = rollout_index::lock_cache_for_tests();
    write(&h.path, &h.header);
    let before = h.binding();
    let mut payload = h.payload.clone();
    payload.as_object_mut().unwrap().remove("transcript_path");
    rollout_index::fail_header_reads_for_tests(Some(h.path.clone()));
    assert_eq!(
        h.send(&payload, Some(h.context.clone()), "incomplete-index")
            .0,
        202
    );
    let events = h.events();
    rollout_index::fail_header_reads_for_tests(None);
    assert!(
        matches!(events.last().unwrap().new, BindingTarget::Pending { .. }),
        "incomplete index must not be accepted as a Source"
    );
    assert_eq!(h.binding(), before);
}

#[test]
fn codex_permanent_rejections_leave_binding_log_and_cursor_untouched() {
    let h = Harness::new();
    write(&h.path, &h.header);
    let stable = h.snapshot();
    for problem in ["filename", "source", "outside"] {
        let mut payload = h.payload.clone();
        match problem {
            "filename" => payload["transcript_path"] = json!(h.path.with_file_name("wrong.jsonl")),
            "source" => {
                let mut header = h.header.clone();
                header["payload"]["source"] = json!("exec");
                write(&h.path, &header);
            }
            _ => {
                let outside = h.root.path().join(h.path.file_name().unwrap());
                write(&outside, &h.header);
                payload["transcript_path"] = json!(outside);
            }
        }
        let (status, body) = h.send(&payload, Some(h.context.clone()), problem);
        assert_eq!(status, 202);
        assert_eq!(
            h.snapshot(),
            stable,
            "permanent rejection must not become Pending or move binding/log/cursor: {problem}"
        );
        assert_eq!(
            body["binding_observation"], "NotApplicable(CodexSourceRejected)",
            "permanent rejection must be observable, not Proceed"
        );
    }
}

#[test]
fn codex_launch_root_is_captured_and_hook_env_cannot_change_the_verdict() {
    let h = Harness::new();
    write(&h.path, &h.header);
    let other = h.root.path().join("later-home");
    let _changed = Guard::set_path_after_shared_test_env_lock("CODEX_HOME", &other);
    assert_eq!(h.hook().0, 202);
    assert_eq!(
        h.binding().output_path,
        h.path.canonicalize().unwrap().display().to_string(),
        "hook must use launch root after env changes"
    );
}

#[test]
fn codex_missing_root_never_falls_back_to_hook_environment() {
    let h = Harness::new();
    write(&h.path, &h.header);
    let before = h.snapshot();
    let mut context = h.context.clone();
    context.provider_root = None;
    let (status, body) = h.send(&h.payload, Some(context), "no-root");
    assert_eq!(status, 202);
    assert_eq!(
        h.snapshot(),
        before,
        "missing launch root must not fall back to env"
    );
    assert_eq!(
        body["binding_observation"], "NotApplicable(CodexContextUnavailable)",
        "missing root must be explicitly rejected"
    );
}

#[test]
fn codex_pending_and_source_fsync_failures_are_425_and_retry_the_same_receipt() {
    let h = Harness::new();
    let mut events = h.state.subscribe();
    for (present, step) in [
        (false, "write"),
        (false, "sync"),
        (true, "write"),
        (true, "sync"),
    ] {
        if present {
            write(&h.path, &h.header);
        }
        let before = h.snapshot();
        APPEND_FAULT.with(|s| s.set(Some(step)));
        let (status, _) = h.send(
            &h.payload,
            Some(h.context.clone()),
            &format!("{present}-{step}"),
        );
        APPEND_FAULT.with(|s| s.set(None));
        assert_eq!(status, 425, "failed Codex persistence must refuse ACK");
        assert!(
            events.try_recv().is_err(),
            "refused source must not broadcast"
        );
        assert_eq!(
            h.snapshot(),
            before,
            "fsync failure must not publish or leave a log line"
        );
    }
    assert_eq!(
        h.send(&h.payload, Some(h.context.clone()), "true-sync").0,
        202,
        "abandoned receipt must retry"
    );
    assert_eq!(
        h.binding().session_id.as_deref(),
        h.payload["session_id"].as_str()
    );
}

#[test]
fn codex_stale_context_is_rejected_before_binding() {
    let h = Harness::new();
    write(&h.path, &h.header);
    let before = h.snapshot();
    for field in ["nonce", "provider", "pane", "absent"] {
        let mut ctx = h.context.clone();
        match field {
            "nonce" => ctx.execution_nonce = "0".repeat(32),
            "provider" => ctx.provider = "claude".into(),
            "pane" => ctx.tmux_session = "other-pane".into(),
            _ => {}
        }
        let ctx = (field != "absent").then_some(ctx);
        let (_, body) = h.send(&h.payload, ctx, field);
        assert_eq!(h.snapshot(), before);
        assert_eq!(
            body["binding_observation"], "NotApplicable(CodexContextUnavailable)",
            "invalid context must be refused"
        );
    }
}

#[test]
fn codex_successive_clear_hooks_cannot_rebind_a_retired_session() {
    let h = Harness::new();
    let first = h.binding();
    write(&h.path, &h.header);
    assert_eq!(h.hook().0, 202);
    let id = uuid::Uuid::new_v4().to_string();
    let next = h
        .path
        .with_file_name(format!("rollout-2026-09-27T21-06-14-{id}.jsonl"));
    let mut header = h.header.clone();
    header["payload"]["id"] = json!(id);
    write(&next, &header);
    let mut payload = h.payload.clone();
    payload["session_id"] = json!(id);
    payload["transcript_path"] = json!(next);
    assert_eq!(
        h.send(&payload, Some(h.context.clone()), "next-clear").0,
        202
    );
    let stable = h.snapshot();
    for (id, path) in [
        (first.session_id.unwrap(), first.output_path),
        (
            h.payload["session_id"].as_str().unwrap().to_owned(),
            h.path.display().to_string(),
        ),
    ] {
        payload["session_id"] = json!(id);
        payload["transcript_path"] = json!(path);
        let (_, body) = h.send(&payload, Some(h.context.clone()), &format!("late-{id}"));
        assert_eq!(
            h.snapshot(),
            stable,
            "late known source must not reverse a clear"
        );
        assert_eq!(
            body["binding_observation"],
            "NotApplicable(CodexSourceRejected)"
        );
    }
}

#[test]
fn codex_pending_claim_cannot_override_a_later_verified_source() {
    let h = Harness::new();
    assert_eq!(h.hook().0, 202);
    let id = uuid::Uuid::new_v4().to_string();
    let next = h
        .path
        .with_file_name(format!("rollout-2026-09-27T21-06-14-{id}.jsonl"));
    let mut header = h.header.clone();
    header["payload"]["id"] = json!(id);
    write(&next, &header);
    let mut payload = h.payload.clone();
    payload["session_id"] = json!(id);
    payload["transcript_path"] = json!(next);
    assert_eq!(h.send(&payload, Some(h.context.clone()), "newer").0, 202);
    let stable = h.snapshot();
    write(&h.path, &h.header);
    assert_eq!(
        h.hook().1["binding_observation"],
        "NotApplicable(CodexSourceRejected)"
    );
    assert_eq!(
        h.snapshot(),
        stable,
        "superseded Pending cannot overwrite Source"
    );
}

#[test]
fn codex_marker_failure_retries_the_durable_source_without_a_duplicate_event() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::new();
    write(&h.path, &h.header);
    let before = h.snapshot();
    let path = crate::services::tmux_common::session_temp_path(
        &h.context.tmux_session,
        crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
    );
    let permissions = fs::metadata(&path).unwrap().permissions();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
    let result = h.send(&h.payload, Some(h.context.clone()), "marker-failure");
    fs::set_permissions(&path, permissions).unwrap();
    assert_eq!(result.0, 425, "marker failure must be retried");
    assert_eq!(h.binding(), before.0);
    assert_eq!(h.snapshot().2, before.2);
    assert_eq!(h.events().len(), before.1.len() + 1);
    assert_eq!(
        h.send(&h.payload, Some(h.context.clone()), "marker-failure")
            .0,
        202
    );
    let after = h.snapshot();
    assert_eq!(
        h.events().len(),
        before.1.len() + 1,
        "retry must reuse durable Source"
    );
    assert_eq!(
        h.binding().session_id.as_deref(),
        h.payload["session_id"].as_str()
    );
    assert_eq!(
        h.send(&h.payload, Some(h.context.clone()), "marker-failure")
            .0,
        202
    );
    assert_eq!(
        h.snapshot(),
        after,
        "cached receipt must not repeat adoption"
    );
}

#[test]
fn codex_finished_old_tail_cannot_reclaim_a_hook_published_source() {
    let h = Harness::new();
    let _tmux = dedupe::binding_context::tests::fake_tmux(h.root.path());
    let old = h.binding();
    write(&h.path, &h.header);
    assert_eq!(h.hook().0, 202);
    let published = h.snapshot();
    let (sender, receiver) = std::sync::mpsc::channel();
    crate::services::codex::emit_codex_tui_post_tail_handoff(
        crate::services::codex_tui::rollout_tail::CodexTuiTailResult {
            read_result: crate::services::provider::ReadOutputResult::Completed { offset: 19 },
            rollout_path: PathBuf::from(&old.output_path),
            final_offset: 19,
            session_id: old.session_id,
        },
        sender,
        None,
        &h.context.tmux_session,
    )
    .unwrap();
    assert_eq!(
        h.snapshot(),
        published,
        "old tail must not republish its retired source"
    );
    assert!(
        receiver.try_recv().is_err(),
        "old tail must not hand its retired source to the bridge"
    );
    let (status, body) = h.hook();
    assert_eq!(status, 202);
    assert_ne!(
        body["binding_observation"], "NotApplicable(CodexSourceRejected)",
        "the current source must keep accepting its hooks"
    );
    assert_eq!(
        h.binding().output_path,
        h.path.canonicalize().unwrap().display().to_string(),
        "relay must keep reading the hook source"
    );
}

#[test]
fn codex_restore_after_a_crash_between_source_and_marker_completes_the_hook_source() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::new();
    let _tmux = dedupe::binding_context::tests::fake_tmux(h.root.path());
    write(&h.path, &h.header);
    let marker = crate::services::tmux_common::session_temp_path(
        &h.context.tmux_session,
        crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
    );
    let permissions = fs::metadata(&marker).unwrap().permissions();
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o400)).unwrap();
    let crashed = h.send(&h.payload, Some(h.context.clone()), "crash");
    fs::set_permissions(&marker, permissions).unwrap();
    assert_eq!(crashed.0, 425);
    let events = h.events();
    let current = source(events.last().unwrap()).clone();
    let marker_path = || {
        session::read_codex_tui_rollout_marker(&h.context.tmux_session)
            .unwrap()
            .rollout_path
    };
    assert_eq!(current.path, h.path.canonicalize().unwrap());
    assert_ne!(
        marker_path(),
        current.path,
        "marker still names the old source"
    );
    dedupe::reset_state_for_tests();
    binding_events::forget_channel_for_tests(8745);
    dedupe::register_provider_session("codex", &h.command, &h.context.tmux_session);
    let restored = crate::services::discord::rehydrate_codex_tui_binding_for_tests(
        &h.context.tmux_session,
        8745,
    )
    .expect("restore must register the live pane");
    let expected = current.path.display().to_string();
    assert_eq!(
        restored.output_path, expected,
        "restore must complete the hook source"
    );
    assert_eq!(h.binding().output_path, expected);
    assert_eq!(
        marker_path(),
        current.path,
        "restore must roll the marker forward"
    );
    assert_eq!(
        h.events(),
        events,
        "restore must not record the retired marker source as a new transition"
    );
    let (status, body) = h.send(&h.payload, Some(h.context.clone()), "crash");
    assert_eq!(
        status, 202,
        "the abandoned receipt must be acknowledged after restore"
    );
    assert_ne!(
        body["binding_observation"], "NotApplicable(CodexSourceRejected)",
        "the restored source must keep accepting its hooks"
    );
    assert_eq!(h.events(), events, "retry must reuse the durable Source");
}

/// Sets the hook switch, or leaves it unset for `None`; restored on drop.
fn hooks_switch(value: Option<&str>) -> Guard {
    let guard = Guard::capture_after_shared_test_env_lock("AGENTDESK_CODEX_DIRECT_TUI_HOOKS");
    match value {
        Some(value) => unsafe { std::env::set_var("AGENTDESK_CODEX_DIRECT_TUI_HOOKS", value) },
        None => unsafe { std::env::remove_var("AGENTDESK_CODEX_DIRECT_TUI_HOOKS") },
    }
    guard
}

/// A live pane whose capture fails, so a rollout-reported ready composer is taken as ready.
fn live_tmux(root: &Path) -> Guard {
    use std::os::unix::fs::PermissionsExt;
    let stub = "#!/bin/bash\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    capture-pane) exit 1 ;;\n    list-panes) echo 0; exit 0 ;;\n  esac\ndone\nexit 0\n";
    fs::write(root.join("tmux"), stub).unwrap();
    fs::set_permissions(root.join("tmux"), fs::Permissions::from_mode(0o700)).unwrap();
    Guard::set_path_after_shared_test_env_lock("PATH", root)
}

/// Runs the production post-tail handoff of `tail` with a ready composer and returns what the bridge got.
fn post_tail(h: &Harness, tail: &dedupe::TuiRuntimeBinding) -> Vec<StreamMessage> {
    crate::services::codex_tui::input::record_rollout_composer_ready(&h.context.tmux_session);
    let (sender, receiver) = std::sync::mpsc::channel();
    crate::services::codex::emit_codex_tui_post_tail_handoff(
        crate::services::codex_tui::rollout_tail::CodexTuiTailResult {
            read_result: crate::services::provider::ReadOutputResult::Completed { offset: 19 },
            rollout_path: PathBuf::from(&tail.output_path),
            final_offset: 19,
            session_id: tail.session_id.clone(),
        },
        sender,
        None,
        &h.context.tmux_session,
    )
    .unwrap();
    receiver.try_iter().collect()
}

fn handed_off(messages: &[StreamMessage], tail: &dedupe::TuiRuntimeBinding) -> bool {
    messages.iter().any(|message| {
        matches!(
            message,
            StreamMessage::RuntimeReady {
                handoff: RuntimeHandoff::CodexTui { rollout_path, .. },
            } if *rollout_path == tail.output_path
        )
    })
}

fn corrupt_binding_log(h: &Harness) {
    let log = h
        .root
        .path()
        .join(binding_events::BINDING_EVENTS_DIR)
        .join("8745.log");
    let mut text = fs::read_to_string(&log).unwrap();
    text.push_str("{not json\n");
    fs::write(&log, text).unwrap();
}

fn marker_path(h: &Harness) -> PathBuf {
    session::read_codex_tui_rollout_marker(&h.context.tmux_session)
        .unwrap()
        .rollout_path
}

#[test]
fn codex_tail_with_hooks_on_is_held_when_the_hook_history_is_unreadable() {
    let h = Harness::new();
    let _tmux = live_tmux(h.root.path());
    let _hooks = hooks_switch(Some("1"));
    let old = h.binding();
    write(&h.path, &h.header);
    assert_eq!(h.hook().0, 202);
    let hooked = h.binding();
    let hooked_marker = marker_path(&h);
    assert_ne!(hooked.output_path, old.output_path);
    corrupt_binding_log(&h);
    let messages = post_tail(&h, &old);
    assert_eq!(
        (h.binding(), marker_path(&h)),
        (hooked, hooked_marker),
        "[T1:held] an unreadable history must not let the old tail reclaim the hook source"
    );
    assert!(
        !handed_off(&messages, &old),
        "[T1:held] the held tail must not reach the bridge"
    );
}

#[test]
fn codex_runtime_ready_is_withheld_when_a_hook_replaces_the_source_during_the_readiness_wait() {
    let h = std::rc::Rc::new(Harness::new());
    let _tmux = live_tmux(h.root.path());
    let _hooks = hooks_switch(None);
    let old = h.binding();
    write(&h.path, &h.header);
    assert!(
        handed_off(&post_tail(&h, &old), &old),
        "[T2:control] a current source is handed off once the composer is ready"
    );
    let hooked = h.clone();
    crate::services::codex::AFTER_READINESS_WAIT.with_borrow_mut(|seam| {
        *seam = Some(Box::new(move || assert_eq!(hooked.hook().0, 202)));
    });
    let messages = post_tail(&h, &old);
    crate::services::codex::AFTER_READINESS_WAIT.with_borrow_mut(Option::take);
    let current = h.path.canonicalize().unwrap().display().to_string();
    assert_eq!(h.binding().output_path, current, "the hook moved the pane");
    assert!(
        !handed_off(&messages, &old),
        "[T2:withheld] a source a hook replaced during the wait must not be handed off"
    );
}

#[test]
fn codex_post_tail_with_hooks_off_installs_and_hands_off_as_before() {
    let h = std::rc::Rc::new(Harness::new());
    let _tmux = live_tmux(h.root.path());
    let _hooks = hooks_switch(Some("0"));
    let old = h.binding();
    write(&h.path, &h.header);
    let hooked = h.clone();
    crate::services::codex::AFTER_READINESS_WAIT.with_borrow_mut(|seam| {
        *seam = Some(Box::new(move || assert_eq!(hooked.hook().0, 202)));
    });
    let messages = post_tail(&h, &old);
    crate::services::codex::AFTER_READINESS_WAIT.with_borrow_mut(Option::take);
    assert!(
        handed_off(&messages, &old),
        "[T3:off] with hooks off the handoff is not checked again after the wait"
    );
    corrupt_binding_log(&h);
    let messages = post_tail(&h, &old);
    assert_eq!(
        marker_path(&h),
        PathBuf::from(&old.output_path),
        "[T3:off] with hooks off an unreadable history still installs the tail"
    );
    assert!(
        handed_off(&messages, &old),
        "[T3:off] with hooks off the installed tail is handed off"
    );
}

/// A hook moves the pane from A to B; returns A as the stale claim.
fn hooked_away(h: &Harness) -> dedupe::TuiRuntimeBinding {
    let old = h.binding();
    write(&h.path, &h.header);
    assert_eq!(h.hook().0, 202);
    old
}

#[test]
fn codex_stale_marker_and_recovery_writes_cannot_name_a_retired_source_with_hooks_on() {
    let h = Harness::new();
    let _tmux = dedupe::binding_context::tests::fake_tmux(h.root.path());
    let _hooks = hooks_switch(None);
    let old = hooked_away(&h);
    let hooked = (h.binding(), marker_path(&h));
    let stale = PathBuf::from(&old.output_path);
    session::write_codex_tui_rollout_marker_with_start_offset(
        &h.context.tmux_session,
        &stale,
        old.session_id.as_deref(),
        Some(5),
    )
    .unwrap();
    assert_eq!(
        marker_path(&h),
        hooked.1,
        "[T4:marker] a pre-tail or rebind cursor write must not move the marker back"
    );
    session::install_codex_tui_runtime_binding(&h.context.tmux_session, Some(19), old);
    assert_eq!(
        (h.binding(), marker_path(&h)),
        hooked,
        "[T4:rebind] a recovery install must not rebind the pane to a retired source"
    );
}

#[test]
fn codex_stale_marker_and_recovery_writes_with_hooks_off_behave_as_before() {
    let h = Harness::new();
    let _tmux = dedupe::binding_context::tests::fake_tmux(h.root.path());
    let _hooks = hooks_switch(Some("0"));
    let old = hooked_away(&h);
    let stale = PathBuf::from(&old.output_path);
    session::write_codex_tui_rollout_marker_with_start_offset(
        &h.context.tmux_session,
        &stale,
        old.session_id.as_deref(),
        Some(5),
    )
    .unwrap();
    assert_eq!(
        marker_path(&h),
        stale,
        "[T5:off] the marker write goes through"
    );
    let hooked = h.binding();
    session::install_codex_tui_runtime_binding(&h.context.tmux_session, Some(19), old.clone());
    assert_ne!(hooked.output_path, old.output_path);
    assert_eq!(
        h.binding().output_path,
        old.output_path,
        "[T5:off] the recovery install goes through"
    );
}
