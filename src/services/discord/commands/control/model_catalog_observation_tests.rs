use super::*;
use crate::services::discord::commands::model_ui::build_model_picker_option_specs;
use crate::services::discord::model_catalog::with_test_claude_catalog_sources;

const SESSION: &str = "80360000-0000-4000-8000-000000000001";
const MODEL: &str = "claude-fable-5-2";

fn options() -> Vec<crate::services::discord::commands::model_ui::ModelPickerOptionSpec> {
    build_model_picker_option_specs(
        &ProviderKind::Claude,
        None,
        None,
        "default",
        "default",
        None,
    )
}

async fn observe(root: &Path, selector: &str, model: &str, error: bool, session: &str) {
    let records = root.join("records");
    std::fs::create_dir_all(&records).unwrap();
    let record =
        crate::services::claude_tui::hook_bundle::statusline::record_path(&records, SESSION);
    std::fs::write(
        record,
        serde_json::json!({"session_id": SESSION, "written_at": 2.0}).to_string(),
    )
    .unwrap();
    let old = format!(
        "{}\n",
        serde_json::json!({"type":"assistant", "sessionId":SESSION,
        "message":{"model":"claude-old"}})
    );
    let foreign = serde_json::json!({"type":"assistant", "sessionId":"other-session",
        "message":{"model":"claude-foreign"}});
    let answer = serde_json::json!({"type":"assistant", "sessionId":session,
        "isApiErrorMessage":error, "message":{"model":model}});
    let transcript = root.join("transcript.jsonl");
    std::fs::write(&transcript, format!("{old}{foreign}\n{answer}\n")).unwrap();
    let confirm = Confirm {
        session: SESSION.to_string(),
        requested_fast: None,
        at: 1.0,
        model: Some(ModelCheck {
            requested: Some(selector.to_string()),
            effort: None,
            transcript: Some(transcript),
            offset: old.len() as u64,
        }),
    };
    let waits = Waits {
        record: Duration::ZERO,
        settle: Duration::ZERO,
        poll: Duration::ZERO,
        answer: Duration::ZERO,
    };
    confirm_notice(
        &confirm,
        &records,
        || async { Some(SESSION.to_string()) },
        &waits,
    )
    .await;
}

#[tokio::test]
async fn model_catalog_observation_reaches_actual_model_picker() {
    let root = crate::config::TestRuntimeRootGuard::new();
    observe(root.path(), "fable", MODEL, false, SESSION).await;
    let entries = options();
    let alias = entries.iter().find(|entry| entry.value == "fable").unwrap();
    assert_eq!(alias.label, format!("Fable · {MODEL}"));
    assert!(entries.iter().any(|entry| entry.value == MODEL));
    assert!(
        !entries
            .iter()
            .any(|entry| entry.value == "claude-old" || entry.value == "claude-foreign")
    );
    assert!(entries.len() <= 25);
    assert!(
        entries
            .iter()
            .all(|entry| entry.label.chars().count() <= 100
                && crate::services::discord::model_catalog::is_valid_discord_select_option_value(
                    &entry.value
                ))
    );
    observe(root.path(), "fable", "claude-fable-next", false, SESSION).await;
    assert_eq!(
        options()
            .iter()
            .find(|entry| entry.value == "fable")
            .unwrap()
            .label,
        "Fable · claude-fable-next"
    );
}

#[test]
fn model_catalog_observation_missing_and_corrupt_files_keep_unversioned_aliases() {
    let root = crate::config::TestRuntimeRootGuard::new();
    let path = root.path().join("runtime/claude_model_observations.json");
    for raw in [None, Some("{"), Some("[]")] {
        if let Some(raw) = raw {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, raw).unwrap();
        }
        let entries = options();
        for (selector, label) in [
            ("fable", "Fable"),
            ("opus", "Opus"),
            ("sonnet", "Sonnet"),
            ("haiku", "Haiku"),
        ] {
            assert_eq!(
                entries
                    .iter()
                    .find(|entry| entry.value == selector)
                    .unwrap()
                    .label,
                label
            );
        }
    }
}

#[tokio::test]
async fn model_catalog_observation_excludes_synthetic_errors_nonaliases_and_foreign_sessions() {
    let root = crate::config::TestRuntimeRootGuard::new();
    let path = root.path().join("runtime/claude_model_observations.json");
    for (selector, model, error, session) in [
        ("fable", "<synthetic>", false, SESSION),
        ("fable", MODEL, true, SESSION),
        (MODEL, MODEL, false, SESSION),
        ("opusplan", MODEL, false, SESSION),
        ("fable", MODEL, false, "other-session"),
    ] {
        observe(root.path(), selector, model, error, session).await;
        assert!(
            !path.exists(),
            "must exclude {selector} / {model} / error={error} / {session}"
        );
    }
}

fn gateway_file(path: &Path, models: serde_json::Value, fresh: bool) {
    let fetched = if fresh {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    } else {
        1
    };
    std::fs::write(
        path,
        serde_json::json!({"fetchedAt":fetched,"models":models}).to_string(),
    )
    .unwrap();
}

async fn observed_gateway_picker(shared: bool) {
    let root = crate::config::TestRuntimeRootGuard::new();
    observe(root.path(), "fable", MODEL, false, SESSION).await;
    let path = root.path().join("gateway.json");
    let mut models = (0..16)
        .map(|index| {
            serde_json::json!({"id":format!("gateway-{index}"),
        "display_name":format!("Gateway {index}")})
        })
        .collect::<Vec<_>>();
    if shared {
        models.push(
            serde_json::json!({"id":MODEL.to_ascii_uppercase(),"display_name":"Gateway winner"}),
        );
    }
    gateway_file(&path, models.into(), true);
    with_test_claude_catalog_sources(
        path,
        root.path().join("runtime/claude_model_observations.json"),
        || {
            let entries = options();
            assert_eq!(
                entries
                    .iter()
                    .find(|entry| entry.value == "fable")
                    .unwrap()
                    .label,
                format!("Fable · {MODEL}")
            );
            let full = entries
                .iter()
                .filter(|entry| entry.value.eq_ignore_ascii_case(MODEL))
                .collect::<Vec<_>>();
            assert_eq!(
                full.len(),
                1,
                "observed ID must survive the picker cap without duplicates"
            );
            assert_eq!(full[0].label, if shared { "Gateway winner" } else { MODEL });
            assert_eq!(entries[8].value.to_ascii_lowercase(), MODEL);
            assert!(entries.len() <= 25);
        },
    );
}

#[tokio::test]
async fn model_catalog_observation_gateway_limit_keeps_new_full_id() {
    observed_gateway_picker(false).await;
}

#[tokio::test]
async fn model_catalog_observation_gateway_shared_id_keeps_winning_metadata() {
    observed_gateway_picker(true).await;
}

#[test]
fn model_catalog_gateway_resolver_preserves_last_good_until_fresh_replacement() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("gateway.json");
    gateway_file(&path, serde_json::json!([{"id":"gateway-old"}]), true);
    with_test_claude_catalog_sources(path.clone(), root.path().join("observations.json"), || {
        let has = |id: &str| options().iter().any(|entry| entry.value == id);
        assert!(has("gateway-old"));
        gateway_file(&path, serde_json::json!([{"id":"gateway-stale"}]), false);
        assert!(has("gateway-old"));
        assert!(!has("gateway-stale"));
        std::fs::write(&path, "{").unwrap();
        assert!(has("gateway-old"));
        std::fs::remove_file(&path).unwrap();
        assert!(has("gateway-old"));
        gateway_file(&path, serde_json::json!([{"id":"gateway-new"}]), true);
        assert!(has("gateway-new"));
        assert!(!has("gateway-old"));
    });
}

#[cfg(unix)]
#[test]
fn model_catalog_new_model_save_cancels_previous_observation_after_failed_launch() {
    let root = crate::config::TestRuntimeRootGuard::new();
    let hosting = crate::services::provider_hosting::TEST_CONFIG_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let config_path = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &root.path().join("missing.yaml"),
    );
    let hosts = crate::config::session_hosts::force_for_test(None, &[]);
    struct RestoreHosting;
    impl Drop for RestoreHosting {
        fn drop(&mut self) {
            crate::services::provider_hosting::install_provider_hosting_config(
                &crate::config::Config::default(),
            );
        }
    }
    let restore = RestoreHosting;
    let mut config = crate::config::Config::default();
    config.providers.insert(
        "claude".into(),
        crate::config::ProviderConfig {
            tui_hosting: Some(true),
            ..Default::default()
        },
    );
    crate::services::provider_hosting::install_provider_hosting_config(&config);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = serenity::ChannelId::new(8_036_003);
        let provider = ProviderKind::Claude;
        let name = provider.build_tmux_session_name("model-catalog-recovery");
        let tmux =
            crate::services::provider_teardown::tests::test_support::FakeTmux::install(&name);
        let discord = crate::services::discord::admin_host_guard::tests::Recorder::start().await;
        shared.core.lock().await.sessions.insert(
            channel,
            crate::services::discord::DiscordSession {
                session_id: Some(SESSION.to_string()),
                memento_context_loaded: false,
                memento_reflected: false,
                current_path: Some(root.path().display().to_string()),
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                channel_id: Some(channel.get()),
                channel_name: Some("model-catalog-recovery".into()),
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: shared.restart.current_generation,
            },
        );
        shared
            .overrides
            .model_overrides
            .insert(channel, "fable".into());
        shared
            .settings
            .write()
            .await
            .channel_model_overrides
            .insert(channel.get().to_string(), "fable".into());
        let transcript = root.path().join("recovery.jsonl");
        std::fs::write(&transcript, "").unwrap();
        spawn_with(
            discord.http.clone(),
            shared.clone(),
            channel,
            Confirm {
                session: SESSION.to_string(),
                requested_fast: None,
                at: f64::MAX,
                model: Some(ModelCheck {
                    requested: Some("fable".into()),
                    effort: None,
                    transcript: Some(transcript.clone()),
                    offset: 0,
                }),
            },
            Waits {
                record: Duration::ZERO,
                settle: Duration::ZERO,
                poll: Duration::from_millis(1),
                answer: Duration::from_secs(900),
            },
        );
        tokio::task::yield_now().await;
        assert!(
            running(channel),
            "A waits before an assistant record exists"
        );
        tmux.mark_missing();
        assert!(
            crate::services::discord::commands::config::update_channel_model_override(
                &shared,
                "test-token",
                channel,
                &provider,
                Some("sonnet".into())
            )
            .await
            .unwrap()
        );
        assert!(
            crate::services::discord::commands::config::claude_local_tui_relaunch_applies(
                &provider, channel
            )
        );
        assert!(
            shared
                .settings
                .read()
                .await
                .channel_relaunch_pending
                .contains(&channel.get().to_string())
        );
        assert!(matches!(
            super::super::relaunch::pane_state(&shared, &provider, channel).await,
            super::super::relaunch::PaneState::None
        ));
        assert!(
            super::super::reset_provider_session_if_pending(
                &discord.http,
                &shared,
                &provider,
                channel,
                channel
            )
            .await
            .is_none()
        );
        assert!(
            !shared
                .settings
                .read()
                .await
                .channel_relaunch_pending
                .contains(&channel.get().to_string())
        );
        assert_eq!(
            shared.core.lock().await.sessions[&channel]
                .session_id
                .as_deref(),
            Some(SESSION)
        );
        std::fs::write(
            &transcript,
            format!(
                "{}\n",
                serde_json::json!({"type":"assistant", "sessionId":SESSION,
            "message":{"model":"claude-sonnet-next"}})
            ),
        )
        .unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        let observations = root.path().join("runtime/claude_model_observations.json");
        let saved = std::fs::read(&observations)
            .ok()
            .map(|raw| serde_json::from_slice::<serde_json::Value>(&raw).unwrap());
        abort(channel);
        assert!(
            saved
                .as_ref()
                .is_none_or(|value| value.get("fable").is_none()),
            "B response must not be recorded for A: {saved:?}"
        );
    });
    drop(restore);
    drop(hosts);
    drop(config_path);
    drop(hosting);
    drop(root);
}
