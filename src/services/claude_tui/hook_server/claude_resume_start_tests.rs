use super::*;
use crate::services::claude_tui::hook_server::observation_ingress::tests::Ingress;
use crate::services::tui_prompt_dedupe::binding_context::{
    self, CapturedContext, ClaudeResumeSource, HookBindingEnvelope, PreparedIncarnation,
};
use crate::services::tui_prompt_dedupe::binding_events::{
    BindingCause, BindingTarget, CauseSource, Proposal,
};

const CHANNEL: u64 = 7_503_301;

fn setup(ingress: &mut Ingress) -> (BindingContext, Value) {
    let logical = format!("resume-start-{}", uuid::Uuid::new_v4().simple());
    let sid = uuid::Uuid::new_v4().to_string();
    let old_nonce = uuid::Uuid::new_v4().simple().to_string();
    let marker = crate::services::tmux_common::session_temp_path(&logical, "spawn_nonce");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(&marker, &old_nonce).unwrap();
    let path = ingress.pane(&logical, CHANNEL, &sid);
    let source =
        crate::services::tui_o::shadow::binding_reader::source_id_for(&sid, &path).unwrap();
    let path_text = path.to_str().unwrap();
    let hook = HookSignal::from_payload("session_start", &ingress.payload(&sid, Some("startup")));
    let proposal = Proposal {
        channel_id: CHANNEL,
        provider: "claude",
        tmux_session: &logical,
        session_id: Some(&sid),
        path: path_text,
        replaced: None,
        cause: CauseSource::Hook(BindingCause::Startup),
        hook: Some(&hook),
    };
    binding_events::record_verified(&proposal, &source).unwrap();
    let prepared =
        PreparedIncarnation::prepare("claude", &logical, Some(CHANNEL), Some(&sid), true).unwrap();
    let mut context = prepared.context;
    context.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
    context.provider_root = Some(path.parent().unwrap().to_path_buf());
    context.claude_resume = Some(ClaudeResumeSource {
        source,
        previous_nonce: old_nonce,
    });
    let context = PreparedIncarnation::create(context).unwrap().context;
    std::fs::write(marker, &context.execution_nonce).unwrap();
    ingress.enable_resume_start(context.clone());
    (context, ingress.payload(&sid, Some("resume")))
}

fn envelope(context: &BindingContext) -> String {
    HookBindingEnvelope {
        context: CapturedContext::Captured(context.clone()),
        observed: Default::default(),
    }
    .encode()
    .unwrap()
}

fn send(ingress: &Ingress, context: &BindingContext, payload: &Value) -> u16 {
    let uri = format!(
        "/hooks/claude/SessionStart?session_id={}",
        context.expected_native_session_id.as_deref().unwrap()
    );
    ingress
        .send_envelope(
            &uri,
            payload,
            Some(&uuid::Uuid::new_v4().to_string()),
            Some(&envelope(context)),
        )
        .0
}

#[test]
fn r4_same_source_start_rebinds_exactly_once_to_the_new_nonce() {
    let (_root, _env) = binding_context::tests::fixture();
    let mut ingress = Ingress::new();
    let (context, payload) = setup(&mut ingress);
    assert_eq!(send(&ingress, &context, &payload), 202);
    let records = binding_events::records_strict_verified(CHANNEL)
        .unwrap()
        .unwrap();
    let starts: Vec<_> = records
        .iter()
        .filter(|record| {
            record.verified
                && record.event.execution_nonce.as_deref() == Some(context.execution_nonce.as_str())
                && record.event.cause == BindingCause::Resume
                && record.event.evidence.hook_event.as_deref() == Some("session_start")
        })
        .collect();
    assert_eq!(
        starts.len(),
        1,
        "ACK requires one durable same-source Resume of the new nonce"
    );
    assert!(
        matches!(&starts[0].event.new, BindingTarget::Source(source) if Some(source) == context.claude_resume.as_ref().map(|resume| &resume.source))
    );
    assert_eq!(send(&ingress, &context, &payload), 202);
    assert_eq!(
        binding_events::records_strict_verified(CHANNEL)
            .unwrap()
            .unwrap()
            .len(),
        records.len()
    );
}

#[test]
fn r11_durable_start_is_consumed_after_ack_and_writer_restart() {
    let (_root, _env) = binding_context::tests::fixture();
    let mut ingress = Ingress::new();
    let (context, payload) = setup(&mut ingress);
    assert_eq!(send(&ingress, &context, &payload), 202);
    binding_events::forget_channel_for_tests(CHANNEL);
    let restored = binding_context::execution_context("claude", &context.execution_nonce).unwrap();
    assert_eq!(
        binding_events::claude_resume_start::started_source(&restored).unwrap(),
        Some(restored.claude_resume.as_ref().unwrap().source.clone()),
        "Pending restart must recover the ACKed start without another hook or create"
    );
}

#[test]
fn r5_missing_original_context_does_not_ack_a_managed_resume_start() {
    let (_root, _env) = binding_context::tests::fixture();
    let mut ingress = Ingress::new();
    let (context, payload) = setup(&mut ingress);
    let uri = format!(
        "/hooks/claude/SessionStart?session_id={}",
        context.expected_native_session_id.unwrap()
    );
    assert_eq!(
        ingress
            .send(&uri, &payload, Some(&uuid::Uuid::new_v4().to_string()))
            .0,
        425,
        "a known resume cannot replace its original immutable context with a current marker"
    );
}

#[test]
fn r11_sync_failure_refuses_ack_and_the_exact_request_retries_once() {
    use binding_events::{APPEND_FAULT, FileSyncPoint, with_file_sync_observer};
    use std::sync::{Arc, Mutex};
    let (_root, _env) = binding_context::tests::fixture();
    let mut ingress = Ingress::new();
    let (context, payload) = setup(&mut ingress);
    let uri = format!(
        "/hooks/claude/SessionStart?session_id={}",
        context.expected_native_session_id.as_deref().unwrap()
    );
    let id = uuid::Uuid::new_v4().to_string();
    let encoded = envelope(&context);
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    APPEND_FAULT.set(Some("sync"));
    let refused = with_file_sync_observer(
        Arc::new(move |point| captured.lock().unwrap().push(point)),
        || {
            ingress
                .send_envelope(&uri, &payload, Some(&id), Some(&encoded))
                .0
        },
    );
    APPEND_FAULT.set(None);
    assert_eq!(refused, 425, "failed file sync cannot ACK a resume start");
    assert_eq!(*events.lock().unwrap(), [FileSyncPoint::Entered("sync")]);
    assert!(
        binding_events::records_strict_verified(CHANNEL)
            .unwrap()
            .unwrap()
            .iter()
            .all(|record| record.event.execution_nonce.as_deref()
                != Some(context.execution_nonce.as_str()))
    );
    let captured = events.clone();
    assert_eq!(
        with_file_sync_observer(
            Arc::new(move |point| captured.lock().unwrap().push(point)),
            || {
                ingress
                    .send_envelope(&uri, &payload, Some(&id), Some(&encoded))
                    .0
            }
        ),
        202
    );
    assert_eq!(
        *events.lock().unwrap(),
        [
            FileSyncPoint::Entered("sync"),
            FileSyncPoint::Entered("sync"),
            FileSyncPoint::Completed("sync")
        ]
    );
    assert_eq!(
        binding_events::records_strict_verified(CHANNEL)
            .unwrap()
            .unwrap()
            .iter()
            .filter(|record| record.event.execution_nonce.as_deref()
                == Some(context.execution_nonce.as_str()))
            .count(),
        1
    );
}

#[test]
fn r11_ack_waits_until_the_entered_file_sync_operation_succeeds() {
    use binding_events::{FileSyncPoint, with_file_sync_observer};
    use std::sync::{Arc, Barrier, Mutex, mpsc};
    use tower::ServiceExt;
    let (_root, _env) = binding_context::tests::fixture();
    let mut ingress = Ingress::new();
    let (context, payload) = setup(&mut ingress);
    let app = ingress.router_for_tests();
    let events_root = binding_events::test_root().unwrap();
    let uri = format!(
        "/hooks/claude/SessionStart?session_id={}",
        context.expected_native_session_id.as_deref().unwrap()
    );
    let request = axum::http::Request::post(uri)
        .header("content-type", "application/json")
        .header(binding_context::BINDING_HEADER, envelope(&context))
        .body(axum::body::Body::from(payload.to_string()))
        .unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (ack_tx, ack_rx) = mpsc::channel();
    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = barrier.clone();
    let events = Arc::new(Mutex::new(Vec::new()));
    let worker_events = events.clone();
    let worker = std::thread::spawn(move || {
        binding_events::set_test_root(Some(&events_root));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let response = with_file_sync_observer(
            Arc::new(move |point| {
                worker_events.lock().unwrap().push(point);
                if point == FileSyncPoint::Entered("sync") {
                    entered_tx.send(()).unwrap();
                    worker_barrier.wait();
                }
            }),
            || runtime.block_on(app.oneshot(request)).unwrap(),
        );
        ack_tx.send(response.status().as_u16()).unwrap();
        binding_events::set_test_root(None);
    });
    let entered = entered_rx.recv_timeout(std::time::Duration::from_secs(5));
    if entered.is_err() {
        worker.join().unwrap();
        panic!("sync operation was never entered before ACK: {entered:?}");
    }
    let premature = ack_rx.try_recv();
    barrier.wait();
    worker.join().unwrap();
    assert!(
        matches!(premature, Err(mpsc::TryRecvError::Empty)),
        "ACK escaped before file sync: {premature:?}"
    );
    assert_eq!(ack_rx.recv().unwrap(), 202);
    assert_eq!(
        *events.lock().unwrap(),
        [
            FileSyncPoint::Entered("sync"),
            FileSyncPoint::Completed("sync")
        ]
    );
}

#[test]
fn r11_complete_start_line_requires_successful_reload_sync_before_consumption() {
    use binding_events::{APPEND_FAULT, FileSyncPoint, with_file_sync_observer};
    use std::sync::{Arc, Mutex};
    let (_root, _env) = binding_context::tests::fixture();
    let mut ingress = Ingress::new();
    let (context, _) = setup(&mut ingress);
    let path = binding_events::test_root()
        .unwrap()
        .join(binding_events::BINDING_EVENTS_DIR)
        .join(format!("{CHANNEL}.log"));
    let text = std::fs::read_to_string(&path).unwrap();
    let mut line: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    line["seq"] = json!(line["seq"].as_u64().unwrap() + 1);
    line["execution_nonce"] = json!(context.execution_nonce);
    line["cause"] = json!("resume");
    line["evidence"]["hook_event"] = json!("session_start");
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{line}").unwrap();
    }
    binding_events::forget_channel_for_tests(CHANNEL);
    APPEND_FAULT.set(Some("reload"));
    assert!(
        binding_events::claude_resume_start::started_source(&context).is_err(),
        "a complete line is not a durable restart approval"
    );
    APPEND_FAULT.set(None);
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    let recovered = with_file_sync_observer(
        Arc::new(move |point| captured.lock().unwrap().push(point)),
        || binding_events::claude_resume_start::started_source(&context).unwrap(),
    );
    assert_eq!(
        recovered,
        Some(context.claude_resume.as_ref().unwrap().source.clone())
    );
    assert_eq!(
        *events.lock().unwrap(),
        [
            FileSyncPoint::Entered("reload"),
            FileSyncPoint::Completed("reload")
        ]
    );
}

#[test]
fn r5_invalid_start_context_reason_and_source_never_append() {
    let (_root, _env) = binding_context::tests::fixture();
    let mut ingress = Ingress::new();
    let (context, payload) = setup(&mut ingress);
    let initial = binding_events::records_strict_verified(CHANNEL)
        .unwrap()
        .unwrap()
        .len();
    for field in [
        "execution_nonce",
        "provider",
        "owner_runtime_root",
        "tmux_session",
        "channel_id",
        "expected_native_session_id",
        "provider_root",
    ] {
        let mut value = serde_json::to_value(&context).unwrap();
        value[field] = if field == "channel_id" {
            json!(CHANNEL + 1)
        } else {
            json!("wrong")
        };
        let changed: BindingContext = serde_json::from_value(value).unwrap();
        let uri = format!(
            "/hooks/claude/SessionStart?session_id={}",
            context.expected_native_session_id.as_deref().unwrap()
        );
        assert_eq!(
            ingress
                .send_envelope(
                    &uri,
                    &payload,
                    Some(&uuid::Uuid::new_v4().to_string()),
                    Some(&envelope(&changed))
                )
                .0,
            425,
            "bad original context {field}"
        );
    }
    for field in ["source", "session_id", "transcript_path"] {
        let mut changed = payload.clone();
        changed[field] = json!("wrong");
        assert_eq!(
            send(&ingress, &context, &changed),
            425,
            "bad resume payload {field}"
        );
    }
    assert_eq!(
        binding_events::records_strict_verified(CHANNEL)
            .unwrap()
            .unwrap()
            .len(),
        initial
    );
    assert_eq!(send(&ingress, &context, &payload), 202);
}

#[test]
fn r4_a_colliding_nonce_or_changed_old_nonce_is_not_a_resume_baseline() {
    for collision in [false, true] {
        let (_root, _env) = binding_context::tests::fixture();
        let mut ingress = Ingress::new();
        let (context, payload) = setup(&mut ingress);
        let path = binding_events::test_root()
            .unwrap()
            .join(binding_events::BINDING_EVENTS_DIR)
            .join(format!("{CHANNEL}.log"));
        let text = std::fs::read_to_string(&path).unwrap();
        let mut records: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        if collision {
            records[0]["execution_nonce"] = json!(context.execution_nonce);
        } else {
            records.last_mut().unwrap()["execution_nonce"] =
                json!(uuid::Uuid::new_v4().simple().to_string());
        }
        std::fs::write(
            &path,
            records
                .iter()
                .map(|record| format!("{record}\n"))
                .collect::<String>(),
        )
        .unwrap();
        binding_events::forget_channel_for_tests(CHANNEL);
        assert_eq!(
            send(&ingress, &context, &payload),
            425,
            "collision={collision}"
        );
    }
}

#[test]
fn resumed_admitted_execution_can_clear_and_late_old_start_stays_refused() {
    for reason in ["clear", "fork", "compact"] {
        let (_root, _env) = binding_context::tests::fixture();
        let mut ingress = Ingress::new();
        let (context, payload) = setup(&mut ingress);
        assert_eq!(send(&ingress, &context, &payload), 202);
        crate::services::tui_prompt_dedupe::admit_herdr_execution(
            &context.tmux_session,
            &context.execution_nonce,
        );
        let next_sid = uuid::Uuid::new_v4().to_string();
        let next = ingress.payload(&next_sid, Some(reason));
        let uri = format!(
            "/hooks/claude/SessionStart?session_id={}",
            context.expected_native_session_id.as_deref().unwrap()
        );
        assert_eq!(
            ingress
                .send_envelope(
                    &uri,
                    &next,
                    Some(&uuid::Uuid::new_v4().to_string()),
                    Some(&envelope(&context))
                )
                .0,
            202,
            "resumed admission must preserve native {reason}"
        );
        let records = binding_events::records_strict_verified(CHANNEL)
            .unwrap()
            .unwrap();
        assert!(
            matches!(&records.last().unwrap().event.new, BindingTarget::Pending { payload_session_id, .. } if payload_session_id == &next_sid),
            "native {reason} must reach its actual adoption, not merely ACK"
        );
        assert_eq!(
            records.last().unwrap().event.execution_nonce.as_deref(),
            Some(context.execution_nonce.as_str())
        );
        let mut old = context.clone();
        old.execution_nonce = context
            .claude_resume
            .as_ref()
            .unwrap()
            .previous_nonce
            .clone();
        assert_eq!(
            ingress
                .send_envelope(
                    &uri,
                    &payload,
                    Some(&uuid::Uuid::new_v4().to_string()),
                    Some(&envelope(&old))
                )
                .0,
            425
        );
        assert!(binding_events::claude_resume_start::started_source(&context).is_err());
    }
}

#[test]
fn resumed_hook_wait_consumes_only_the_exact_started_context() {
    let (_root, _env) = binding_context::tests::fixture();
    let mut ingress = Ingress::new();
    let (context, payload) = setup(&mut ingress);
    assert_eq!(send(&ingress, &context, &payload), 202);
    let (events, mut receiver) = tokio::sync::broadcast::channel(4);
    let valid = HookEvent {
        provider: "claude".into(),
        kind: HookEventKind::SessionStart,
        session_id: context.expected_native_session_id.clone().unwrap(),
        payload,
        original_context: Some(context.clone()),
        ..Default::default()
    };
    let mut old = valid.clone();
    old.original_context = None;
    events.send(old).unwrap();
    events.send(valid).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    crate::services::claude::herdr_turn::resumed_session_started(
        runtime.handle(),
        &mut receiver,
        &context,
        None,
    )
    .unwrap();
    assert!(
        matches!(
            receiver.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "an old hook cannot wake the resume waiter"
    );
}

#[test]
fn an_old_context_without_resume_source_keeps_the_launch_hook_compatibility() {
    let (_root, _env) = binding_context::tests::fixture();
    let ingress = Ingress::new();
    let sid = uuid::Uuid::new_v4().to_string();
    let logical = format!("legacy-resume-{}", uuid::Uuid::new_v4().simple());
    ingress.pane(&logical, CHANNEL, &sid);
    let context =
        PreparedIncarnation::prepare("claude", &logical, Some(CHANNEL), Some(&sid), false)
            .unwrap()
            .context;
    let encoded = serde_json::to_value(&context).unwrap();
    assert!(encoded.get("claude_resume").is_none());
    let restored: BindingContext = serde_json::from_value(encoded).unwrap();
    assert!(restored.claude_resume.is_none());
    let uri = format!("/hooks/claude/SessionStart?session_id={sid}");
    assert_eq!(
        ingress
            .send(
                &uri,
                &ingress.payload(&sid, Some("startup")),
                Some(&uuid::Uuid::new_v4().to_string())
            )
            .0,
        202
    );
}

fn complete_native_clear(ingress: &Ingress, context: &BindingContext) -> String {
    let next_sid = uuid::Uuid::new_v4().to_string();
    let payload = ingress.payload(&next_sid, Some("clear"));
    let uri = format!(
        "/hooks/claude/SessionStart?session_id={}",
        context.expected_native_session_id.as_deref().unwrap()
    );
    assert_eq!(
        ingress
            .send_envelope(
                &uri,
                &payload,
                Some(&uuid::Uuid::new_v4().to_string()),
                Some(&envelope(context))
            )
            .0,
        202
    );
    let records = binding_events::records_strict_verified(CHANNEL)
        .unwrap()
        .unwrap();
    assert!(
        matches!(&records.last().unwrap().event.new, BindingTarget::Pending { payload_session_id, .. } if payload_session_id == &next_sid)
    );
    let path = ingress.transcript(&next_sid);
    let source =
        crate::services::tui_o::shadow::binding_reader::source_id_for(&next_sid, &path).unwrap();
    let hook = HookSignal::from_payload("session_start", &payload);
    let proposal = Proposal {
        channel_id: CHANNEL,
        provider: "claude",
        tmux_session: &context.tmux_session,
        session_id: Some(&next_sid),
        path: path.to_str().unwrap(),
        replaced: None,
        cause: CauseSource::Hook(BindingCause::Clear),
        hook: Some(&hook),
    };
    binding_events::record_verified(&proposal, &source).unwrap();
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        &context.tmux_session,
        crate::services::claude_tui::hook_server::observation_ingress::tests::claude(
            &path, &next_sid,
        ),
    );
    assert_eq!(
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&context.tmux_session)
            .unwrap()
            .session_id
            .as_deref(),
        Some(next_sid.as_str())
    );
    assert!(matches!(
        binding_events::records_strict_verified(CHANNEL)
            .unwrap()
            .unwrap()
            .last()
            .unwrap()
            .event
            .new,
        BindingTarget::Resolved { .. }
    ));
    next_sid
}

#[test]
fn admitted_lifecycle_uses_historical_start_after_the_origin_is_deleted_or_replaced() {
    for replaced in [false, true] {
        for reason in ["fork", "compact"] {
            let (_root, _env) = binding_context::tests::fixture();
            let mut ingress = Ingress::new();
            let (context, payload) = setup(&mut ingress);
            assert_eq!(send(&ingress, &context, &payload), 202);
            crate::services::tui_prompt_dedupe::admit_herdr_execution(
                &context.tmux_session,
                &context.execution_nonce,
            );
            complete_native_clear(&ingress, &context);
            let origin = &context.claude_resume.as_ref().unwrap().source.path;
            if replaced {
                std::fs::rename(origin, origin.with_extension("retained")).unwrap();
                std::fs::write(origin, "replacement\n").unwrap();
            } else {
                std::fs::remove_file(origin).unwrap();
            }
            assert!(
                binding_events::claude_resume_start::context_source(&context).is_err(),
                "fixture must invalidate the origin's current file identity"
            );
            let next_sid = uuid::Uuid::new_v4().to_string();
            let next = ingress.payload(&next_sid, Some(reason));
            let uri = format!(
                "/hooks/claude/SessionStart?session_id={}",
                context.expected_native_session_id.as_deref().unwrap()
            );
            assert_eq!(
                ingress
                    .send_envelope(
                        &uri,
                        &next,
                        Some(&uuid::Uuid::new_v4().to_string()),
                        Some(&envelope(&context))
                    )
                    .0,
                202,
                "normal {reason} after clear must use the current source"
            );
            assert!(
                binding_events::claude_resume_start::start_recorded(&context).unwrap(),
                "historical start proof must survive origin replacement/deletion"
            );
            let records = binding_events::records_strict_verified(CHANNEL)
                .unwrap()
                .unwrap();
            assert!(
                matches!(&records.last().unwrap().event.new, BindingTarget::Pending { payload_session_id, .. } if payload_session_id == &next_sid)
            );
            let mut old = context.clone();
            old.execution_nonce = context
                .claude_resume
                .as_ref()
                .unwrap()
                .previous_nonce
                .clone();
            assert_eq!(
                ingress
                    .send_envelope(
                        &uri,
                        &next,
                        Some(&uuid::Uuid::new_v4().to_string()),
                        Some(&envelope(&old))
                    )
                    .0,
                425
            );
            assert_eq!(
                ingress
                    .send(&uri, &next, Some(&uuid::Uuid::new_v4().to_string()))
                    .0,
                425
            );
        }
    }
}

#[test]
fn admitted_interactive_resume_can_return_to_the_launch_sid_after_native_clear() {
    let (_root, _env) = binding_context::tests::fixture();
    let mut ingress = Ingress::new();
    let (context, payload) = setup(&mut ingress);
    assert_eq!(send(&ingress, &context, &payload), 202);
    crate::services::tui_prompt_dedupe::admit_herdr_execution(
        &context.tmux_session,
        &context.execution_nonce,
    );
    complete_native_clear(&ingress, &context);
    let before = binding_events::records_strict_verified(CHANNEL)
        .unwrap()
        .unwrap()
        .len();
    let uri = format!(
        "/hooks/claude/SessionStart?session_id={}",
        context.expected_native_session_id.as_deref().unwrap()
    );
    let (status, body) = ingress.send_envelope(
        &uri,
        &payload,
        Some(&uuid::Uuid::new_v4().to_string()),
        Some(&envelope(&context)),
    );
    assert_eq!(
        status, 202,
        "same-nonce interactive resume is not a second migration start"
    );
    assert_eq!(
        body["binding_observation"], "Proceed(NoSessionSwitch)",
        "subsequent SessionStart must reach the ordinary lifecycle path"
    );
    assert_eq!(
        binding_events::records_strict_verified(CHANNEL)
            .unwrap()
            .unwrap()
            .len(),
        before
    );
    assert!(binding_events::claude_resume_start::start_recorded(&context).unwrap());
}

#[test]
fn resumed_start_preserves_published_at_metadata_and_accepts_its_absence() {
    use tower::ServiceExt;
    let now = chrono::Utc::now();
    for published in [None, Some(now.to_rfc3339())] {
        let (_root, _env) = binding_context::tests::fixture();
        let mut ingress = Ingress::new();
        let (context, payload) = setup(&mut ingress);
        let uri = format!(
            "/hooks/claude/SessionStart?session_id={}",
            context.expected_native_session_id.as_deref().unwrap()
        );
        let mut request = axum::http::Request::post(uri)
            .header("content-type", "application/json")
            .header(binding_context::BINDING_HEADER, envelope(&context));
        if let Some(published) = published.as_deref() {
            request = request
                .header(
                    super::super::relay_receipts::RELAY_PUBLISHED_AT_HEADER,
                    published,
                )
                .header(
                    super::super::relay_receipts::RELAY_REQUEST_ID_HEADER,
                    uuid::Uuid::new_v4().to_string(),
                )
                .header(
                    super::super::relay_receipts::RELAY_DEADLINE_HEADER,
                    (now + chrono::Duration::minutes(5)).to_rfc3339(),
                );
        }
        let request = request
            .body(axum::body::Body::from(payload.to_string()))
            .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(
            runtime
                .block_on(ingress.router_for_tests().oneshot(request))
                .unwrap()
                .status()
                .as_u16(),
            202
        );
        let records = binding_events::records_strict_verified(CHANNEL)
            .unwrap()
            .unwrap();
        let start = records.last().unwrap();
        assert_eq!(
            start.event.execution_nonce.as_deref(),
            Some(context.execution_nonce.as_str())
        );
        assert_eq!(
            start.published_at,
            published.map(|value| chrono::DateTime::parse_from_rfc3339(&value)
                .unwrap()
                .with_timezone(&chrono::Utc))
        );
    }
}
