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
