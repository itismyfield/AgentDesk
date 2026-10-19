#![cfg(unix)]

use super::*;
use crate::config::{Config, ProviderConfig, TestEnvVarGuard};
use crate::services::discord::inflight::read_only_owner_snapshot::save_actual_episode_for_tests;
use crate::services::provider_hosting::{TEST_CONFIG_LOCK, install_provider_hosting_config};
use crate::services::provider_teardown::tests::test_support::FakeTmux;
use crate::services::turn_orchestrator::ChannelMailboxRegistry;
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use std::sync::atomic::Ordering;

const CHANNEL: u64 = 7_163_201;
const TMUX: &str = "b1-absent-inflight";

fn with_scope(run: impl FnOnce(&std::path::Path)) {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir().unwrap();
    let _root =
        TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path());
    let _hosting = TEST_CONFIG_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
    let mut config = Config::default();
    config.providers.insert(
        "codex".into(),
        ProviderConfig {
            tui_hosting: Some(true),
            ..Default::default()
        },
    );
    install_provider_hosting_config(&config);
    let fake = FakeTmux::install(TMUX);
    fake.enable_fixture_launch();
    crate::services::platform::tmux::invalidate_availability_cache();
    std::fs::remove_dir(root.path().join("runtime/sessions")).unwrap();
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            install_provider_hosting_config(&Config::default());
            crate::services::platform::tmux::invalidate_availability_cache();
        }
    }
    let _reset = Reset;
    run(root.path());
}

#[test]
fn readiness_observation_refuses_new_rows_and_read_errors_without_repair() {
    with_scope(|root| {
        let observed = readiness(CHANNEL, TMUX).expect("confirmed missing row");
        assert!(observed.recheck(CHANNEL, TMUX));
        assert!(!observed.matches_token(&Arc::new(CancelToken::new())));
        let token = Arc::new(CancelToken::new());
        let path = save_actual_episode_for_tests(CHANNEL, TMUX, 30, &token);
        assert!(!observed.recheck(CHANNEL, TMUX));
        assert!(readiness(CHANNEL, TMUX).is_none());
        std::fs::write(&path, b"{broken row").unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(!observed.recheck(CHANNEL, TMUX));
        assert!(readiness(CHANNEL, TMUX).is_none());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(!observed.recheck(CHANNEL, TMUX));
        assert!(readiness(CHANNEL, TMUX).is_none());
        assert!(path.is_dir(), "read error must not repair the row path");
        assert!(!root.join("runtime/sessions").exists());
    });
}

#[test]
fn exact_self_observation_rechecks_row_nonce_and_cancellation_without_actor_wait() {
    with_scope(|_| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let registry = ChannelMailboxRegistry::default();
        let token = Arc::new(CancelToken::new());
        let path = save_actual_episode_for_tests(CHANNEL, TMUX, 30, &token);
        let observed = runtime.block_on(async {
            assert!(
                registry
                    .handle(ChannelId::new(CHANNEL))
                    .try_start_turn(Arc::clone(&token), UserId::new(8), MessageId::new(30))
                    .await
            );
            observe_turn(CHANNEL, TMUX, &token)
                .await
                .expect("exact active episode")
        });
        assert!(observed.matches_token(&token));
        assert!(observed.recheck(CHANNEL, TMUX));
        assert!(!observed.recheck(CHANNEL + 1, TMUX));
        assert!(!observed.recheck(CHANNEL, "another-target"));
        let duplicate = Arc::new(CancelToken::from_persisted_turn_nonce(
            token.turn_nonce().map(str::to_owned),
        ));
        assert!(!observed.matches_token(&duplicate));
        let original = std::fs::read(&path).unwrap();
        for (field, value) in [
            ("turn_nonce", serde_json::json!("replacement")),
            ("user_msg_id", serde_json::json!(31)),
            ("request_owner_user_id", serde_json::json!(9)),
            ("tmux_session_name", serde_json::json!("another-target")),
        ] {
            let mut row: serde_json::Value = serde_json::from_slice(&original).unwrap();
            row[field] = value;
            let bytes = serde_json::to_vec(&row).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            assert!(!observed.recheck(CHANNEL, TMUX), "accepted changed {field}");
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        std::fs::write(&path, original).unwrap();
        assert!(observed.recheck(CHANNEL, TMUX));
        token.cancelled.store(true, Ordering::Release);
        assert!(!observed.recheck(CHANNEL, TMUX));
    });
}

#[test]
fn full_snapshot_refuses_foreign_arc_actor_and_unreadable_rows() {
    with_scope(|_| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let registry = ChannelMailboxRegistry::default();
        let token = Arc::new(CancelToken::new());
        let path = save_actual_episode_for_tests(CHANNEL, TMUX, 30, &token);
        let original = std::fs::read(&path).unwrap();
        runtime.block_on(async {
            assert!(
                registry
                    .handle(ChannelId::new(CHANNEL))
                    .try_start_turn(Arc::clone(&token), UserId::new(8), MessageId::new(30))
                    .await
            );
            let duplicate = Arc::new(CancelToken::from_persisted_turn_nonce(
                token.turn_nonce().map(str::to_owned),
            ));
            assert!(observe_turn(CHANNEL, TMUX, &duplicate).await.is_none());
            let observed = observe_turn(CHANNEL, TMUX, &token).await.unwrap();
            for (field, value) in [
                ("turn_nonce", serde_json::json!("foreign-episode")),
                ("user_msg_id", serde_json::json!(31)),
                ("request_owner_user_id", serde_json::json!(9)),
            ] {
                let mut row: serde_json::Value = serde_json::from_slice(&original).unwrap();
                row[field] = value;
                let bytes = serde_json::to_vec(&row).unwrap();
                std::fs::write(&path, &bytes).unwrap();
                assert!(
                    observe_turn(CHANNEL, TMUX, &token).await.is_none(),
                    "full snapshot accepted changed {field}"
                );
                assert_eq!(std::fs::read(&path).unwrap(), bytes);
            }
            std::fs::write(&path, &original).unwrap();
            let successor = ChannelMailboxRegistry::default();
            assert!(
                successor
                    .handle(ChannelId::new(CHANNEL))
                    .try_start_turn(Arc::clone(&duplicate), UserId::new(8), MessageId::new(30))
                    .await
            );
            assert!(
                !registry
                    .handle(ChannelId::new(CHANNEL))
                    .same_actor(&successor.handle(ChannelId::new(CHANNEL)))
            );
            assert!(observe_turn(CHANNEL, TMUX, &token).await.is_none());
            assert!(observe_turn(CHANNEL, TMUX, &duplicate).await.is_some());
            assert!(!observed.matches_token(&duplicate));
            std::fs::write(&path, b"{unreadable row").unwrap();
            let bytes = std::fs::read(&path).unwrap();
            assert!(observe_turn(CHANNEL, TMUX, &duplicate).await.is_none());
            assert!(!observed.recheck(CHANNEL, TMUX));
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
            assert!(observe_turn(CHANNEL, TMUX, &duplicate).await.is_none());
            assert!(!observed.recheck(CHANNEL, TMUX));
            assert!(path.is_dir());
        });
    });
}

#[test]
fn observations_refuse_legacy_driver_and_missing_target_identity() {
    with_scope(|_| {
        assert!(readiness(0, TMUX).is_none());
        assert!(readiness(CHANNEL, " ").is_none());
        let observed = readiness(CHANNEL, TMUX).unwrap();
        install_provider_hosting_config(&Config::default());
        assert!(readiness(CHANNEL, TMUX).is_none());
        assert!(!observed.recheck(CHANNEL, TMUX));
    });
}

#[test]
fn observations_refuse_process_fallback_when_tmux_becomes_unavailable() {
    with_scope(|_| {
        let observed = readiness(CHANNEL, TMUX).expect("available direct TUI scope");
        assert!(observed.recheck(CHANNEL, TMUX));
        let unavailable = FakeTmux::install(TMUX);
        crate::services::platform::tmux::invalidate_availability_cache();
        assert!(!crate::services::claude::is_tmux_available());
        assert!(readiness(CHANNEL, TMUX).is_none());
        assert!(!observed.recheck(CHANNEL, TMUX));
        unavailable.enable_fixture_launch();
        crate::services::platform::tmux::invalidate_availability_cache();
        assert!(crate::services::claude::is_tmux_available());
        assert!(readiness(CHANNEL, TMUX).is_some());
        assert!(observed.recheck(CHANNEL, TMUX));
    });
}
