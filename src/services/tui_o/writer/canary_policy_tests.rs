use super::*;

#[path = "operator_resume_source_tests.rs"]
mod operator_resume;
use crate::services::tui_prompt_dedupe::{
    self as dedupe,
    binding_context::{
        BINDING_HEADER, BindingContext, CapturedContext, HookBindingEnvelope, ObservedHookProcess,
        PreparedIncarnation,
    },
};

const CANARY_CHANNEL: u64 = crate::services::codex_tui::canary::CANARY_CHANNEL;
const CANARY_TMUX: &str = crate::services::codex_tui::canary::CANARY_TMUX;
const ID: &str = "019e660d-4859-7522-9cee-8ba7c4e7c743";

fn context(root: &Path, tmux: &str, policy: &str) -> BindingContext {
    BindingContext {
        schema: 1,
        provider: "codex".into(),
        created_at: Utc::now(),
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        tmux_session: tmux.into(),
        channel_id: Some(CANARY_CHANNEL),
        owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
        host: None,
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: Some(root.canonicalize().unwrap()),
        first_prompt_digest: None,
        source_policy: Some(policy.into()),
    }
}

fn publish_nonce(context: &BindingContext) {
    let marker =
        crate::services::tmux_common::session_temp_path(&context.tmux_session, "spawn_nonce");
    std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, &context.execution_nonce).unwrap();
}

fn durable_canary_without_launch_files(root: &Path) -> BindingContext {
    let context = context(root, CANARY_TMUX, "verified");
    let prepared = PreparedIncarnation::create(context.clone()).unwrap();
    publish_nonce(&context);
    dedupe::register_tmux_channel(CANARY_TMUX, CANARY_CHANNEL);
    let path = context
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{ID}.jsonl"));
    let timestamp = (context.created_at + chrono::Duration::seconds(1)).to_rfc3339();
    let meta = serde_json::json!({"type":"session_meta", "timestamp":timestamp,
        "payload":{"id":ID,"timestamp":timestamp,"cwd":root,"source":"cli","originator":"codex_cli_rs"}});
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
            Some(ID),
            Some(ID),
            &serde_json::json!({"session_id":ID,"transcript_path":path,"source":"startup"}),
            &headers,
        );
    let fold = dedupe::binding_events::codex::read_ownership(&context).unwrap();
    assert!(
        fold.verified.is_some(),
        "durable proof required: {ingress:?}, {fold:?}"
    );
    assert!(dedupe::runtime_binding_for_tmux_session(CANARY_TMUX).is_some());
    std::fs::remove_file(prepared.path).unwrap();
    std::fs::remove_file(crate::services::tmux_common::session_temp_path(
        CANARY_TMUX,
        crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
    ))
    .unwrap();
    std::fs::remove_file(crate::services::tmux_common::session_temp_path(
        CANARY_TMUX,
        "spawn_nonce",
    ))
    .unwrap();
    dedupe::reset_state_for_tests();
    dedupe::binding_events::forget_channel_for_tests(CANARY_CHANNEL);
    assert!(dedupe::codex_verified_requires_proof(CANARY_TMUX));
    assert_eq!(
        dedupe::codex_verified_hold_reason(CANARY_TMUX),
        "verified_unavailable"
    );
    assert_eq!(
        dedupe::binding_events::codex::read_ownership(&context).unwrap(),
        fold
    );
    context
}

fn output_line(kind: &str, payload: serde_json::Value) -> Vec<u8> {
    format!("{}\n", serde_json::json!({"type":kind,"payload":payload})).into_bytes()
}

#[tokio::test(start_paused = true)]
async fn canary_alternate_legacy_and_herdr_owner_actor_capture_and_post() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::canary_alternate_legacy_and_herdr_owner_actor_capture_and_post"
    )) {
        return;
    }
    exercise_actor(true).await;
}

#[tokio::test(start_paused = true)]
async fn canary_without_authoritative_owner_actor_preserves_durable_unavailable_hold() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::canary_without_authoritative_owner_actor_preserves_durable_unavailable_hold"
    )) {
        return;
    }
    exercise_actor(false).await;
}

async fn exercise_actor(alternate_owner: bool) {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe_lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _fake_tmux =
        crate::services::provider_teardown::tests::test_support::FakeTmux::install(CANARY_TMUX);
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    let old_context = durable_canary_without_launch_files(root.path());
    let old_fold = dedupe::binding_events::codex::read_ownership(&old_context).unwrap();
    for herdr in [false, true] {
        dedupe::reset_state_for_tests();
        let logical = format!("alternate-codex-{}", uuid::Uuid::new_v4().simple());
        if alternate_owner {
            let alternate = context(root.path(), &logical, "legacy");
            PreparedIncarnation::create(alternate.clone()).unwrap();
            publish_nonce(&alternate);
            if herdr {
                dedupe::register_codex_herdr_placeholder(&logical, CANARY_CHANNEL);
            } else {
                dedupe::register_tmux_channel(&logical, CANARY_CHANNEL);
            }
            assert!(!dedupe::codex_verified_requires_proof(&logical));
        }
        let mut native = None;
        let harness = Harness::build_channel(CANARY_CHANNEL, |runtime| {
            let path = runtime.join("alternate.jsonl");
            std::fs::write(&path, b"").unwrap();
            let source = source_id_for("alternate-session", &path).unwrap();
            native = Some((path, source.clone()));
            vec![InitSource {
                source_id: source,
                delivery_start: 0,
                prefix_hash: hex::encode(Sha256::digest(b"")),
            }]
        });
        let (path, source) = native.unwrap();
        assert_eq!(
            dedupe::codex_verified_channel_delivery_allowed(CANARY_CHANNEL),
            alternate_owner
        );
        assert_eq!(
            dedupe::codex_verified_o_source_allowed(CANARY_CHANNEL, &source),
            alternate_owner
        );
        let bindings = Arc::new(FakeBindings::new());
        let mut event = bound(
            1,
            None,
            BindingTarget::Source(source.clone()),
            BindingCause::Startup,
            None,
        );
        event.channel_id = CANARY_CHANNEL;
        event.provider = ShadowProvider::Codex;
        event.tmux_session = logical;
        bindings.commit(event);
        harness.gate.acquired();
        let before = harness.channel().cursor(&source).unwrap().captured_through;
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Codex, bindings);
        for line in [
            output_line(
                "event_msg",
                serde_json::json!({"type":"task_started","turn_id":"t1"}),
            ),
            output_line(
                "response_item",
                serde_json::json!({"type":"message","role":"assistant","id":"m1",
                "content":[{"type":"output_text","text":"alternate legacy output"}]}),
            ),
            output_line(
                "event_msg",
                serde_json::json!({"type":"task_complete","turn_id":"t1"}),
            ),
        ] {
            append(&path, &line);
        }
        polls(6).await;
        if alternate_owner {
            assert_eq!(harness.port.posts(), ["alternate legacy output"]);
            assert!(harness.channel().cursor(&source).unwrap().captured_through > before);
        } else {
            assert!(harness.port.posts().is_empty());
            assert_eq!(
                harness.channel().cursor(&source).unwrap().captured_through,
                before
            );
        }
        assert!(harness.alarms.taken().is_empty());
        halt(stop, task).await;
        assert_eq!(
            dedupe::binding_events::codex::read_ownership(&old_context).unwrap(),
            old_fold
        );
        for ext in [
            "spawn_nonce",
            crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
        ] {
            assert!(
                !Path::new(&crate::services::tmux_common::session_temp_path(
                    CANARY_TMUX,
                    ext
                ))
                .exists()
            );
        }
    }
    dedupe::binding_events::forget_channel_for_tests(CANARY_CHANNEL);
    dedupe::binding_events::set_test_root(None);
    dedupe::reset_state_for_tests();
}
