//! A rebind's atomic relay replacement as the actual idle scanner sees it.

use super::*;

const CHILD: &str = "ADK_IDLE_REPLACE_CHILD";

/// After a rebind replaces the relay the idle scanner restarts it at byte 0: the rebuilt
/// head an open row deferred is recovered, and the body delivered earlier is not resent.
#[tokio::test(start_paused = true)]
async fn idle_cursor_restarts_a_rebind_replaced_relay_at_zero() {
    if std::env::var_os(CHILD).is_none() {
        let name = "services::discord::session_relay_sink::tests::idle_replace_tests::idle_cursor_restarts_a_rebind_replaced_relay_at_zero";
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .expect("isolated scanner child must execute");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(stdout.contains("1 passed; 0 failed"), "{stdout}");
        return;
    }
    let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let dir = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        dir.path(),
    );
    let path = dir.path().join("relay.jsonl");
    let delivered = "{\"type\":\"system\",\"subtype\":\"init\"}\n{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"delivered before the rebind\"}]}}\n";
    let rebuilt = "{\"type\":\"system\",\"subtype\":\"init\"}\n{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"rebuilt\"}]}}\n";
    assert!(rebuilt.len() < delivered.len());
    let mut f = Dc1Fixture::start(&path, delivered.as_bytes(), true).await;
    let name = f.binding.expected_session_name.clone();
    let cursor = dc1_tick().await[&name];
    assert_eq!(
        cursor,
        (delivered.len() as u64, 0),
        "delivered bytes sit behind the cursor"
    );

    let mut row = inflight_for(&name, RelayOwnerKind::Watcher, false);
    row.channel_id = 58080001;
    inflight::save_inflight_state(&row).unwrap();
    let staged = format!("{}.rebuilt", path.display());
    std::fs::write(&staged, rebuilt).unwrap();
    super::super::super::recovery_engine::relay_output_replacements::note_relay_output_replaced(
        path.to_str().unwrap(),
    );
    std::fs::rename(&staged, &path).unwrap();
    assert_eq!(
        dc1_tick().await[&name],
        (0, 1),
        "the open row defers the rebuilt relay from byte 0"
    );

    inflight::clear_inflight_state(&ProviderKind::Claude, 58080001);
    // Production grace is the monotonic OS clock. Tokio advance alone cannot expire it.
    std::thread::sleep(Duration::from_millis(10_100));
    f.producer(true);
    dc1_tick().await;
    f.frame(0, rebuilt).await;
    assert!(
        f.rx.try_recv().is_err(),
        "the delivered body is never resent"
    );
    f.stop().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn context_followup_chunks_p2_allowed_mentions_preserve_users() {
    use crate::services::discord::admin_host_guard::tests::Recorder;
    use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
    use crate::services::discord::{Data, make_shared_data_for_tests_with_storage};
    use poise::serenity_prelude as serenity;
    use std::sync::Arc;

    let _root = crate::config::TestRuntimeRootGuard::new();
    let api = Recorder::start().await;
    let (_, gateway, gateway_server) =
        discord_mock::start(discord_mock::DiscordMockState::new()).await;
    let serenity_ctx = discord_mock::serenity_context_over(api.http.clone(), gateway).await;
    let (manager, _) =
        serenity::gateway::ShardManager::new(serenity::gateway::ShardManagerOptions {
            data: serenity_ctx.data.clone(),
            event_handlers: vec![],
            raw_event_handlers: vec![],
            framework: Arc::new(std::sync::OnceLock::new()),
            shard_index: 0,
            shard_init: 0,
            shard_total: 1,
            ws_url: Arc::new(tokio::sync::Mutex::new("ws://127.0.0.1:1".into())),
            cache: serenity_ctx.cache.clone(),
            http: api.http.clone(),
            intents: serenity::GatewayIntents::empty(),
            presence: None,
        });
    let data = Data {
        shared: make_shared_data_for_tests_with_storage(None),
        token: "test-token".into(),
        provider: crate::services::provider::ProviderKind::Claude,
    };
    let options =
        poise::FrameworkOptions::<Data, crate::services::discord::formatting::Error>::default();
    let command = poise::Command::<Data, crate::services::discord::formatting::Error>::default();
    let command_tree = [&command];
    let message = discord_mock::user_message(741_900, "test command");
    let invocation_data: tokio::sync::Mutex<Box<dyn std::any::Any + Send + Sync>> =
        tokio::sync::Mutex::new(Box::new(()));
    let ctx = poise::Context::Prefix(poise::PrefixContext {
        msg: &message,
        content_start: 0,
        invoked_command_name: "test",
        args: "",
        framework: poise::FrameworkContext {
            serenity_context: &serenity_ctx,
            options: &options,
            user_data: &data,
            shard_manager: &manager,
        },
        command_tree: &command_tree,
        invocation_data: &invocation_data,
        trigger: poise::MessageDispatchTrigger::MessageCreate,
        __non_exhaustive: (),
    });
    let content = "<@1234> @everyone @here <@&5678> 한글\n".repeat(100);
    let chunks = crate::domain::message_split::split_message(&content);
    assert!(chunks.len() > 1, "fixture must reach channel followups");

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        crate::services::discord::formatting::delivery::send_long_message_ctx(ctx, &content),
    )
    .await;
    gateway_server.abort();
    result
        .expect("command chunks finish")
        .expect("command chunks delivered");

    let calls = api.take();
    assert_eq!(
        calls.len(),
        chunks.len(),
        "every chunk reaches HTTP: {calls:?}"
    );
    let prefix = format!(
        "POST /api/v10/channels/{}/messages ",
        message.channel_id.get()
    );
    for (call, chunk) in calls.iter().zip(chunks) {
        let body: serde_json::Value =
            serde_json::from_str(call.strip_prefix(&prefix).expect("actual channel POST")).unwrap();
        assert_eq!(body["content"], chunk);
        assert!(body["content"].as_str().unwrap().contains("<@1234>"));
        assert_eq!(
            body["allowed_mentions"]["parse"],
            serde_json::json!(["users"])
        );
    }
}
