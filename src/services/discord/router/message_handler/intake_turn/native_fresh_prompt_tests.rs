use super::*;
use crate::services::discord::commands::control::native::{
    switch_on_for_tests,
    tests::{Fake, Fixture, runtime},
};
use crate::services::memory::{SessionAnchorRequest, load_session_anchor_prompt};
use crate::services::tui_prompt_dedupe::binding_events::BindingCause;
use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Anchors {
    endpoint: String,
    calls: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Anchors {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Anchors {
    async fn start() -> Self {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/mcp", post(|State(calls): State<Arc<AtomicUsize>>, Json(body): Json<Value>| async move {
                let mut headers = HeaderMap::new();
                let result = if body["method"] == "initialize" {
                    headers.insert("MCP-Session-Id", "scratch-session".parse().unwrap());
                    json!({"protocolVersion":"2025-11-25", "capabilities":{}, "serverInfo":{"name":"scratch", "version":"1"}})
                } else if body["params"]["name"] == "context" {
                    let content = if calls.fetch_add(1, Ordering::SeqCst) == 0 { "before clear anchor" } else { "after clear fresh anchor" };
                    let payload = json!({"anchorCount":1, "anchors":{"permanent":[{"content":content}]}, "injectionText":format!("[ANCHOR MEMORY]\n- {content}")});
                    json!({"content":[{"type":"text", "text":payload.to_string()}]})
                } else {
                    json!({"content":[{"type":"text", "text":"{\"success\":true}"}]})
                };
                (headers, Json(json!({"jsonrpc":"2.0", "id":body["id"], "result":result})))
            }))
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            endpoint,
            calls,
            task,
        }
    }
}

#[test]
fn restart_native_clear_first_intake_submits_fresh_anchor_prompt_pg() {
    if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::intake_turn::native_fresh_prompt_tests::",
        "restart_native_clear_first_intake_submits_fresh_anchor_prompt_pg"
    )) {
        return;
    }
    runtime().block_on(async {
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let mut fixture = Fixture::new_locked(91, None).await;
        let root = crate::config::runtime_root().unwrap();
        let _tmux = crate::services::tui_prompt_dedupe::binding_context::tests::fake_tmux(&root);
        std::fs::write(root.join("tmux"), "#!/bin/bash\nexit 1\n").unwrap();
        let anchors = Anchors::start().await;
        let config = std::env::var_os("AGENTDESK_CONFIG").unwrap();
        let mut yaml = std::fs::read_to_string(&config).unwrap();
        yaml.push_str(&format!("memory:\n  backend: memento\n  mcp:\n    endpoint: {}\n    access_key_env: ADK_NATIVE_FRESH_TEST_KEY\n", anchors.endpoint));
        std::fs::write(&config, yaml).unwrap();
        let _key = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock("ADK_NATIVE_FRESH_TEST_KEY", "scratch-key".as_ref());
        let _workspace = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock("MEMENTO_WORKSPACE", "scratch-native-clear".as_ref());
        let boot = crate::config::load_from_path(std::path::Path::new(&config)).unwrap();
        crate::services::tui_o::channel_policy::install(&boot).unwrap();
        let api = crate::services::discord::admin_host_guard::tests::Recorder::start().await;
        crate::services::discord::internal_api::init(api.port, None);
        Arc::get_mut(&mut fixture.shared).unwrap().api_port = api.port;
        {
            let mut core = fixture.shared.core.lock().await;
            core.sessions.get_mut(&fixture.channel_id).unwrap().current_path = Some(root.display().to_string());
        }
        let memory = settings::memory_settings_for_binding(None);
        assert_eq!(memory.backend, settings::MemoryBackendKind::Memento);
        let seeded = load_session_anchor_prompt(SessionAnchorRequest {
            settings: &memory, provider: &ProviderKind::Claude,
            current_path: root.to_str().unwrap(), channel_id: fixture.channel_id.get(),
            memory_scope_channel_id: fixture.channel_id.get(), role_binding: None,
            session_id: Some("new"), fresh: false,
        }).await.unwrap();
        assert!(seeded.contains("before clear anchor"));
        let pool: &sqlx::PgPool = &fixture.pool;
        fixture.unresolved().await;
        fixture.record("new", BindingCause::Clear, false);
        let fake = Arc::new(Fake::default());
        let _on = switch_on_for_tests(fake.clone());
        let (capture, submitted_prompt) = tokio::sync::oneshot::channel();
        *super::super::provider_dispatch::INPUT_PROMPT_PROBE.lock().unwrap() = Some((fixture.channel_id.get(), capture));
        let (entered, provider_entered) = tokio::sync::oneshot::channel();
        let (release_provider, release) = std::sync::mpsc::channel();
        *super::super::provider_dispatch::INPUT_EFFECT_PROBE.lock().unwrap() = Some(super::super::provider_dispatch::InputEffectProbe {
            channel: fixture.channel_id.get(), entered, release, terminal_before_release: false,
        });
        let (captured, bridge_captured) = tokio::sync::oneshot::channel();
        let (release_bridge, resume) = tokio::sync::oneshot::channel();
        *crate::services::discord::turn_bridge::resume_pin_tests::BRIDGE_CAPTURE_PROBE.lock().unwrap() = Some((fixture.channel_id, captured, resume));
        let (completed, bridge_completed) = tokio::sync::oneshot::channel();
        *crate::services::discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE.lock().unwrap() = Some((fixture.channel_id, completed));
        let request = IntakeRequest {
            intake_outbox_id: None, channel_id: fixture.channel_id,
            user_msg_id: MessageId::new(6_577_991), source_message_ids: Vec::new(),
            busy_followup_retry_user_msg_id: MessageId::new(6_577_991),
            request_owner: UserId::new(7), request_owner_name: "owner".into(),
            user_text: "first prompt after restart clear".into(),
            reply_to_user_message: false, defer_watcher_resume: false,
            wait_for_completion: false, merge_consecutive: false, reply_context: None,
            has_reply_boundary: false, dm_hint: Some(false), turn_kind: TurnKind::Foreground,
            preserve_on_cancel: false,
        };
        tokio::time::timeout(std::time::Duration::from_secs(20), execute_intake_turn_core(&api.http, &fixture.shared, "", request, Vec::new())).await.unwrap().unwrap();
        let submitted = tokio::time::timeout(std::time::Duration::from_secs(10), submitted_prompt).await.unwrap().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), provider_entered).await.unwrap().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), bridge_captured).await.unwrap().unwrap();
        release_provider.send(()).unwrap();
        release_bridge.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(20), bridge_completed).await.unwrap().unwrap();
        assert!(submitted.prompt.contains("first prompt after restart clear"));
        assert_eq!(submitted.session_id.as_deref(), Some("new"), "the first prompt warm resumes Y");
        let system_prompt = submitted.system_prompt.unwrap();
        assert!(system_prompt.contains("after clear fresh anchor"), "the actual provider submission must contain the refreshed anchor layer: {system_prompt}");
        assert!(!system_prompt.contains("before clear anchor"));
        assert_eq!(anchors.calls.load(Ordering::SeqCst), 2);
        assert_eq!(fake.calls(), vec!["save:new"]);
        assert!(!fixture.shared.core.lock().await.sessions[&fixture.channel_id].cleared, "the next turn must not consume recovery freshness again");
        assert_eq!(crate::db::session_transcripts::native_channel_clear_state(pool, &fixture.channel_id.get().to_string()).await.unwrap(), crate::db::session_transcripts::NativeClearBoundary::Resolved);
        fixture.shared.mailboxes.remove_fixture_for_test(fixture.channel_id);
        fixture.drop_db().await;
    });
}
