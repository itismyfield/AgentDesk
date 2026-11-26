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

// A model that changes under a saved level without the save check, as a role's does, never
// launches with a level it is known to lack; unknown support and the saved value are left alone.
#[test]
fn a_launch_drops_a_saved_level_the_current_model_lacks() {
    fixture(|shared, channel, _| {
        runtime().block_on(async {
            let codex = ProviderKind::Codex;
            let model = |slug: &str| {
                shared
                    .overrides
                    .model_overrides
                    .insert(channel, slug.into());
            };
            model(ASTRA);
            assert!(
                update_channel_effort(shared, TOKEN, channel, &codex, "ultra")
                    .await
                    .unwrap()
            );
            let launched = || channel_effort_level(shared, &codex, channel);
            assert_eq!(launched().await.as_deref(), Some("ultra"));
            model(LUNA);
            assert_eq!(
                launched().await,
                None,
                "luna lists its levels without ultra"
            );
            model("unlisted-model");
            assert_eq!(launched().await.as_deref(), Some("ultra"));
            model(ASTRA);
            assert_eq!(launched().await.as_deref(), Some("ultra"));
            let key = channel.to_string();
            let saved = shared.settings.read().await;
            let level = saved.channel_effort_levels.get(&key);
            assert_eq!(level.map(String::as_str), Some("ultra"));
        })
    });
}

/// `/effort` restarts only a Herdr Codex TUI channel; a Herdr Claude pane takes the level in place.
#[cfg(unix)]
#[test]
fn only_a_herdr_codex_tui_channel_restarts_for_effort() {
    let _hosting = crate::services::provider_hosting::TEST_CONFIG_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut config = crate::config::Config::default();
    for provider in ["claude", "codex"] {
        let hosted = crate::config::ProviderConfig {
            tui_hosting: Some(true),
            ..Default::default()
        };
        config.providers.insert(provider.into(), hosted);
    }
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let default = crate::config::Config::default();
            crate::services::provider_hosting::install_provider_hosting_config(&default);
        }
    }
    let _restore = Restore;
    crate::services::provider_hosting::install_provider_hosting_config(&config);
    let channel = serenity::ChannelId::new(821_740);
    let _hosts =
        crate::config::session_hosts::force_for_test(Some("node-a"), &[(channel.get(), "node-a")]);
    let _on = crate::services::turn_host::force_codex_switch_for_test(Some(true));
    // The Claude Herdr turn is on too, so only the provider keeps a Claude pane off the restart.
    let _claude = crate::services::turn_host::force_switch_for_test(Some(true));
    assert!(herdr_codex(&ProviderKind::Codex, channel, true));
    assert!(
        !herdr_codex(&ProviderKind::Claude, channel, true),
        "a Herdr Claude pane is never restarted for effort"
    );
    let elsewhere = serenity::ChannelId::new(821_741);
    assert!(
        !herdr_codex(&ProviderKind::Codex, elsewhere, false),
        "a local TUI keeps the tmux path"
    );
    let _off = crate::services::turn_host::force_codex_switch_for_test(Some(false));
    assert!(
        !herdr_codex(&ProviderKind::Codex, channel, true),
        "the Codex Herdr switch gates it"
    );
}

/// A Codex thread whose Bound pane runs on its Herdr parent's endpoint takes `/effort` there,
/// though no boot entry names the thread: the capability and the save read one current host.
#[cfg(unix)]
#[test]
fn a_bound_codex_thread_under_a_herdr_parent_takes_effort_pg() {
    use crate::db::dispatched_sessions::hosted_execution::{HostedState, tests};
    use crate::services::session_host::surface_policy::{Relation, force_relation_for_test};
    fixture(|_, _, _| {
        runtime().block_on(async {
            let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
                pool.clone(),
            ));
            let (parent, thread, tmux) = (8_118_490, 8_118_491, "AgentDesk-codex-s2a1-effort");
            // The settings owner check compares the pane's node with this instance.
            let node =
                crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
            let hosts = [(parent, node.as_str())];
            let _hosts = crate::config::session_hosts::force_for_test(Some(&node), &hosts);
            let _thread = force_relation_for_test(thread, Relation::Child { parent });
            let _codex = crate::services::turn_host::force_codex_switch_for_test(Some(true));
            let mut owner = tests::owner(&thread.to_string());
            (owner.provider, owner.logical_key) = ("codex".into(), tmux.into());
            (owner.discord_token_hash, owner.owner_node) =
                (shared.token_hash.clone(), node.clone());
            owner.runtime_root = crate::config::runtime_root().unwrap().display().to_string();
            let mut bound = tests::record(&owner, "nonce-effort", HostedState::Bound);
            let at = bound.location.as_mut().unwrap();
            (at.execution_node, at.endpoint_config_key) =
                (node.clone(), format!("{node}-endpoint"));
            at.socket_addr = "/adk/herdr/agentdesk.sock".into();
            sqlx::query(
                "INSERT INTO sessions (session_key, provider, status, identity_kind,
                                       discord_token_hash, channel_id, hosted_execution)
                 VALUES ($1, 'codex', 'idle', 'discord_channel', $2, $3, $4)",
            )
            .bind(
                crate::services::discord::adk_session::build_namespaced_session_key(
                    &shared.token_hash,
                    &ProviderKind::Codex,
                    tmux,
                ),
            )
            .bind(&shared.token_hash)
            .bind(thread.to_string())
            .bind(tests::wire(&bound))
            .execute(&pool)
            .await
            .unwrap();
            let (codex, channel) = (ProviderKind::Codex, serenity::ChannelId::new(thread));
            shared
                .overrides
                .model_overrides
                .insert(channel, ASTRA.into());

            let level = validate_effort_entry(&shared, &codex, channel, "ultra").await;
            assert_eq!(level, Ok("ultra"));
            let saved = update_channel_effort(&shared, TOKEN, channel, &codex, "ultra").await;
            assert!(
                saved.unwrap(),
                "the level is saved for the thread's Herdr pane"
            );
            pool.close().await;
            db.drop().await;
        })
    });
}

/// An effort restart's end spends the Codex goals reset it reflects, as it spends `/fast` and `/model`.
#[cfg(unix)]
#[tokio::test]
async fn an_effort_restart_spends_the_codex_goals_reset() {
    let shared = make_shared_data_for_tests();
    let channel = serenity::ChannelId::new(821_742);
    shared
        .overrides
        .codex_goals_session_reset_pending
        .insert(channel);
    shared
        .settings
        .write()
        .await
        .channel_codex_goals_reset_pending
        .insert(channel.get().to_string());
    sync_session_reset_pending(&shared, channel);
    assert!(shared.overrides.session_reset_pending.contains(&channel));
    super::super::control::consume_reflected(&shared, &ProviderKind::Codex, channel).await;
    assert!(
        !shared
            .overrides
            .codex_goals_session_reset_pending
            .contains(&channel)
    );
    assert!(
        !shared
            .settings
            .read()
            .await
            .channel_codex_goals_reset_pending
            .contains(&channel.get().to_string())
    );
    assert!(
        !shared.overrides.session_reset_pending.contains(&channel),
        "no later turn repeats the Herdr reset refusal"
    );
}
