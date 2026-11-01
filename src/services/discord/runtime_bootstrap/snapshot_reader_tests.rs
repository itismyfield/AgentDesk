use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

#[test]
fn bot_boot_modules_keep_the_installed_config() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "server: {}\n",
            "server: {}\ncluster: {enabled: true, role: runner, runtime_profile: runner, intake_routing: {enabled: true, mode: observe}}\n",
        );
        assert!(
            bot_boot_settings()
                .cluster
                .runtime_profile
                .modules()
                .gateway
        );
        fixture.reload();
        assert!(
            !bot_boot_settings()
                .cluster
                .runtime_profile
                .modules()
                .gateway
        );
    });
}

#[test]
fn credential_watcher_plan_keeps_disabled_boot_value() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "server: {}\nmcp: {watch_credentials: false, credential_notify_dedupe_secs: 41}\n",
            "server: {}\nmcp: {watch_credentials: true, credential_notify_dedupe_secs: 73}\n",
        );
        assert_eq!(framework_setup::credential_watcher_dedupe_window(), None);
        fixture.reload();
        assert_eq!(
            framework_setup::credential_watcher_dedupe_window(),
            Some(std::time::Duration::from_secs(73))
        );
    });
}

#[test]
fn credential_watcher_plan_keeps_installed_dedupe_and_default() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "server: {}\nmcp: {watch_credentials: true, credential_notify_dedupe_secs: 41}\n",
            "server: {}\nmcp: {watch_credentials: true}\n",
        );
        assert_eq!(
            framework_setup::credential_watcher_dedupe_window(),
            Some(std::time::Duration::from_secs(41))
        );
        fixture.reload();
        assert_eq!(
            framework_setup::credential_watcher_dedupe_window(),
            Some(std::time::Duration::from_secs(300))
        );
    });
}

fn shared_with_configured_placeholder() -> Arc<SharedData> {
    run_bot_build_shared_data(
        DiscordBotSettings::default(),
        &ProviderKind::Claude,
        RuntimeServices {
            initial_skills: Vec::new(),
            token_hash: "snapshot-reader-shared".to_string(),
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
            placeholder_live_events_enabled: false,
            status_panel_v2_enabled: true,
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

fn configured_placeholder_footer() -> String {
    use crate::services::discord::placeholder_live_events::status_events_from_tool_use_with_id;
    let shared = shared_with_configured_placeholder();
    let channel = ChannelId::new(7_349_221);
    shared.ui.placeholder_live_events.push_status_events(
        channel,
        status_events_from_tool_use_with_id(
            "Bash",
            r#"{"command":"echo snapshot-display"}"#,
            Some("fixture-tool"),
        ),
    );
    shared
        .ui
        .placeholder_live_events
        .render_footer_status_panel(channel, &ProviderKind::Claude, 1_700_000_000, ("", "⠸"))
}

#[test]
fn shared_data_placeholder_uses_the_installed_display_mode() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "server: {}\nplaceholder: {tool_activity_display: summary}\n",
            "server: {}\nplaceholder: {tool_activity_display: class}\n",
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let footer = configured_placeholder_footer();
            assert!(footer.contains("도구 1"), "{footer}");
            assert!(!footer.contains("마지막 도구"), "{footer}");
            fixture.reload();
            let footer = configured_placeholder_footer();
            assert!(footer.contains("마지막 도구 ([Bash])"), "{footer}");
            assert!(!footer.contains("도구 1"), "{footer}");
        });
    });
}

#[test]
fn boot_consumers_call_the_configuration_seams() {
    let run_bot = include_str!("../runtime_bootstrap.rs")
        .split_once("pub(crate) async fn run_bot(")
        .unwrap()
        .1;
    assert!(run_bot.contains("let boot_config = bot_boot_settings();"));
    assert_eq!(run_bot.matches("bot_boot_settings()").count(), 1);
    for consumer in [
        "let modules = boot_config.cluster.runtime_profile.modules();",
        "boot_config.tui_o.as_ref()",
    ] {
        assert!(run_bot.contains(consumer), "{consumer}");
    }
    assert!(
        include_str!("framework_setup.rs")
            .contains("if let Some(dedupe_window) = credential_watcher_dedupe_window()")
    );
}
