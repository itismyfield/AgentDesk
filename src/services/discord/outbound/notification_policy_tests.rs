use super::notification_policy::progress;

#[tokio::test(flavor = "current_thread")]
async fn scoped_progress_silences_the_shared_card_and_relay_builders() {
    use crate::services::discord::{admin_host_guard::tests::Recorder, gateway, http};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _live = crate::config_live_reload::test_live_guard_after_shared_test_env_lock();
    let mut config = crate::config::Config::default();
    config.discord.secretary_channels = vec!["74190301".into()];
    crate::config_live_reload::install(config);
    let (api, shared) = (
        Recorder::start().await,
        crate::services::discord::make_shared_data_for_tests(),
    );
    let channel = poise::serenity_prelude::ChannelId::new(74_190_301);
    let card = gateway::send_outbound_message_with_nonce_classified(
        api.http.clone(),
        shared,
        channel,
        "card",
        "card-nonce-0001",
    );
    let _ = progress(card).await;
    let _ = progress(http::send_channel_message(&api.http, channel, "panel")).await;
    let _ = http::send_channel_message(&api.http, channel, "answer").await;
    let flags: Vec<_> = api
        .take()
        .iter()
        .filter_map(|call| {
            call.strip_prefix("POST ")?
                .split_once(' ')
                .map(|(_, body)| body.to_owned())
        })
        .map(|body| serde_json::from_str::<serde_json::Value>(&body).unwrap()["flags"].as_u64())
        .collect();
    assert_eq!(flags, [Some(4096), Some(4096), None]);
}
