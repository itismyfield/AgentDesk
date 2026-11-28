use crate::services::discord::outbound::send_gate;
use crate::services::provider::ProviderKind;
#[tokio::test]
async fn provider_notice_uses_channel_owner_tool_card_credentials() {
    use crate::services::discord::{health::HealthRegistry, make_shared_data_for_tests};
    use std::sync::Arc;
    let registry = HealthRegistry::new();
    let first = make_shared_data_for_tests();
    let owner = make_shared_data_for_tests();
    first.settings.write().await.allowed_channel_ids = vec![41];
    owner.settings.write().await.allowed_channel_ids = vec![42];
    first
        .http
        .cached_bot_token
        .set("first-token".into())
        .unwrap();
    owner
        .http
        .cached_bot_token
        .set("owner-token".into())
        .unwrap();
    registry.register("claude".into(), Arc::clone(&first)).await;
    registry.register("claude".into(), Arc::clone(&owner)).await;
    registry
        .register_http(
            "claude".into(),
            first.serenity_http_or_token_fallback().unwrap(),
        )
        .await;
    let options = send_gate::ManualOutboundOptions {
        trusted_internal_outbox: true,
        channel_owner_provider: Some(ProviderKind::Claude),
        ..Default::default()
    };
    let http = send_gate::resolve_manual_bot_http(
        &registry,
        "claude",
        poise::serenity_prelude::ChannelId::new(42),
        &options,
    )
    .await
    .unwrap();
    assert!(http.token().ends_with("owner-token"));
    first.settings.write().await.allowed_channel_ids.push(42);
    assert_eq!(
        send_gate::resolve_manual_bot_http(
            &registry,
            "claude",
            poise::serenity_prelude::ChannelId::new(42),
            &options
        )
        .await
        .unwrap_err()
        .0,
        "503 Service Unavailable"
    );
    assert!(
        send_gate::resolve_manual_bot_http(
            &HealthRegistry::new(),
            "claude",
            poise::serenity_prelude::ChannelId::new(42),
            &options
        )
        .await
        .is_err()
    );
}
