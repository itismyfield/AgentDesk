use super::tests::install_clear_end;
use crate::config::TestEnvVarGuard;
use crate::services::{
    claude_tui::hook_server::observation_ingress::observe_binding_hook,
    codex_tui::{
        execution_end::attempt::{self, Phase},
        host_input::spy::{SpyGuard, SpyState},
        warm_followup::{self, VerifiedWarmRequest},
    },
    provider::CancelToken,
    provider_teardown::tests::test_support::FakeTmux,
    tmux_common as tc,
    tui_o::store::codex_end,
    tui_prompt_dedupe::{
        self as dedupe,
        binding_context::{
            BINDING_HEADER, BindingContext, CapturedContext, HookBindingEnvelope,
            ObservedHookProcess, PreparedIncarnation,
        },
        binding_events,
    },
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{fs, io::Write, os::unix::fs::PermissionsExt, sync::Arc};

#[test]
fn actual_verified_warm_consumes_ticket_without_sealing_or_later_reuse() {
    let _shared = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    dedupe::reset_state_for_tests();
    attempt::reset_for_tests();
    binding_events::set_test_root(Some(root.path()));
    let _mode = TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CODEX_DIRECT_TUI_SOURCE_MODE",
        "verified".as_ref(),
    );
    let _hooks = TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CODEX_DIRECT_TUI_HOOKS",
        "1".as_ref(),
    );
    let sessions = root.path().join("sessions");
    fs::create_dir(&sessions).unwrap();
    let sessions = sessions.canonicalize().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let channel = crate::services::codex_tui::canary::CANARY_CHANNEL;
    let pane = crate::services::codex_tui::canary::CANARY_TMUX;
    let session = "019e660d-4859-7522-9cee-8ba7c4e7c744";
    let prompt = "initial verified prompt";
    let context = BindingContext {
        schema: 1,
        provider: "codex".into(),
        created_at: chrono::Utc::now(),
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        tmux_session: pane.into(),
        channel_id: Some(channel),
        owner_runtime_root: tc::current_tmux_owner_marker(),
        host: None,
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: Some(sessions.clone()),
        first_prompt_digest: Some(format!("sha256:{:x}", Sha256::digest(prompt.as_bytes()))),
        source_policy: Some("verified".into()),
    };
    PreparedIncarnation::create(context.clone()).unwrap();
    dedupe::codex_verified::set_permission_for_tests(
        &context,
        dedupe::codex_verified::DeliveryPermission::Allowed,
    );
    let marker = tc::session_temp_path(pane, "spawn_nonce");
    fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    fs::write(marker, &context.execution_nonce).unwrap();
    fs::write(
        tc::session_temp_path(pane, tc::TMUX_RUNTIME_KIND_TEMP_EXT),
        crate::services::agent_protocol::RuntimeHandoffKind::CodexTui.as_str(),
    )
    .unwrap();
    let fake = FakeTmux::install(pane);
    dedupe::register_tmux_channel(pane, channel);
    let path = sessions.join(format!("rollout-{session}.jsonl"));
    let time = (context.created_at + chrono::Duration::seconds(1)).to_rfc3339();
    let header = json!({"type":"session_meta","timestamp":time,"payload":{
        "id":session,"timestamp":time,"cwd":cwd,"source":"cli","originator":"codex_cli_rs"
    }});
    fs::write(&path, format!("{header}\n")).unwrap();
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(context.clone()),
        observed: ObservedHookProcess::default(),
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    let payload =
        json!({"session_id":session,"transcript_path":path,"source":"startup","prompt":prompt});
    let ingress = observe_binding_hook(
        "codex",
        "session-start",
        Some(session),
        Some(session),
        &payload,
        &headers,
    );
    let fold = binding_events::codex::read_ownership(&context).unwrap();
    assert_eq!(
        fold.verified
            .as_ref()
            .map(|proof| proof.source.session_id.as_str()),
        Some(session),
        "actual native hook proof required: {ingress:?}; {fold:?}"
    );
    assert_eq!(
        dedupe::runtime_binding_for_tmux_session(pane)
            .unwrap()
            .session_id
            .as_deref(),
        Some(session)
    );

    let binary = root.path().join("codex");
    fs::write(&binary, "#!/bin/sh\nif [ \"$1 $2\" = 'resume --help' ]; then echo --dangerously-bypass-hook-trust; else echo 'codex-cli 0.157.1'; fi\n").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let _binary =
        TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_CODEX_PATH", &binary);
    let _endpoint = crate::services::claude_tui::hook_server::publish_hook_endpoint(
        "http://127.0.0.1:9".into(),
    );
    let options = crate::services::codex::actual_submit_tests::options(session, &cwd);
    warm_followup::write_codex_tui_launch_options_evidence(pane, &options).unwrap();
    let ready = "╭──────────────────────────────────────────────╮\n│ ▌                                            │\n╰──────────────────────────────────────────────╯\n  Esc to interrupt   Ctrl+J newline   ⏎ send";
    let draft = "╭──────────────────────────────────────────────╮\n│ first ▌                                      │\n╰──────────────────────────────────────────────╯\n  Esc to interrupt   Ctrl+J newline   ⏎ send";
    let output = path.clone();
    let spy = SpyGuard::install(SpyState {
        captures: [ready, draft, ready, ready]
            .into_iter()
            .map(|s| Some(s.into()))
            .collect(),
        on_enter: Some(Box::new(move || {
            let message = json!({"type":"response_item","payload":{"type":"message","role":"assistant","id":"immediate","content":[{"type":"output_text","text":"complete"}]}});
            let done = json!({"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"complete"}});
            writeln!(
                fs::OpenOptions::new().append(true).open(&output).unwrap(),
                "{message}\n{done}"
            )
            .unwrap();
        })),
        ..SpyState::default()
    });
    let end = install_clear_end(channel, pane);
    let dispatch = attempt::begin(Some(channel), Some(pane));
    let id = attempt::current(channel, pane).unwrap();
    let token = Arc::new(CancelToken::new());
    let deadline = Arc::clone(&token);
    let (finished, wait) = std::sync::mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        if wait
            .recv_timeout(std::time::Duration::from_secs(3))
            .is_err()
        {
            deadline
                .cancelled
                .store(true, std::sync::atomic::Ordering::Release);
        }
    });
    let (sender, _receiver) = std::sync::mpsc::channel();
    let result = warm_followup::verified_warm(VerifiedWarmRequest {
        tmux: pane,
        channel,
        session_id: session,
        cwd: &cwd,
        prompt: "first",
        options: &options,
        token: Arc::clone(&token),
        sender,
    });
    let _ = finished.send(());
    watchdog.join().unwrap();
    assert!(
        !token.cancelled.load(std::sync::atomic::Ordering::Acquire),
        "watchdog cancellation is not verified warm success: {result:?}"
    );
    assert!(result.is_ok(), "{result:?}; {:?}", spy.calls());
    tc::with_tmux_source_authority(pane, |authority| {
        assert_eq!(
            attempt::phase_under(&id, authority),
            Some(Phase::Bound {
                nonce: context.execution_nonce.clone()
            })
        );
    });
    assert!(attempt::take_ticket(&id).is_none());
    assert_eq!(
        codex_end::read(&crate::config::runtime_root().unwrap(), channel)
            .unwrap()
            .as_slice(),
        std::slice::from_ref(&end)
    );
    assert_eq!(
        spy.calls()
            .iter()
            .filter(|call| call.as_str() == "keys:Enter")
            .count(),
        1
    );
    assert!(
        fake.take_calls()
            .iter()
            .all(|call| !call.starts_with("kill-"))
    );
    drop(dispatch);
    let _later = attempt::begin(Some(channel), Some(pane));
    assert!(attempt::take_ticket(&attempt::current(channel, pane).unwrap()).is_none());
    assert_eq!(
        codex_end::read(&crate::config::runtime_root().unwrap(), channel).unwrap(),
        [end]
    );
    binding_events::forget_channel_for_tests(channel);
    binding_events::set_test_root(None);
    dedupe::reset_state_for_tests();
}
