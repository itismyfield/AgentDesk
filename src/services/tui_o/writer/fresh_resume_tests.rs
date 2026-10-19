use super::*;
use crate::services::tui_o::writer::binding::ChannelBindingLog;

pub(crate) fn proven(root: &Path) -> (BindingContext, SourceId) {
    let mut context = context(root, CANARY_TMUX, "verified");
    context.host = dedupe::binding_context::stable_host_identity();
    PreparedIncarnation::create(context.clone()).unwrap();
    publish_nonce(&context);
    dedupe::register_tmux_channel(CANARY_TMUX, CANARY_CHANNEL);
    let id = uuid::Uuid::new_v4().to_string();
    let path = context
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{id}.jsonl"));
    let at = (context.created_at + chrono::Duration::seconds(1)).to_rfc3339();
    let meta = serde_json::json!({"type":"session_meta", "timestamp":at,
        "payload":{"id":id,"timestamp":at,"cwd":root,"source":"cli","originator":"codex_cli_rs"}});
    std::fs::write(&path, format!("{meta}\n")).unwrap();
    dedupe::set_codex_delivery_permission_for_tests(
        &context,
        dedupe::CodexDeliveryPermissionForTests::Allowed,
    );
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(context.clone()),
        observed: ObservedHookProcess::default(),
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    let ingress =
        crate::services::claude_tui::hook_server::observation_ingress::observe_binding_hook(
            "codex",
            "session_start",
            Some(&id),
            Some(&id),
            &serde_json::json!({"session_id":id,"transcript_path":path,"source":"startup"}),
            &headers,
        );
    let fold = dedupe::binding_events::codex::read_ownership(&context).unwrap();
    let source = fold
        .verified
        .as_ref()
        .unwrap_or_else(|| panic!("proof: {ingress:?}, {fold:?}"))
        .source
        .clone();
    assert!(dedupe::codex_verified_channel_delivery_allowed(
        CANARY_CHANNEL
    ));
    assert!(dedupe::codex_verified_o_source_allowed(
        CANARY_CHANNEL,
        &source
    ));
    (context, source)
}

fn answer(source: &SourceId, id: &str) {
    for line in [
        output_line(
            "event_msg",
            serde_json::json!({"type":"task_started","turn_id":id}),
        ),
        output_line(
            "response_item",
            serde_json::json!({"type":"message","role":"assistant","id":id,
            "content":[{"type":"output_text","text":id}]}),
        ),
        output_line(
            "event_msg",
            serde_json::json!({"type":"task_complete","turn_id":id}),
        ),
    ] {
        append(&source.path, &line);
    }
}

async fn scenario(in_place: bool) {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe_lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _fake_tmux =
        crate::services::provider_teardown::tests::test_support::FakeTmux::install(CANARY_TMUX);
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    let (_, n2) = proven(root.path());
    let harness = Harness::build_channel(CANARY_CHANNEL, |_| {
        vec![InitSource {
            source_id: n2.clone(),
            delivery_start: std::fs::metadata(&n2.path).unwrap().len(),
            prefix_hash: hex::encode(Sha256::digest(std::fs::read(&n2.path).unwrap())),
        }]
    });
    harness.gate.acquired();
    let checkpoint = dedupe::binding_events::binding_events_since(CANARY_CHANNEL, 0)
        .unwrap()
        .last()
        .unwrap()
        .seq;
    harness
        .channel()
        .set_binding_checkpoint(checkpoint)
        .unwrap();
    let before = harness.channel().cursor(&n2).unwrap().clone();
    let bytes = std::fs::read(&n2.path).unwrap();
    let (stop, stopped) = watch::channel(false);
    let (resumed, ready) = watch::channel(false);
    let make_n3 = || proven(root.path());
    let n3 = (!in_place).then(make_n3);
    let mut n2_io = (!in_place).then(|| crate::services::codex::actual_submit_tests::source_io::Watch::start(&n2.path));
    let actor = tokio::spawn(run_channel(
        harness.writer(),
        ShadowProvider::Codex,
        Arc::new(ChannelBindingLog::new(
            CANARY_CHANNEL,
            ShadowProvider::Codex,
        )),
        stopped,
        resumed,
    ));
    polls(3).await;
    if in_place {
        assert!(*ready.borrow(), "N2 actor starts fully resumed");
    }
    let (_, n3) = n3.unwrap_or_else(make_n3);
    n2_io.get_or_insert_with(|| crate::services::codex::actual_submit_tests::source_io::Watch::start(&n2.path));
    assert!(!dedupe::codex_verified_o_source_allowed(
        CANARY_CHANNEL,
        &n2
    ));
    answer(&n3, "Q1");
    polls(6).await;
    assert!(*ready.borrow(), "fresh proof restores the scoped writer");
    assert_eq!(
        harness.port.posts(),
        ["Q1"],
        "O2/O2-W: a fresh source posts behind a denied old cursor"
    );
    let n3_before = harness.channel().cursor(&n3).unwrap().clone();
    let n3_bytes = std::fs::read(&n3.path).unwrap();
    let n3_rotation = harness.channel().rotation().unwrap().link(&n3).cloned();
    let n3_io = crate::services::codex::actual_submit_tests::source_io::Watch::start(&n3.path);
    let (_, n4) = proven(root.path());
    answer(&n4, "Q2");
    polls(6).await;
    assert_eq!(
        harness.port.posts(),
        ["Q1", "Q2"],
        "O7-2X: one actor resumes the next exact incarnation"
    );
    assert_eq!(harness.channel().cursor(&n2).unwrap(), &before);
    assert_eq!(harness.channel().cursor(&n3).unwrap(), &n3_before);
    assert_eq!(std::fs::read(&n2.path).unwrap(), bytes);
    assert_eq!(std::fs::read(&n3.path).unwrap(), n3_bytes);
    assert_eq!(harness.channel().rotation().unwrap().link(&n3), n3_rotation.as_ref());
    assert_eq!(n2_io.unwrap().stats().read_bytes, 0, "suspended N2 bytes were read");
    assert_eq!(n3_io.stats().read_bytes, 0, "suspended N3 bytes were read");
    stop.send(true).unwrap();
    assert_eq!(actor.await.unwrap(), None);
    dedupe::binding_events::forget_channel_for_tests(CANARY_CHANNEL);
    dedupe::binding_events::set_test_root(None);
    dedupe::reset_state_for_tests();
}

#[tokio::test(start_paused = true)]
async fn fresh_startup_proof_resumes_with_old_cursor_untouched() {
    if crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::fresh_startup_proof_resumes_with_old_cursor_untouched"
    )) {
        scenario(false).await;
    }
}

#[tokio::test(start_paused = true)]
async fn same_actor_scopes_two_fresh_incarnations_without_clear_or_restart() {
    if crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::same_actor_scopes_two_fresh_incarnations_without_clear_or_restart"
    )) {
        scenario(true).await;
    }
}
