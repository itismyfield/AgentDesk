//! A fresh Discord Codex turn has its admission mark before the real provider effect begins.

use super::*;
use crate::services::tui_o::channel_policy::{Adoption, BootChannels};
use crate::services::tui_o::cutover::test_override;
use std::collections::BTreeMap;
use std::time::Duration;

fn reserved(channel: u64) -> BootChannels {
    let config = serde_json::from_value(serde_json::json!({
        "server": {},
        "agents": [{"id": "mark", "name": "Mark", "channels": {
            "codex": {"id": channel.to_string(), "runtime": "tui"}
        }}],
        "tui_o": {"writer": {"channels": [channel], "late_adopt": true},
            "turn": {"channels": [channel]}}
    }))
    .unwrap();
    BootChannels::validate(&config)
        .unwrap()
        .seeded(&config, |_, _| {
            Ok(BTreeMap::from([(channel, Adoption::Pending)]))
        })
        .unwrap()
}

async fn intake_at_provider(resumed: bool) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = crate::config::runtime_root().unwrap();
    let config_path = root.join("turn-mark-fixture.yaml");
    std::fs::write(&config_path, "server: {}\ndata: {dir: data}\n").unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config_path,
    );
    let _tmux = crate::services::tui_prompt_dedupe::binding_context::tests::fake_tmux(&root);
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(
        "http://127.0.0.1:9".to_string(),
    );
    let empty = serde_json::from_value(serde_json::json!({"server": {}, "agents": []})).unwrap();
    crate::services::tui_o::channel_policy::install(&empty).unwrap();
    let channel = ChannelId::new(if resumed { 7_030_872 } else { 7_030_871 });
    let boot = reserved(channel.get());
    let candidate = boot.candidate(channel.get()).unwrap().clone();
    assert!(candidate.late_adopt_origin());
    assert_eq!(candidate.peek(), Adoption::Deferred);
    let _boot = test_override::force_boot(boot);
    let api = crate::services::discord::admin_host_guard::tests::Recorder::start().await;
    crate::services::discord::internal_api::init(api.port, None);
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    let own = Arc::get_mut(&mut shared).unwrap();
    own.provider = ProviderKind::Codex;
    own.api_port = api.port;
    shared.settings.write().await.provider = ProviderKind::Codex;
    crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, "").await;
    {
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        session.channel_name = Some("turn-mark-entry".into());
        session.current_path = Some(root.display().to_string());
        session.session_id = resumed.then(|| "mark-existing-session".into());
    }
    let (capture, submitted) = tokio::sync::oneshot::channel();
    *provider_dispatch::INPUT_PROMPT_PROBE.lock().unwrap() = Some((channel.get(), capture));
    let (entered, release) = hold_input_effect_for_tests(channel.get());
    let (observed, at_effect) = tokio::sync::oneshot::channel();
    let observed_candidate = candidate.clone();
    *provider_dispatch::INPUT_EFFECT_OBSERVER.lock().unwrap() = Some((
        channel.get(),
        Box::new(move |actual_provider| {
            let _ = observed.send((actual_provider, observed_candidate.turn_mark()));
        }),
    ));
    let (completed, bridge_completed) = tokio::sync::oneshot::channel();
    *crate::services::discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
        .lock()
        .unwrap() = Some((channel, completed));
    let message = MessageId::new(channel.get() + 10);
    let request = IntakeRequest {
        intake_outbox_id: None,
        channel_id: channel,
        user_msg_id: message,
        source_message_ids: Vec::new(),
        busy_followup_retry_user_msg_id: message,
        request_owner: UserId::new(7),
        request_owner_name: "owner".into(),
        user_text: "check the admission mark before Codex input".into(),
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        merge_consecutive: false,
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: TurnKind::Foreground,
        preserve_on_cancel: false,
    };
    let wait = Duration::from_secs(20);
    tokio::time::timeout(
        wait,
        execute_intake_turn_core(&api.http, &shared, "", request, Vec::new()),
    )
    .await
    .unwrap()
    .unwrap();
    tokio::time::timeout(wait, entered)
        .await
        .expect("the actual intake reached the provider effect")
        .unwrap();
    let submitted = tokio::time::timeout(wait, submitted)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        submitted.session_id.as_deref(),
        resumed.then_some("mark-existing-session"),
        "the runtime selected the session strategy this case exercises"
    );
    let (actual_provider, marked) = tokio::time::timeout(wait, at_effect)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        actual_provider, "codex",
        "the actual provider boundary must be Codex"
    );
    release.send(()).unwrap();
    tokio::time::timeout(wait, bridge_completed)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        marked.is_some(),
        !resumed,
        "only a fresh session is marked before provider input, resumed={resumed}"
    );
    assert_eq!(candidate.peek(), Adoption::Deferred);
    shared.mailboxes.remove_fixture_for_test(channel);
}

#[tokio::test]
async fn fresh_codex_intake_marks_before_provider_input() {
    if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::turn_mark_admission_tests::",
        "fresh_codex_intake_marks_before_provider_input"
    )) {
        return;
    }
    intake_at_provider(false).await;
}

#[tokio::test]
async fn reused_codex_intake_has_no_admission_mark_at_provider_input() {
    if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::turn_mark_admission_tests::",
        "reused_codex_intake_has_no_admission_mark_at_provider_input"
    )) {
        return;
    }
    intake_at_provider(true).await;
}
