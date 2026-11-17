use super::*;
use crate::services::tui_o::cutover::test_override;

fn shared() -> Arc<SharedData> {
    run_bot_build_shared_data(
        DiscordBotSettings::default(),
        &ProviderKind::Antigravity,
        RuntimeServices {
            initial_skills: Vec::new(),
            token_hash: "thread-lookup-fixture".into(),
            api_port: 9,
            health_registry: Arc::new(health::HealthRegistry::new()),
            pg_pool: None,
            engine: None,
        },
        ProcessLifecycleCounters {
            global_active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            global_finalizing: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            shutdown_remaining: Arc::new(std::sync::atomic::AtomicUsize::new(1)),
        },
        UiFeatureFlags {
            two_message_panel_enabled: false,
        },
        RestoredSessionState {
            model_overrides: &[],
            node_overrides: &[],
            fast_mode_channels: &[],
            fast_mode_reset_entries: &[],
            fast_mode_reset_channels: &[],
            codex_goals_channels: &[],
            codex_goals_reset_channels: &[],
        },
    )
}

#[tokio::test]
async fn gateway_without_shadow_does_not_install_a_lookup() {
    let _lookups = test_override::force_channel_lookups();
    spawn(&shared(), &ProviderKind::Antigravity, None, false);
    assert!(test_override::channel_lookup_providers().is_empty());
}

#[tokio::test]
async fn execution_without_shadow_does_not_install_a_lookup() {
    let _lookups = test_override::force_channel_lookups();
    spawn_execution(&shared(), &ProviderKind::Antigravity, None);
    assert!(test_override::channel_lookup_providers().is_empty());
}
