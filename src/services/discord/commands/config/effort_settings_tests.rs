use super::*;
use crate::config::{TestEnvVarGuard, TestRuntimeRootGuard};
use crate::services::discord::{SharedData, make_shared_data_for_tests};
use std::path::Path;

const TOKEN: &str = "effort-fixture-token";
const ASTRA: &str = "gpt-6-astra";
const SOL: &str = "gpt-6.1-sol";
const LUNA: &str = "gpt-6-luna";

fn fixture(check: impl FnOnce(&Arc<SharedData>, serenity::ChannelId, &Path)) {
    let _hosting = crate::services::provider_hosting::TEST_CONFIG_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let root = TestRuntimeRootGuard::new();
    let set = TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _config = set("AGENTDESK_CONFIG", &root.path().join("effort-fixture.yaml"));
    let home = root.path().join("codex");
    std::fs::create_dir_all(&home).unwrap();
    let _home = set("CODEX_HOME", &home);
    let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
    std::fs::write(home.join("models_cache.json"), serde_json::to_vec(&serde_json::json!({
        "models": [
            {"slug": ASTRA, "display_name": "Astra", "visibility": "list",
             "supported_reasoning_levels": [{"effort":"low"},{"effort":"high"},{"effort":"max"},{"effort":"ultra"}]},
            {"slug": SOL, "display_name": "Sol", "visibility": "list",
             "supported_reasoning_levels": [{"effort":"low"},{"effort":"high"},{"effort":"max"},{"effort":"ultra"}]},
            {"slug": "gpt-5.5", "display_name": "Old", "visibility": "list",
             "supported_reasoning_levels": [{"effort":"low"},{"effort":"high"},{"effort":"xhigh"}]},
            {"slug": LUNA, "display_name": "Luna", "visibility": "list",
             "supported_reasoning_levels": [{"effort":"low"},{"effort":"high"},{"effort":"max"}]}
        ]
    })).unwrap()).unwrap();
    let mut config = crate::config::Config::default();
    for provider in ["claude", "codex"] {
        config.providers.insert(
            provider.into(),
            crate::config::ProviderConfig {
                tui_hosting: Some(true),
                ..Default::default()
            },
        );
    }
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            crate::services::provider_hosting::install_provider_hosting_config(
                &crate::config::Config::default(),
            );
        }
    }
    let _restore = Restore;
    crate::services::provider_hosting::install_provider_hosting_config(&config);
    let shared = make_shared_data_for_tests();
    check(&shared, serenity::ChannelId::new(8109101), &home);
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn capability_false_or_unknown_has_zero_save_stop() {
    fixture(|shared, channel, home| {
        runtime().block_on(async {
            for (provider, model) in [
                (ProviderKind::Claude, "claude-opus-4-8"),
                (ProviderKind::Codex, LUNA),
                (ProviderKind::Codex, "unlisted-model"),
            ] {
                shared
                    .overrides
                    .model_overrides
                    .insert(channel, model.into());
                let result =
                    update_channel_effort(shared, TOKEN, channel, &provider, "ultra").await;
                assert!(result.is_err(), "{provider:?}/{model}: {result:?}");
                let settings = shared.settings.read().await;
                assert!(
                    !settings
                        .channel_effort_levels
                        .contains_key(&channel.to_string())
                );
                assert!(
                    !settings
                        .channel_relaunch_pending
                        .contains(&channel.to_string())
                );
                assert!(!shared.overrides.session_reset_pending.contains(&channel));
            }
            assert!(
                !crate::services::discord::runtime_store::bot_settings_path()
                    .unwrap()
                    .exists()
            );
            assert!(home.join("models_cache.json").is_file());
        })
    });
}

#[test]
fn codex_effort_persists_and_restores_on_its_channel() {
    fixture(|shared, channel, _| {
        runtime().block_on(async {
            shared
                .overrides
                .model_overrides
                .insert(channel, ASTRA.into());
            assert!(
                update_channel_effort(shared, TOKEN, channel, &ProviderKind::Codex, "ultra")
                    .await
                    .unwrap()
            );
            let stored = crate::services::discord::settings::load_bot_settings(TOKEN);
            assert_eq!(
                stored
                    .channel_effort_levels
                    .get(&channel.to_string())
                    .map(String::as_str),
                Some("ultra")
            );
            assert_eq!(
                channel_effort_level(shared, &ProviderKind::Codex, channel)
                    .await
                    .as_deref(),
                Some("ultra")
            );
            assert!(
                !update_channel_effort(shared, TOKEN, channel, &ProviderKind::Codex, "ultra")
                    .await
                    .unwrap()
            );
            assert!(
                update_channel_effort(shared, TOKEN, channel, &ProviderKind::Codex, "high")
                    .await
                    .unwrap()
            );
            assert_eq!(
                channel_effort_level(shared, &ProviderKind::Codex, channel)
                    .await
                    .as_deref(),
                Some("high")
            );
        })
    });
}

#[test]
fn legacy_five_levels_restore() {
    fixture(|shared, channel, _| {
        runtime().block_on(async {
            for level in ["low", "medium", "high", "xhigh", "max"] {
                shared
                    .settings
                    .write()
                    .await
                    .channel_effort_levels
                    .insert(channel.to_string(), level.into());
                assert_eq!(
                    channel_effort_level(shared, &ProviderKind::Claude, channel)
                        .await
                        .as_deref(),
                    Some(level)
                );
            }
            for raw in ["ultracode", "ULTRA", "invalid"] {
                shared
                    .settings
                    .write()
                    .await
                    .channel_effort_levels
                    .insert(channel.to_string(), raw.into());
                assert_eq!(
                    channel_effort_level(shared, &ProviderKind::Claude, channel).await,
                    None,
                    "{raw}"
                );
            }
        })
    });
}

#[test]
fn invalid_raw_effort_cannot_be_saved_or_promoted() {
    fixture(|shared, channel, _| {
        runtime().block_on(async {
            for raw in ["ultracode", "ULTRA", "invalid"] {
                assert!(
                    update_channel_effort(shared, TOKEN, channel, &ProviderKind::Claude, raw)
                        .await
                        .is_err(),
                    "{raw}"
                );
                assert!(
                    !shared
                        .settings
                        .read()
                        .await
                        .channel_effort_levels
                        .contains_key(&channel.to_string())
                );
            }
        })
    });
}

#[test]
fn ultra_saved_model_change_rejects_unsupported_model() {
    fixture(|shared, channel, _| {
        runtime().block_on(async {
            shared
                .overrides
                .model_overrides
                .insert(channel, ASTRA.into());
            shared
                .settings
                .write()
                .await
                .channel_effort_levels
                .insert(channel.to_string(), "ultra".into());
            let result = update_channel_model_override(
                shared,
                TOKEN,
                channel,
                &ProviderKind::Codex,
                Some(LUNA.into()),
            )
            .await;
            assert!(result.is_err(), "{result:?}");
            assert!(result.unwrap_err().to_string().contains("ultra"));
            assert_eq!(
                shared
                    .overrides
                    .model_overrides
                    .get(&channel)
                    .unwrap()
                    .as_str(),
                ASTRA
            );
            let settings = shared.settings.read().await;
            assert_eq!(
                settings.channel_effort_levels[&channel.to_string()],
                "ultra"
            );
            assert!(
                !settings
                    .channel_model_overrides
                    .contains_key(&channel.to_string())
            );
            assert!(
                !settings
                    .channel_relaunch_pending
                    .contains(&channel.to_string())
            );
            assert!(!shared.overrides.session_reset_pending.contains(&channel));
        })
    });
}

#[test]
fn ultra_saved_model_change_keeps_supported_model_timing() {
    fixture(|shared, channel, _| {
        runtime().block_on(async {
            shared
                .overrides
                .model_overrides
                .insert(channel, ASTRA.into());
            shared
                .settings
                .write()
                .await
                .channel_effort_levels
                .insert(channel.to_string(), "ultra".into());
            assert!(
                update_channel_model_override(
                    shared,
                    TOKEN,
                    channel,
                    &ProviderKind::Codex,
                    Some(SOL.into())
                )
                .await
                .unwrap()
            );
            assert_eq!(
                shared
                    .overrides
                    .model_overrides
                    .get(&channel)
                    .unwrap()
                    .as_str(),
                SOL
            );
            let settings = shared.settings.read().await;
            assert_eq!(
                settings.channel_effort_levels[&channel.to_string()],
                "ultra"
            );
            assert!(
                !settings
                    .channel_relaunch_pending
                    .contains(&channel.to_string())
            );
            assert!(
                shared
                    .overrides
                    .model_session_reset_pending
                    .contains(&channel)
            );
            assert!(shared.overrides.session_reset_pending.contains(&channel));
        })
    });
}

#[test]
fn saved_codex_max_rejects_a_model_with_only_xhigh() {
    fixture(|shared, channel, _| {
        runtime().block_on(async {
            shared
                .overrides
                .model_overrides
                .insert(channel, ASTRA.into());
            shared
                .settings
                .write()
                .await
                .channel_effort_levels
                .insert(channel.to_string(), "max".into());
            assert!(
                update_channel_model_override(
                    shared,
                    TOKEN,
                    channel,
                    &ProviderKind::Codex,
                    Some("gpt-5.5".into())
                )
                .await
                .is_err()
            );
            assert_eq!(
                shared
                    .overrides
                    .model_overrides
                    .get(&channel)
                    .unwrap()
                    .as_str(),
                ASTRA
            );
            assert_eq!(
                shared.settings.read().await.channel_effort_levels[&channel.to_string()],
                "max"
            );
            assert!(!shared.overrides.session_reset_pending.contains(&channel));
        })
    });
}

#[test]
fn saved_codex_high_keeps_a_supported_older_model_selectable() {
    fixture(|shared, channel, _| {
        runtime().block_on(async {
            shared
                .overrides
                .model_overrides
                .insert(channel, ASTRA.into());
            shared
                .settings
                .write()
                .await
                .channel_effort_levels
                .insert(channel.to_string(), "high".into());
            assert!(
                update_channel_model_override(
                    shared,
                    TOKEN,
                    channel,
                    &ProviderKind::Codex,
                    Some("gpt-5.5".into())
                )
                .await
                .unwrap()
            );
            assert_eq!(
                shared
                    .overrides
                    .model_overrides
                    .get(&channel)
                    .unwrap()
                    .as_str(),
                "gpt-5.5"
            );
            assert_eq!(
                shared.settings.read().await.channel_effort_levels[&channel.to_string()],
                "high"
            );
            assert!(
                shared
                    .overrides
                    .model_session_reset_pending
                    .contains(&channel)
            );
        })
    });
}

#[test]
fn effort_rechecks_the_role_after_waiting_for_settings() {
    fixture(|shared, channel, _| {
        runtime().block_on(async {
            let org = crate::services::discord::runtime_store::org_schema_path_for_root(
                &crate::config::runtime_root().unwrap(),
            );
            std::fs::create_dir_all(org.parent().unwrap()).unwrap();
            std::fs::write(
                &org,
                serde_yaml::to_string(&serde_json::json!({
                    "version":1,
                    "agents":{
                        "astra":{"display_name":"Astra","provider":"codex","model":ASTRA},
                        "luna":{"display_name":"Luna","provider":"codex","model":LUNA}
                    },
                    "channels":{"by_id":{
                        "8109102":{"agent":"astra"},"8109103":{"agent":"luna"}
                    }}
                }))
                .unwrap(),
            )
            .unwrap();
            shared
                .dispatch
                .role_overrides
                .insert(channel, serenity::ChannelId::new(8109102));
            let lock = shared.settings.write().await;
            let change =
                update_channel_effort(shared, TOKEN, channel, &ProviderKind::Codex, "ultra");
            tokio::pin!(change);
            let pending = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(std::future::Future::poll(change.as_mut(), cx).is_pending())
            })
            .await;
            assert!(pending, "the settings writer must be waiting for its lock");
            shared
                .dispatch
                .role_overrides
                .insert(channel, serenity::ChannelId::new(8109103));
            drop(lock);
            assert!(
                change.await.is_err(),
                "the current Luna role must reject ultra"
            );
            assert!(
                !shared
                    .settings
                    .read()
                    .await
                    .channel_effort_levels
                    .contains_key(&channel.to_string())
            );
            assert!(!shared.overrides.session_reset_pending.contains(&channel));
        })
    });
}
