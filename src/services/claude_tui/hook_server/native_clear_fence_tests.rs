use super::*;

// Each fence test uses a scratch runtime root and only writes its fixture marker.
fn fence_envelope(
    tmux: &str,
    channel: u64,
) -> crate::services::tui_prompt_dedupe::binding_context::HookBindingEnvelope {
    use crate::services::tui_prompt_dedupe::binding_context::*;
    let prepared =
        PreparedIncarnation::prepare("claude", tmux, Some(channel), None, false).unwrap();
    let marker = crate::services::tmux_common::session_temp_path(tmux, "spawn_nonce");
    fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    fs::write(marker, &prepared.context.execution_nonce).unwrap();
    HookBindingEnvelope {
        context: CapturedContext::Captured(prepared.context),
        observed: Default::default(),
    }
}

fn fence_hook(
    ingress: &Ingress,
    command: &str,
    payload: &Value,
    envelope: &crate::services::tui_prompt_dedupe::binding_context::HookBindingEnvelope,
) -> u16 {
    ingress
        .send_envelope(
            &format!("/hooks/claude/SessionStart?session_id={command}"),
            payload,
            Some(&uuid()),
            Some(&envelope.encode().unwrap()),
        )
        .0
}

#[test]
fn native_fence_refuses_old_nonce_and_missing_marker_without_side_effects() {
    let (_root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let (tmux, channel, a, b) = (format!("fence-{}", uuid()), 657_701, uuid(), uuid());
    ingress.pane(&tmux, channel, &a);
    let envelope = fence_envelope(&tmux, channel);
    ingress.transcript(&b);
    let original = binding_events_since(channel, 0).unwrap();
    let marker = crate::services::tmux_common::session_temp_path(&tmux, "spawn_nonce");
    for nonce in [None, Some(uuid::Uuid::new_v4().simple().to_string())] {
        if let Some(nonce) = nonce {
            fs::write(&marker, nonce).unwrap();
        } else {
            fs::remove_file(&marker).unwrap();
        }
        assert_eq!(
            fence_hook(&ingress, &a, &ingress.payload(&b, Some("clear")), &envelope),
            425
        );
        assert_eq!(binding_events_since(channel, 0).unwrap(), original);
        assert_eq!(
            runtime_binding_for_tmux_session(&tmux)
                .unwrap()
                .session_id
                .as_deref(),
            Some(a.as_str())
        );
        assert_eq!(deferred_adoption_count(), 0);
        assert_eq!(buffered(&a), 0);
    }
}

#[test]
fn native_fence_same_nonce_pending_resolves_after_transcript_arrives() {
    let (_root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let (tmux, channel, a, b) = (format!("fence-{}", uuid()), 657_702, uuid(), uuid());
    ingress.pane(&tmux, channel, &a);
    let envelope = fence_envelope(&tmux, channel);
    assert_eq!(
        fence_hook(&ingress, &a, &ingress.payload(&b, Some("clear")), &envelope),
        202
    );
    let pending = binding_events_since(channel, 0)
        .unwrap()
        .into_iter()
        .find(|e| matches!(e.new, BindingTarget::Pending { .. }))
        .unwrap();
    assert_eq!(deferred_adoption_count(), 1);
    ingress.transcript(&b);
    retry_deferred_claude_adoptions();
    let events = binding_events_since(channel, 0).unwrap();
    assert!(events.iter().any(|e| matches!(&e.new, BindingTarget::Resolved { pending_seq, source } if *pending_seq == pending.seq && source.session_id == b)));
    assert_eq!(
        runtime_binding_for_tmux_session(&tmux)
            .unwrap()
            .session_id
            .as_deref(),
        Some(b.as_str())
    );
}

#[test]
fn native_fence_deferred_retry_keeps_original_nonce() {
    let (_root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let (tmux, channel, a, b) = (format!("fence-{}", uuid()), 657_703, uuid(), uuid());
    ingress.pane(&tmux, channel, &a);
    let envelope = fence_envelope(&tmux, channel);
    assert_eq!(
        fence_hook(&ingress, &a, &ingress.payload(&b, Some("clear")), &envelope),
        202
    );
    let original = binding_events_since(channel, 0).unwrap();
    ingress.transcript(&b);
    fs::write(
        crate::services::tmux_common::session_temp_path(&tmux, "spawn_nonce"),
        uuid::Uuid::new_v4().simple().to_string(),
    )
    .unwrap();
    retry_deferred_claude_adoptions();
    assert_eq!(binding_events_since(channel, 0).unwrap(), original);
    assert_eq!(
        runtime_binding_for_tmux_session(&tmux)
            .unwrap()
            .session_id
            .as_deref(),
        Some(a.as_str())
    );
    assert_eq!(deferred_adoption_count(), 0);
}

#[test]
fn native_fence_legacy_envelopeless_continuation_still_adopts() {
    let (_root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let (tmux, channel, a, b) = (format!("fence-{}", uuid()), 657_704, uuid(), uuid());
    ingress.pane(&tmux, channel, &a);
    ingress.transcript(&b);
    assert_eq!(
        ingress.claude_hook(
            "SessionStart",
            &a,
            &ingress.payload(&b, Some("clear")),
            Some(&uuid())
        ),
        202
    );
    assert_eq!(
        runtime_binding_for_tmux_session(&tmux)
            .unwrap()
            .session_id
            .as_deref(),
        Some(b.as_str())
    );
}

#[test]
fn native_fence_launch_session_prompt_cannot_reclaim_after_nonce_changes() {
    let (_root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let (tmux, channel, a, b) = (format!("fence-{}", uuid()), 657_705, uuid(), uuid());
    ingress.pane(&tmux, channel, &a);
    let envelope = fence_envelope(&tmux, channel);
    assert_eq!(
        fence_hook(&ingress, &a, &ingress.payload(&b, Some("clear")), &envelope),
        202
    );
    fs::write(
        crate::services::tmux_common::session_temp_path(&tmux, "spawn_nonce"),
        uuid::Uuid::new_v4().simple().to_string(),
    )
    .unwrap();
    let original = binding_events_since(channel, 0).unwrap();
    let uri = format!("/hooks/claude/UserPromptSubmit?session_id={a}");
    assert_eq!(
        ingress
            .send_envelope(
                &uri,
                &ingress.payload(&a, None),
                Some(&uuid()),
                Some(&envelope.encode().unwrap())
            )
            .0,
        425
    );
    assert_eq!(binding_events_since(channel, 0).unwrap(), original);
    assert_eq!(deferred_adoption_count(), 1);
}

#[test]
fn native_fence_unreadable_marker_holds_pending_until_same_nonce_returns() {
    let (_root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let (tmux, channel, a, b) = (format!("fence-{}", uuid()), 657_709, uuid(), uuid());
    ingress.pane(&tmux, channel, &a);
    let envelope = fence_envelope(&tmux, channel);
    assert_eq!(
        fence_hook(&ingress, &a, &ingress.payload(&b, Some("clear")), &envelope),
        202
    );
    let original = binding_events_since(channel, 0).unwrap();
    let marker = crate::services::tmux_common::session_temp_path(&tmux, "spawn_nonce");
    let nonce = fs::read_to_string(&marker).unwrap();
    fs::write(&marker, "").unwrap();
    ingress.transcript(&b);
    retry_deferred_claude_adoptions();
    assert_eq!(binding_events_since(channel, 0).unwrap(), original);
    assert_eq!(deferred_adoption_count(), 1);
    fs::write(marker, nonce).unwrap();
    retry_deferred_claude_adoptions();
    assert!(binding_events_since(channel, 0).unwrap().iter().any(|event| matches!(&event.new, BindingTarget::Resolved { source, .. } if source.session_id == b)));
}

#[test]
fn native_fence_legacy_absent_capture_still_adopts() {
    let (_root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let (tmux, channel, a, b) = (format!("fence-{}", uuid()), 657_706, uuid(), uuid());
    ingress.pane(&tmux, channel, &a);
    ingress.transcript(&b);
    let envelope =
        crate::services::tui_prompt_dedupe::binding_context::HookBindingEnvelope::legacy_request();
    assert_eq!(
        fence_hook(&ingress, &a, &ingress.payload(&b, Some("clear")), &envelope),
        202
    );
    assert_eq!(
        runtime_binding_for_tmux_session(&tmux)
            .unwrap()
            .session_id
            .as_deref(),
        Some(b.as_str())
    );
}

#[test]
fn native_fence_unmapped_capture_checks_its_pane_not_empty_name() {
    let (_root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let tmux = format!("fence-{}", uuid());
    let envelope = fence_envelope(&tmux, 657_710);
    let (a, b) = (uuid(), uuid());
    ingress.transcript(&b);
    assert_eq!(
        fence_hook(&ingress, &a, &ingress.payload(&b, Some("clear")), &envelope),
        202
    );
    fs::remove_file(crate::services::tmux_common::session_temp_path(
        &tmux,
        "spawn_nonce",
    ))
    .unwrap();
    assert_eq!(
        fence_hook(&ingress, &a, &ingress.payload(&b, Some("clear")), &envelope),
        425
    );
}

#[test]
fn native_fence_marker_replacement_before_authority_refuses_observation() {
    let (_root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let (tmux, channel, a, b) = (format!("fence-{}", uuid()), 657_707, uuid(), uuid());
    ingress.pane(&tmux, channel, &a);
    let envelope = fence_envelope(&tmux, channel);
    ingress.transcript(&b);
    let original = binding_events_since(channel, 0).unwrap();
    let marker = crate::services::tmux_common::session_temp_path(&tmux, "spawn_nonce");
    let app = ingress.app.clone();
    let log_root = ingress._root.path().to_path_buf();
    let request = axum::http::Request::post(format!("/hooks/claude/SessionStart?session_id={a}"))
        .header("content-type", "application/json")
        .header(
            crate::services::tui_prompt_dedupe::binding_context::BINDING_HEADER,
            envelope.encode().unwrap(),
        )
        .body(axum::body::Body::from(
            ingress.payload(&b, Some("clear")).to_string(),
        ))
        .unwrap();
    let (contended, reached) = std::sync::mpsc::channel();
    // Hold the cutover authority until the actual receiver has tried to acquire it.
    let (worker, contention) =
        crate::services::tmux_common::with_tmux_source_authority(&tmux, |_| {
            let worker = std::thread::spawn(move || {
                set_test_root(Some(&log_root));
                crate::services::tmux_common::SOURCE_AUTHORITY_CONTENDED.with_borrow_mut(|seam| {
                    *seam = Some(Box::new(move || contended.send(()).unwrap()));
                });
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let response = runtime.block_on(app.oneshot(request)).unwrap();
                set_test_root(None);
                response.status().as_u16()
            });
            let contention = reached.recv_timeout(std::time::Duration::from_secs(10));
            fs::write(marker, uuid::Uuid::new_v4().simple().to_string()).unwrap();
            (worker, contention)
        });
    let status = worker.join().unwrap();
    contention.expect("receiver must contend on the held pane authority");
    assert_eq!(status, 425);
    assert_eq!(binding_events_since(channel, 0).unwrap(), original);
    assert_eq!(deferred_adoption_count(), 0);
}

#[test]
fn native_fence_restored_pending_retains_canonical_nonce() {
    use crate::services::tui_prompt_dedupe::pending::{self, LaunchTranscript, PendingRestore};
    let (_root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let (tmux, channel, a, b) = (format!("fence-{}", uuid()), 657_708, uuid(), uuid());
    let path = ingress.pane(&tmux, channel, &a);
    let envelope = fence_envelope(&tmux, channel);
    assert_eq!(
        fence_hook(&ingress, &a, &ingress.payload(&b, Some("clear")), &envelope),
        202
    );
    reset_deferred_adoptions_for_tests();
    reset_state_for_tests();
    let launch = LaunchTranscript {
        session_id: a.clone(),
        transcript: path,
    };
    let restored = pending::restore_claude_pane(&tmux, channel, Some(launch), |session, path| {
        claude(path, session)
    });
    assert!(
        matches!(restored, Some(PendingRestore::Seeded { .. })),
        "{restored:?}"
    );
    assert_eq!(deferred_adoption_count(), 1);
    let original = binding_events_since(channel, 0).unwrap();
    ingress.transcript(&b);
    fs::write(
        crate::services::tmux_common::session_temp_path(&tmux, "spawn_nonce"),
        uuid::Uuid::new_v4().simple().to_string(),
    )
    .unwrap();
    retry_deferred_claude_adoptions();
    assert_eq!(binding_events_since(channel, 0).unwrap(), original);
    assert_eq!(
        runtime_binding_for_tmux_session(&tmux)
            .unwrap()
            .session_id
            .as_deref(),
        Some(a.as_str())
    );
    assert_eq!(deferred_adoption_count(), 0);
}
