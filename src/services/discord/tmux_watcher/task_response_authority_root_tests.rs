use crate::services::discord::tmux::tmux_watcher::task_response_authority::{
    PrepareWatcherTaskResponseError, watcher_card_clients,
};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude as serenity;
use std::sync::Arc;

#[tokio::test]
async fn persisted_notify_recovery_keeps_the_known_provider_client_for_replacement() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let provider_http = Arc::new(serenity::Http::new("offline-provider"));
    let notify_http = Arc::new(serenity::Http::new("offline-notify"));
    let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
    registry
        .set_utility_bot_http_for_tests(
            crate::services::discord::bot_role::UtilityBotRole::Notify,
            notify_http.clone(),
        )
        .await;
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().health_registry = (&registry).into();
    let clients = watcher_card_clients(
        &provider_http,
        &shared,
        &ProviderKind::Claude,
        Some("notify"),
    )
    .await
    .expect("persisted notify recovery must retain both original and replacement owners");
    assert!(Arc::ptr_eq(
        &clients
            .by_key("notify")
            .expect("original notify owner")
            .http,
        &notify_http
    ));
    assert!(Arc::ptr_eq(
        &clients
            .by_key("provider:claude")
            .expect("the known provider must be available for a canonical replacement")
            .http,
        &provider_http
    ));
    assert!(matches!(
        watcher_card_clients(
            &provider_http,
            &shared,
            &ProviderKind::Claude,
            Some("provider:codex"),
        )
        .await,
        Err(PrepareWatcherTaskResponseError::Permanent(_))
    ));
}
